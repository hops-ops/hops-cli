//! Read the current builder's Docker-save .uppkg output without docker load.
//! No archive entry is extracted onto the workstation filesystem.
use super::registry::{validate_image_name, Blob, Image, MANIFEST_MEDIA_TYPE};
use flate2::read::GzDecoder;
use serde::Deserialize;
use std::collections::HashMap;
use std::error::Error;
use std::io::{BufRead, BufReader, Read};
use tar::Archive;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub struct BuiltArchive {
    pub source: String,
    pub image: Image,
    /// Package-only artifacts need not carry a runtime architecture.
    pub platform: Option<Platform>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
}

impl Platform {
    /// Only use for runnable Function/controller artifacts, not package metadata.
    pub fn require_compatible(&self, node_architectures: &[String]) -> Result<()> {
        if self.os != "linux"
            || node_architectures.is_empty()
            || node_architectures
                .iter()
                .any(|node| node != &self.architecture)
        {
            return Err("runtime artifact is not compatible with every eligible target-node architecture; build a matching or multi-platform artifact".into());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct DockerManifest {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Vec<String>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

/// Memory-bounded initial adapter. Large runtime archives are rejected explicitly
/// rather than risking unbounded memory use; disk-streamed blobs remain separate work.
pub fn read_uppkg(input: impl Read, max_bytes: u64) -> Result<BuiltArchive> {
    if max_bytes == 0 {
        return Err("archive byte limit must be nonzero".into());
    }
    let mut input = BufReader::new(input);
    let magic = input.fill_buf()?;
    if magic.starts_with(&[0x1f, 0x8b]) {
        read_tar(GzDecoder::new(input), max_bytes)
    } else {
        read_tar(input, max_bytes)
    }
}

fn normalized_path(path: &str) -> Result<String> {
    let path = path.strip_prefix("./").unwrap_or(path);
    if path.is_empty()
        || path.contains('\\')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path.chars().any(char::is_control)
    {
        return Err("unsafe path in package archive".into());
    }
    Ok(path.into())
}

fn read_tar(input: impl Read, max_bytes: u64) -> Result<BuiltArchive> {
    // Bound even GNU/PAX metadata that tar processes before yielding entries.
    let mut archive = Archive::new(input.take(max_bytes));
    let mut files = HashMap::new();
    let mut total = 0u64;
    let mut entries = 0;
    for entry in archive.entries().map_err(|_| "invalid package archive")? {
        let mut entry = entry.map_err(|_| "invalid package archive entry")?;
        entries += 1;
        if entries > 4096 {
            return Err("package archive has too many entries".into());
        }
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err("package archive links and special files are forbidden".into());
        }
        let path = entry.path().map_err(|_| "invalid archive path")?;
        let path = normalized_path(path.to_str().ok_or("non-UTF8 archive path")?)?;
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or("package archive exceeds byte limit")?;
        if total > max_bytes {
            return Err("package archive exceeds byte limit".into());
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|_| "truncated package archive entry")?;
        if bytes.len() as u64 != size || files.insert(path, bytes).is_some() {
            return Err("truncated or duplicate package archive entry".into());
        }
    }
    let manifests: Vec<DockerManifest> = serde_json::from_slice(
        files
            .get("manifest.json")
            .ok_or("Docker-save manifest.json missing")?,
    )
    .map_err(|_| "invalid Docker-save manifest")?;
    if manifests.len() != 1 || manifests[0].repo_tags.len() != 1 {
        return Err("package archive must identify exactly one image and source tag".into());
    }
    let manifest = manifests.into_iter().next().unwrap();
    let source = manifest.repo_tags.into_iter().next().unwrap();
    let (repository, tag) = source
        .rsplit_once(':')
        .ok_or("builder image is missing its source tag")?;
    validate_image_name(repository)?;
    if tag.is_empty()
        || tag.len() > 128
        || !tag
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return Err("invalid source tag in package archive".into());
    }
    let config_bytes = files
        .remove(&normalized_path(&manifest.config)?)
        .ok_or("package image config missing")?;
    let config: serde_json::Value =
        serde_json::from_slice(&config_bytes).map_err(|_| "invalid image config JSON")?;
    let platform = match (config["os"].as_str(), config["architecture"].as_str()) {
        (Some(os), Some(architecture)) => Some(Platform {
            os: os.into(),
            architecture: architecture.into(),
        }),
        (None, None) => None,
        _ => return Err("incomplete runtime platform in image config".into()),
    };
    let config = Blob {
        bytes: config_bytes,
        media_type: "application/vnd.oci.image.config.v1+json".into(),
    };
    let config_descriptor = config.descriptor();
    let mut blobs = vec![config];
    let mut layers = Vec::new();
    let mut used: HashMap<String, serde_json::Value> = HashMap::new();
    let mut blob_digests =
        std::collections::HashSet::from([config_descriptor["digest"].as_str().unwrap().to_owned()]);
    for path in manifest.layers {
        let path = normalized_path(&path)?;
        if let Some(descriptor) = used.get(&path) {
            layers.push(descriptor.clone());
            continue;
        }
        let bytes = files.remove(&path).ok_or("package layer missing")?;
        let media_type = if bytes.starts_with(&[0x1f, 0x8b]) {
            "application/vnd.oci.image.layer.v1.tar+gzip"
        } else {
            "application/vnd.oci.image.layer.v1.tar"
        };
        let blob = Blob {
            bytes,
            media_type: media_type.into(),
        };
        let descriptor = blob.descriptor();
        used.insert(path, descriptor.clone());
        // OCI permits identical layers at different archive paths.
        if blob_digests.insert(descriptor["digest"].as_str().unwrap().to_owned()) {
            blobs.push(blob);
        }
        layers.push(descriptor);
    }
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2, "mediaType": MANIFEST_MEDIA_TYPE,
        "config": config_descriptor, "layers": layers
    }))?;
    let image = Image { manifest, blobs };
    image.validate()?;
    Ok(BuiltArchive {
        source,
        image,
        platform,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        builder.into_inner().unwrap()
    }

    const MANIFEST: &[u8] = br#"[{"Config":"config.json","RepoTags":["ghcr.io/hops-ops/example:configuration"],"Layers":["layer.tar"]}]"#;
    const CONFIG: &[u8] = br#"{"architecture":"arm64","os":"linux","config":{"Labels":{"io.crossplane.xpkg":"true"}}}"#;

    fn fixture() -> Vec<u8> {
        archive(&[
            ("manifest.json", MANIFEST),
            ("config.json", CONFIG),
            ("layer.tar", b"fixture-layer"),
        ])
    }

    #[test]
    fn plain_and_gzip_builder_archives_have_identical_immutable_images() {
        let bytes = fixture();
        let plain = read_uppkg(bytes.as_slice(), 1024 * 1024).unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&bytes).unwrap();
        let compressed = read_uppkg(gzip.finish().unwrap().as_slice(), 1024 * 1024).unwrap();
        assert_eq!(plain.source, "ghcr.io/hops-ops/example:configuration");
        assert_eq!(plain.image.manifest, compressed.image.manifest);
        assert_eq!(
            plain.image.validate().unwrap(),
            compressed.image.validate().unwrap()
        );
        assert_eq!(
            plain.platform.unwrap(),
            Platform {
                os: "linux".into(),
                architecture: "arm64".into()
            }
        );
        assert_eq!(plain.image.blobs[0].bytes, CONFIG);
    }

    #[test]
    fn rejects_limits_duplicates_missing_layers_and_traversal_without_extracting() {
        assert!(read_uppkg(fixture().as_slice(), 10).is_err());
        let duplicate = archive(&[("manifest.json", MANIFEST), ("manifest.json", MANIFEST)]);
        assert!(read_uppkg(duplicate.as_slice(), 1024 * 1024).is_err());
        let missing = archive(&[("manifest.json", MANIFEST), ("config.json", CONFIG)]);
        assert!(read_uppkg(missing.as_slice(), 1024 * 1024).is_err());
        for path in ["/outside", "../outside", "./../outside", "a//b", "a\\b"] {
            assert!(normalized_path(path).is_err());
        }
        assert!(read_uppkg(b"invalid archive".as_slice(), 1024).is_err());
    }

    #[test]
    fn runtime_platform_checks_every_eligible_architecture() {
        let runtime = Platform {
            os: "linux".into(),
            architecture: "arm64".into(),
        };
        runtime.require_compatible(&["arm64".into()]).unwrap();
        assert!(runtime.require_compatible(&["amd64".into()]).is_err());
        assert!(runtime
            .require_compatible(&["arm64".into(), "amd64".into()])
            .is_err());
        assert!(runtime.require_compatible(&[]).is_err());
    }
}
