use super::{kubectl_apply_stdin, run_cmd, run_cmd_output, sync_registry_hosts_entry};
use clap::Args;
use flate2::read::GzDecoder;
use serde::Deserialize;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use tar::Archive;

const REGISTRY_YAML: &str = include_str!("../../../bootstrap/registry/registry.yaml");

/// Host address for `docker push` (NodePort exposed by the in-cluster registry)
const REGISTRY_PUSH: &str = "localhost:30500";

/// Cluster-internal address used in Crossplane package references
const REGISTRY_PULL: &str = "registry.crossplane-system.svc.cluster.local:5000";
const REGISTRY_HOSTNAME: &str = "registry.crossplane-system.svc.cluster.local";

#[derive(Args, Debug)]
pub struct ConfigArgs {
    /// Path to the local XRD project directory (defaults to current directory)
    #[arg(long, conflicts_with = "repo")]
    pub path: Option<String>,

    /// GitHub repository in <org>/<repo> format (for example hops-ops/helm-certmanager)
    #[arg(long, conflicts_with = "path")]
    pub repo: Option<String>,

    /// Version tag to apply directly from ghcr.io without cloning/building (requires --repo)
    #[arg(long, requires = "repo")]
    pub version: Option<String>,
}

#[derive(Clone, Debug)]
struct RepoSpec {
    org: String,
    repo: String,
}

#[derive(Clone, Debug)]
struct LoadedImage {
    source: String,
    uppkg_path: PathBuf,
}

#[derive(Clone, Debug)]
struct RenderRewrite {
    digest: String,
    target_prefix: String,
}

#[derive(Debug, Deserialize)]
struct DockerSaveManifestEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DockerImageConfig {
    config: Option<DockerImageConfigSection>,
}

#[derive(Debug, Deserialize)]
struct DockerImageConfigSection {
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

pub fn run(args: &ConfigArgs) -> Result<(), Box<dyn Error>> {
    match (args.repo.as_deref(), args.version.as_deref()) {
        (Some(repo), Some(version)) => apply_repo_version(repo, version),
        (Some(repo), None) => run_repo_clone(repo),
        (None, _) => run_local_path(args.path.as_deref().unwrap_or(".")),
    }
}

fn run_repo_clone(repo: &str) -> Result<(), Box<dyn Error>> {
    let spec = parse_repo_spec(repo)?;
    let clone_dir = std::env::temp_dir().join(format!(
        "hops-cli-config-repo-{}-{}-{}",
        sanitize_name_component(&spec.org),
        sanitize_name_component(&spec.repo),
        unique_suffix()
    ));
    let clone_path = clone_dir.to_string_lossy().to_string();
    let clone_url = format!("https://github.com/{}/{}", spec.org, spec.repo);

    log::info!("Cloning {}...", clone_url);
    run_cmd("git", &["clone", &clone_url, &clone_path])?;

    let result = run_local_path(&clone_path);
    let _ = fs::remove_dir_all(&clone_dir);
    result
}

fn apply_repo_version(repo: &str, version: &str) -> Result<(), Box<dyn Error>> {
    let spec = parse_repo_spec(repo)?;
    let version = version.trim();
    if version.is_empty() {
        return Err("`--version` cannot be empty".into());
    }

    let package_ref = format!("ghcr.io/{}/{}:{}", spec.org, spec.repo, version);
    let config_name = format!(
        "{}-{}",
        sanitize_name_component(&spec.org),
        sanitize_name_component(&spec.repo)
    );
    apply_configuration(&config_name, &package_ref, false)
}

fn parse_repo_spec(repo: &str) -> Result<RepoSpec, Box<dyn Error>> {
    let trimmed = repo.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("`--repo` cannot be empty".into());
    }

    let no_prefix = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("github.com/"))
        .unwrap_or(trimmed);
    let no_suffix = no_prefix.strip_suffix(".git").unwrap_or(no_prefix);

    let parts: Vec<&str> = no_suffix.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(format!("invalid --repo '{}': expected <org>/<repo>", repo).into());
    }

    Ok(RepoSpec {
        org: parts[0].to_string(),
        repo: parts[1].to_string(),
    })
}

fn sanitize_name_component(input: &str) -> String {
    let mut out = input
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();

    while out.contains("--") {
        out = out.replace("--", "-");
    }

    out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "xrd".to_string()
    } else {
        out
    }
}

fn run_local_path(path: &str) -> Result<(), Box<dyn Error>> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", path).into());
    }

    ensure_registry()?;
    sync_registry_hosts_entry("crossplane-system", "registry", REGISTRY_HOSTNAME)?;

    // Build the Crossplane package
    log::info!("Building Crossplane package in {}...", path);
    let status = Command::new("up")
        .args(["project", "build"])
        .current_dir(dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        return Err(format!("up project build exited with {}", status).into());
    }

    // Find .uppkg files in _output/
    let output_dir = dir.join("_output");
    let packages: Vec<_> = fs::read_dir(&output_dir)
        .map_err(|e| format!("Failed to read {}: {}", output_dir.display(), e))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().map_or(false, |ext| ext == "uppkg"))
        .collect();

    if packages.is_empty() {
        return Err(format!("No .uppkg files found in {}", output_dir.display()).into());
    }

    // Load each package into docker and collect image names.
    let mut loaded = Vec::new();
    for pkg in &packages {
        let pkg_path = pkg.path();
        let pkg_str = pkg_path.to_string_lossy();
        log::info!("Loading {}...", pkg_str);

        let output = Command::new("docker")
            .args(["load", "-i", &*pkg_str])
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("docker load failed: {}", stderr).into());
        }

        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(img) = line.strip_prefix("Loaded image: ") {
                loaded.push(LoadedImage {
                    source: img.trim().to_string(),
                    uppkg_path: pkg_path.clone(),
                });
            }
        }
    }

    if loaded.is_empty() {
        return Err("No images were loaded from .uppkg files".into());
    }

    // De-duplicate images that can appear multiple times across loaded tarballs.
    let mut seen = HashSet::new();
    loaded.retain(|img| seen.insert(img.source.clone()));

    let arch = docker_arch().to_string();
    let mut render_rewrites: HashMap<String, RenderRewrite> = HashMap::new();

    // Push non-Configuration images first. For local render functions, capture
    // the pushed digest so we can patch the corresponding configuration package
    // metadata and keep dependency resolution enabled.
    for img in &loaded {
        if is_configuration_image(&img.source) {
            continue;
        }

        let push_ref = rewrite_registry(&img.source, REGISTRY_PUSH);
        let (img_path, tag) = split_ref(&img.source);

        if img_path.ends_with("_render") {
            log::info!("Rebuilding {} (fix OCI config)...", push_ref);
            docker_build_from(&img.source, &push_ref)?;

            if tag == arch {
                let digest = docker_push_and_get_digest(&push_ref)?;
                let target_prefix = format!("{}/{}", REGISTRY_PULL, strip_registry(img_path));
                render_rewrites.insert(
                    img_path.to_string(),
                    RenderRewrite {
                        digest,
                        target_prefix,
                    },
                );
            } else {
                log::info!("Pushing {}...", push_ref);
                run_cmd("docker", &["push", &push_ref])?;
            }
        } else {
            run_cmd("docker", &["tag", &img.source, &push_ref])?;
            log::info!("Pushing {}...", push_ref);
            run_cmd("docker", &["push", &push_ref])?;
        }
    }

    // Rewrite local render dependency pulls to local registry while preserving
    // the original package source in spec.package.
    for (source, rewrite) in &render_rewrites {
        log::info!(
            "Applying ImageConfig rewrite for {} -> {}...",
            source,
            rewrite.target_prefix
        );
        kubectl_apply_stdin(&format!(
            "apiVersion: pkg.crossplane.io/v1beta1
kind: ImageConfig
metadata:
  name: {}
spec:
  matchImages:
    - type: Prefix
      prefix: {}
  rewriteImage:
    prefix: {}
",
            image_config_name(source),
            source,
            rewrite.target_prefix
        ))?;
    }

    // Patch and push configuration images.
    let mut config_pull_refs = Vec::new();
    for img in &loaded {
        if !is_configuration_image(&img.source) {
            continue;
        }

        let push_ref = rewrite_registry(&img.source, REGISTRY_PUSH);
        let pull_ref = rewrite_registry(&img.source, REGISTRY_PULL);
        config_pull_refs.push(pull_ref.clone());

        let mut source_to_push = img.source.clone();
        let package_yaml = extract_package_yaml_from_uppkg(&img.uppkg_path, &img.source)?;
        let (patched_yaml, changed) =
            rewrite_render_dependency_digests(&package_yaml, &render_rewrites);
        if changed {
            log::info!(
                "Patching package metadata for {} to use local render digests...",
                img.source
            );
            source_to_push = build_patched_configuration_image(&img.source, &patched_yaml)?;
        }

        run_cmd("docker", &["tag", &source_to_push, &push_ref])?;
        log::info!("Pushing {}...", push_ref);
        run_cmd("docker", &["push", &push_ref])?;
    }

    // Apply Crossplane Configuration resources and let Crossplane resolve
    // dependencies (skipDependencyResolution is intentionally not set).
    for pull_ref in &config_pull_refs {
        let (img_path, _) = split_ref(pull_ref);
        let name = img_path.rsplit('/').next().unwrap_or(img_path);
        apply_configuration(name, pull_ref, false)?;
    }

    Ok(())
}

fn apply_configuration(
    name: &str,
    package_ref: &str,
    skip_dependency_resolution: bool,
) -> Result<(), Box<dyn Error>> {
    log::info!("Applying Configuration '{}'...", name);
    kubectl_apply_stdin(&build_configuration_yaml(
        name,
        package_ref,
        skip_dependency_resolution,
    ))?;
    Ok(())
}

fn build_configuration_yaml(
    name: &str,
    package_ref: &str,
    skip_dependency_resolution: bool,
) -> String {
    let mut yaml = format!(
        "apiVersion: pkg.crossplane.io/v1
kind: Configuration
metadata:
  name: {name}
spec:
  package: {package_ref}
  packagePullPolicy: Always\n"
    );

    if skip_dependency_resolution {
        yaml.push_str("  skipDependencyResolution: true\n");
    }

    yaml
}

/// Ensure the in-cluster registry is deployed and available.
fn ensure_registry() -> Result<(), Box<dyn Error>> {
    let result = run_cmd_output(
        "kubectl",
        &[
            "get",
            "deployment",
            "registry",
            "-n",
            "crossplane-system",
            "-o",
            "jsonpath={.status.availableReplicas}",
        ],
    );

    if let Ok(replicas) = result {
        if replicas.trim() == "1" {
            return Ok(());
        }
    }

    log::info!("Deploying local package registry...");
    kubectl_apply_stdin(REGISTRY_YAML)?;

    // Wait for the registry pod to become ready
    for _ in 0..60 {
        let out = run_cmd_output(
            "kubectl",
            &[
                "get",
                "deployment",
                "registry",
                "-n",
                "crossplane-system",
                "-o",
                "jsonpath={.status.availableReplicas}",
            ],
        );
        if let Ok(r) = out {
            if r.trim() == "1" {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    Err("Timed out waiting for registry deployment".into())
}

fn is_configuration_image(image: &str) -> bool {
    split_ref(image).1 == "configuration"
}

fn extract_package_yaml_from_uppkg(
    uppkg_path: &Path,
    configuration_image: &str,
) -> Result<String, Box<dyn Error>> {
    let manifest_bytes = read_entry_from_tar(uppkg_path, "manifest.json")?;
    let manifest: Vec<DockerSaveManifestEntry> = serde_json::from_slice(&manifest_bytes)?;

    let config_entry = manifest
        .iter()
        .find(|entry| {
            entry
                .repo_tags
                .as_ref()
                .map(|tags| tags.iter().any(|t| t == configuration_image))
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            format!(
                "Could not find '{}' in manifest {}",
                configuration_image,
                uppkg_path.display()
            )
        })?;

    let mut base_layer: Option<String> = None;
    let config_json = read_entry_from_tar(uppkg_path, &config_entry.config)?;
    if let Ok(image_config) = serde_json::from_slice::<DockerImageConfig>(&config_json) {
        if let Some(labels) = image_config.config.and_then(|c| c.labels) {
            for (key, value) in labels {
                if value != "base" {
                    continue;
                }
                if let Some(digest) = key.strip_prefix("io.crossplane.xpkg:sha256:") {
                    let candidate = format!("{}.tar.gz", digest);
                    if config_entry.layers.iter().any(|l| l == &candidate) {
                        base_layer = Some(candidate);
                        break;
                    }
                }
            }
        }
    }

    let base_layer = base_layer
        .or_else(|| config_entry.layers.first().cloned())
        .ok_or_else(|| {
            format!(
                "Configuration image '{}' has no layers in {}",
                configuration_image,
                uppkg_path.display()
            )
        })?;
    let layer_bytes = read_entry_from_tar(uppkg_path, &base_layer)?;
    let decoder = GzDecoder::new(Cursor::new(layer_bytes));
    let mut layer_archive = Archive::new(decoder);

    for entry in layer_archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        if path == "package.yaml" {
            let mut contents = Vec::new();
            entry.read_to_end(&mut contents)?;
            return Ok(String::from_utf8(contents)?);
        }
    }

    Err(format!(
        "package.yaml not found in base layer '{}' from {}",
        &base_layer,
        uppkg_path.display()
    )
    .into())
}

fn read_entry_from_tar(tar_path: &Path, entry_name: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let file = fs::File::open(tar_path)?;
    let mut archive = Archive::new(file);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        if path == entry_name {
            let mut out = Vec::new();
            entry.read_to_end(&mut out)?;
            return Ok(out);
        }
    }

    Err(format!(
        "entry '{}' not found in tar {}",
        entry_name,
        tar_path.display()
    )
    .into())
}

fn rewrite_render_dependency_digests(
    package_yaml: &str,
    rewrites: &HashMap<String, RenderRewrite>,
) -> (String, bool) {
    if rewrites.is_empty() {
        return (package_yaml.to_string(), false);
    }

    let mut changed = false;
    let mut in_depends = false;
    let mut current_package: Option<String> = None;
    let mut lines: Vec<String> = package_yaml.lines().map(|l| l.to_string()).collect();

    for line in &mut lines {
        let trimmed = line.trim();

        if trimmed == "dependsOn:" {
            in_depends = true;
            current_package = None;
            continue;
        }

        if in_depends && !trimmed.is_empty() && !line.starts_with(' ') && !line.starts_with('\t') {
            in_depends = false;
            current_package = None;
        }

        if !in_depends {
            continue;
        }

        if trimmed.starts_with("- ") {
            current_package = None;
            let item = trimmed.trim_start_matches("- ").trim();
            if let Some(value) = item.strip_prefix("package:") {
                current_package = Some(clean_yaml_scalar(value));
            }
            continue;
        }

        if let Some(value) = trimmed.strip_prefix("package:") {
            current_package = Some(clean_yaml_scalar(value));
            continue;
        }

        if trimmed.starts_with("version:") {
            if let Some(package) = &current_package {
                if let Some(rewrite) = rewrites.get(package) {
                    let indent = &line[..line.len() - line.trim_start().len()];
                    *line = format!("{indent}version: {}", rewrite.digest);
                    changed = true;
                }
            }
        }
    }

    let mut out = lines.join("\n");
    if package_yaml.ends_with('\n') {
        out.push('\n');
    }
    (out, changed)
}

fn clean_yaml_scalar(s: &str) -> String {
    s.trim().trim_matches('"').trim_matches('\'').to_string()
}

fn build_patched_configuration_image(
    source_image: &str,
    package_yaml: &str,
) -> Result<String, Box<dyn Error>> {
    let build_dir = std::env::temp_dir().join(format!(
        "hops-cli-config-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    fs::create_dir_all(&build_dir)?;
    fs::write(build_dir.join("package.yaml"), package_yaml)?;
    fs::write(
        build_dir.join("Dockerfile"),
        format!(
            "FROM {source_image} AS src\n\
             FROM scratch\n\
             COPY --from=src / /\n\
             COPY package.yaml /package.yaml\n"
        ),
    )?;

    let target_tag = format!(
        "hops-local/config-patched-{}:{}",
        short_hash(source_image),
        unique_suffix()
    );

    let status = Command::new("docker")
        .args([
            "build",
            "-t",
            &target_tag,
            build_dir.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    let _ = fs::remove_dir_all(&build_dir);

    if !status.success() {
        return Err(format!("docker build exited with {}", status).into());
    }

    Ok(target_tag)
}

fn docker_push_and_get_digest(image: &str) -> Result<String, Box<dyn Error>> {
    let output = Command::new("docker").args(["push", image]).output()?;
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("docker push failed: {}", stderr).into());
    }

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_docker_push_digest(&combined).ok_or_else(|| {
        format!(
            "Unable to parse digest from docker push output for {}",
            image
        )
        .into()
    })
}

fn parse_docker_push_digest(output: &str) -> Option<String> {
    for line in output.lines() {
        if let Some(idx) = line.find("digest: sha256:") {
            let digest = line[idx + "digest: ".len()..]
                .split_whitespace()
                .next()?
                .to_string();
            return Some(digest);
        }
    }
    None
}

fn image_config_name(source: &str) -> String {
    let hash = short_hash(source);
    let mut body: String = source
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while body.contains("--") {
        body = body.replace("--", "-");
    }
    body = body.trim_matches('-').to_string();
    if body.is_empty() {
        body = "image".to_string();
    }

    let prefix = "hops-local-rewrite-";
    let max_body_len = 63usize.saturating_sub(prefix.len() + hash.len() + 1);
    if body.len() > max_body_len {
        body.truncate(max_body_len);
    }

    format!("{prefix}{body}-{hash}")
}

fn short_hash(input: &str) -> String {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    let hex = format!("{:016x}", hasher.finish());
    hex[..8].to_string()
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Replace the registry portion of an image reference.
/// "ghcr.io/hops-ops/helm-airflow:configuration" -> "<registry>/hops-ops/helm-airflow:configuration"
fn rewrite_registry(image: &str, registry: &str) -> String {
    let (path_with_reg, tag) = split_ref(image);
    let path = strip_registry(path_with_reg);
    format!("{}/{}:{}", registry, path, tag)
}

/// Strip the registry prefix from an image path.
fn strip_registry(path: &str) -> &str {
    if let Some(pos) = path.find('/') {
        let prefix = &path[..pos];
        if prefix.contains('.') || prefix.contains(':') {
            return &path[pos + 1..];
        }
    }
    path
}

/// Split "path:tag" into ("path", "tag").
fn split_ref(image: &str) -> (&str, &str) {
    image.rsplit_once(':').unwrap_or((image, "latest"))
}

/// Map Rust arch constant to Docker platform architecture name.
fn docker_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

/// Rebuild a Docker image with just `FROM <src>` to produce a valid OCI config.
/// This fixes images where rootfs.type is empty (a known issue with `up project build`
/// render function images).
fn docker_build_from(src: &str, tag: &str) -> Result<(), Box<dyn Error>> {
    let dockerfile = format!("FROM {}\n", src);
    let mut child = Command::new("docker")
        .args(["build", "-t", tag, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(dockerfile.as_bytes())?;
    }

    let status = child.wait()?;
    if !status.success() {
        return Err(format!("docker build exited with {}", status).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_push_digest() {
        let out = "latest: digest: sha256:0123456789abcdef size: 1234";
        assert_eq!(
            parse_docker_push_digest(out).as_deref(),
            Some("sha256:0123456789abcdef")
        );
    }

    #[test]
    fn rewrite_render_dep_digest() {
        let yaml = r#"---
apiVersion: meta.pkg.crossplane.io/v1
kind: Configuration
spec:
  dependsOn:
  - kind: Function
    package: ghcr.io/hops-ops/helm-airflow_render
    version: sha256:old
  - kind: Function
    package: xpkg.crossplane.io/crossplane-contrib/function-auto-ready
    version: '>=v0.6.0'
"#;

        let mut rewrites = HashMap::new();
        rewrites.insert(
            "ghcr.io/hops-ops/helm-airflow_render".to_string(),
            RenderRewrite {
                digest: "sha256:new".to_string(),
                target_prefix:
                    "registry.crossplane-system.svc.cluster.local:5000/hops-ops/helm-airflow_render"
                        .to_string(),
            },
        );

        let (patched, changed) = rewrite_render_dependency_digests(yaml, &rewrites);
        assert!(changed);
        assert!(patched.contains("version: sha256:new"));
        assert!(patched.contains("version: '>=v0.6.0'"));
    }

    #[test]
    fn parse_repo_spec_accepts_slug_and_github_url() {
        let slug = parse_repo_spec("hops-ops/helm-certmanager").unwrap();
        assert_eq!(slug.org, "hops-ops");
        assert_eq!(slug.repo, "helm-certmanager");

        let url = parse_repo_spec("https://github.com/hops-ops/helm-certmanager.git").unwrap();
        assert_eq!(url.org, "hops-ops");
        assert_eq!(url.repo, "helm-certmanager");
    }

    #[test]
    fn parse_repo_spec_rejects_invalid_values() {
        assert!(parse_repo_spec("").is_err());
        assert!(parse_repo_spec("hops-ops").is_err());
        assert!(parse_repo_spec("hops-ops/helm-certmanager/extra").is_err());
    }

    #[test]
    fn sanitize_name_component_normalizes_for_k8s_names() {
        assert_eq!(sanitize_name_component("Hops_Ops"), "hops-ops");
        assert_eq!(
            sanitize_name_component("helm.certmanager"),
            "helm-certmanager"
        );
        assert_eq!(sanitize_name_component("---"), "xrd");
    }

    #[test]
    fn build_configuration_yaml_controls_dependency_resolution_flag() {
        let with_skip = build_configuration_yaml("cfg", "ghcr.io/hops-ops/x:v1", true);
        assert!(with_skip.contains("skipDependencyResolution: true"));

        let without_skip = build_configuration_yaml("cfg", "ghcr.io/hops-ops/x:v1", false);
        assert!(!without_skip.contains("skipDependencyResolution: true"));
    }
}
