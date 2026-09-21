//! OCI Distribution v2 transport over a caller-owned Kubernetes API tunnel.
//! No Docker daemon, registry credentials, redirect, or environment proxy.
use sha2::{Digest, Sha256};
use std::error::Error;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

pub fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

/// Reject paths, userinfo, tags, query strings, and shell-shaped repository input.
pub fn validate_repository(repository: &str) -> Result<()> {
    if repository.is_empty()
        || repository.len() > 255
        || !repository.split('/').all(|part| {
            !part.is_empty()
                && part.as_bytes()[0].is_ascii_alphanumeric()
                && part.as_bytes()[part.len() - 1].is_ascii_alphanumeric()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
                && part != "."
                && part != ".."
        })
    {
        return Err("invalid OCI repository path".into());
    }
    Ok(())
}

/// Fully qualified image repository, including an optional registry port.
pub fn validate_image_name(image: &str) -> Result<()> {
    let (authority, repository) = image
        .split_once('/')
        .ok_or("image must include registry and repository")?;
    let (host, port) = authority
        .split_once(':')
        .map(|(h, p)| (h, Some(p)))
        .unwrap_or((authority, None));
    if !host.contains('.')
        || host.len() > 253
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
        || port.is_some_and(|p| p.parse::<u16>().ok().filter(|p| *p != 0).is_none())
    {
        return Err("invalid image registry authority".into());
    }
    validate_repository(repository)
}

/// An OCI blob whose identity is computed from the exact bytes being uploaded.
pub struct Blob {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

impl Blob {
    pub fn descriptor(&self) -> serde_json::Value {
        serde_json::json!({
            "mediaType": self.media_type,
            "digest": digest(&self.bytes),
            "size": self.bytes.len()
        })
    }
}

/// OCI image manifest plus its complete direct blob closure.
/// Image indexes are deliberately rejected here; callers must publish children first.
pub struct Image {
    pub manifest: Vec<u8>,
    pub blobs: Vec<Blob>,
}

impl Image {
    pub fn validate(&self) -> Result<String> {
        let manifest: serde_json::Value =
            serde_json::from_slice(&self.manifest).map_err(|_| "invalid OCI manifest JSON")?;
        if manifest["schemaVersion"] != 2 || manifest["mediaType"] != MANIFEST_MEDIA_TYPE {
            return Err("expected an OCI image manifest (not an index or Docker schema1)".into());
        }
        let layers = manifest["layers"].as_array().ok_or("missing OCI layers")?;
        let mut descriptors = vec![&manifest["config"]];
        descriptors.extend(layers);
        let mut blobs = std::collections::HashMap::new();
        for blob in &self.blobs {
            let descriptor = blob.descriptor();
            let hash = descriptor["digest"].as_str().unwrap().to_owned();
            if blobs.insert(hash, descriptor).is_some() {
                return Err("duplicate blob identity".into());
            }
        }
        let mut referenced = std::collections::HashSet::new();
        for descriptor in descriptors {
            let expected = descriptor["digest"].as_str().ok_or("missing blob digest")?;
            if !valid_digest(expected) {
                return Err("invalid blob digest".into());
            }
            let actual = blobs.get(expected).ok_or("incomplete OCI blob closure")?;
            if descriptor != actual {
                return Err("OCI descriptor media type, size, or fields do not match blob".into());
            }
            referenced.insert(expected);
        }
        if referenced.len() != blobs.len() {
            return Err("image includes unreferenced blob content".into());
        }
        Ok(digest(&self.manifest))
    }
}

/// Always loopback. Constructing this client cannot select a public write host.
pub struct RegistryClient {
    base: String,
    agent: ureq::Agent,
}

fn content_digest(headers: &ureq::http::HeaderMap) -> Option<&str> {
    headers
        .get("Docker-Content-Digest")
        .and_then(|value| value.to_str().ok())
}

impl RegistryClient {
    pub fn loopback(port: u16, timeout: Duration) -> Result<Self> {
        if port == 0 || timeout.is_zero() {
            return Err("registry port and timeout must be nonzero".into());
        }
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .proxy(None)
            .build();
        Ok(Self {
            base: format!("http://{}", SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
            agent: ureq::Agent::new_with_config(config),
        })
    }

    pub fn ready(&self) -> Result<()> {
        let response = self
            .agent
            .get(&format!("{}/v2/", self.base))
            .call()
            .map_err(|_| {
                "registry API unavailable through tunnel; retry without changing the pin"
            })?;
        if response.status() != 200 {
            return Err("registry API is not ready".into());
        }
        Ok(())
    }

    fn exists(&self, repository: &str, kind: &str, expected: &str) -> Result<bool> {
        let url = format!("{}/v2/{repository}/{kind}/{expected}", self.base);
        match self
            .agent
            .head(&url)
            .header("Accept", MANIFEST_MEDIA_TYPE)
            .call()
        {
            Ok(response) if response.status() == 200 => {
                if content_digest(response.headers()) != Some(expected) {
                    return Err("registry HEAD returned a different digest".into());
                }
                Ok(true)
            }
            Err(ureq::Error::StatusCode(404)) => Ok(false),
            _ => Err("registry HEAD failed; retry without changing the pin".into()),
        }
    }

    /// Upload and verify immutable content. Returns false when already present.
    /// This function never changes a Kubernetes resource or Git package pin.
    pub fn publish(&self, repository: &str, image: &Image) -> Result<bool> {
        validate_repository(repository)?;
        let expected = image.validate()?;
        if self.exists(repository, "manifests", &expected)? {
            self.verify_manifest(repository, &image.manifest)?;
            return Ok(false);
        }
        for blob in &image.blobs {
            let expected_blob = digest(&blob.bytes);
            if self.exists(repository, "blobs", &expected_blob)? {
                continue;
            }
            let response = self
                .agent
                .post(&format!("{}/v2/{repository}/blobs/uploads/", self.base))
                .header("Content-Length", "0")
                .send(&[] as &[u8])
                .map_err(|_| "registry upload start failed; retry without changing the pin")?;
            if response.status() != 202 {
                return Err("registry rejected blob upload start".into());
            }
            let location = response
                .headers()
                .get("Location")
                .and_then(|value| value.to_str().ok())
                .ok_or("upload Location missing")?;
            let location = upload_location(repository, location, &expected_blob)?;
            let response = self
                .agent
                .put(&format!("{}{location}", self.base))
                .header("Content-Type", "application/octet-stream")
                .send(blob.bytes.as_slice())
                .map_err(|_| "registry blob upload interrupted; retry without changing the pin")?;
            if response.status() != 201
                || content_digest(response.headers()) != Some(&expected_blob)
            {
                return Err("registry did not confirm uploaded blob digest".into());
            }
            if !self.exists(repository, "blobs", &expected_blob)? {
                return Err("uploaded blob is not readable by digest".into());
            }
        }
        let response = self
            .agent
            .put(&format!(
                "{}/v2/{repository}/manifests/{expected}",
                self.base
            ))
            .header("Content-Type", MANIFEST_MEDIA_TYPE)
            .send(image.manifest.as_slice())
            .map_err(|_| "registry manifest upload failed; retry without changing the pin")?;
        if response.status() != 201 || content_digest(response.headers()) != Some(&expected) {
            return Err("registry did not confirm uploaded manifest digest".into());
        }
        self.verify_manifest(repository, &image.manifest)?;
        Ok(true)
    }

    fn verify_manifest(&self, repository: &str, manifest: &[u8]) -> Result<()> {
        let expected = digest(manifest);
        let mut response = self
            .agent
            .get(&format!(
                "{}/v2/{repository}/manifests/{expected}",
                self.base
            ))
            .header("Accept", MANIFEST_MEDIA_TYPE)
            .call()
            .map_err(|_| "uploaded manifest cannot be read back; do not activate")?;
        if response.status() != 200 || content_digest(response.headers()) != Some(&expected) {
            return Err("manifest readback returned a different digest".into());
        }
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(manifest.len() as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "manifest readback interrupted; do not activate")?;
        if bytes != manifest {
            return Err("manifest readback bytes do not match upload; do not activate".into());
        }
        Ok(())
    }
}

/// Distribution is configured with relativeurls. Never follow an upload URL to
/// another host, repository, route, or an existing query's conflicting digest.
fn upload_location(repository: &str, location: &str, expected: &str) -> Result<String> {
    let prefix = format!("/v2/{repository}/blobs/uploads/");
    let remainder = location
        .strip_prefix(&prefix)
        .ok_or("unsafe registry upload Location")?;
    let (id, query) = remainder.split_once('?').unwrap_or((remainder, ""));
    let state = query.strip_prefix("_state=");
    // Distribution URL-encodes base64 padding. Only the opaque state value may
    // be encoded; accepting arbitrary parameter names permits digest override.
    let safe_query = query.is_empty()
        || state.is_some_and(|value| {
            let decoded = value.replace("%3D", "=").replace("%3d", "=");
            let unpadded = decoded.trim_end_matches('=');
            !unpadded.is_empty()
                && decoded.len() - unpadded.len() <= 2
                && unpadded
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        });
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') || !safe_query {
        return Err("unsafe registry upload Location".into());
    }
    let separator = if location.contains('?') { '&' } else { '?' };
    Ok(format!("{location}{separator}digest={expected}"))
}

/// Stream-safe hash helper for archive preparation; does not load a large file.
pub fn sha256_reader(mut reader: impl Read) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let size = reader.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.write_all(&buffer[..size])?;
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_locations_cannot_escape_tunnel_or_repository() {
        let expected = digest(b"data");
        assert_eq!(
            upload_location(
                "org/pkg",
                "/v2/org/pkg/blobs/uploads/abc-123?_state=abc_DEF-12",
                &expected
            )
            .unwrap(),
            format!("/v2/org/pkg/blobs/uploads/abc-123?_state=abc_DEF-12&digest={expected}")
        );
        assert!(upload_location(
            "org/pkg",
            "/v2/org/pkg/blobs/uploads/abc?_state=abc%3D%3D",
            &expected
        )
        .is_ok());
        for location in [
            "https://public.example/upload",
            "//public.example/upload",
            "/v2/other/pkg/blobs/uploads/abc",
            "/v2/org/pkg/blobs/uploads/../admin",
            "/v2/org/pkg/blobs/uploads/abc?digest=other",
            "/v2/org/pkg/blobs/uploads/abc?%64igest=other",
            "/v2/org/pkg/blobs/uploads/abc?x=bad\n",
            "/v2/org/pkg/blobs/uploads/abc#fragment",
        ] {
            assert!(
                upload_location("org/pkg", location, &expected).is_err(),
                "{location}"
            );
        }
    }

    #[test]
    fn validates_blob_closure_before_network() {
        let blob = Blob {
            bytes: b"{}".to_vec(),
            media_type: "application/vnd.oci.image.config.v1+json".into(),
        };
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2, "mediaType": MANIFEST_MEDIA_TYPE,
            "config": blob.descriptor(), "layers": []
        }))
        .unwrap();
        let mut image = Image {
            manifest,
            blobs: vec![blob],
        };
        assert!(image.validate().is_ok());
        image.blobs[0].bytes.push(b' ');
        assert!(image.validate().is_err());
    }

    #[test]
    fn repository_validation_and_streamed_hash() {
        for repo in ["org/pkg", "org/nested/pkg_v2", "pkg.name"] {
            assert!(validate_repository(repo).is_ok());
        }
        for repo in [
            "",
            "../pkg",
            "org/../pkg",
            "/org/pkg",
            "org/Pkg",
            "pkg:tag",
            "x?token=secret",
            "x@y",
            "org//pkg",
        ] {
            assert!(validate_repository(repo).is_err());
        }
        assert_eq!(sha256_reader(&b"content"[..]).unwrap(), digest(b"content"));
    }
}
