use crate::commands::config::configuration_name_from_package_ref;
use crate::commands::local::backend::{self, Backend, ClusterProvider, DockerProvider};
use crate::commands::local::package_install::run_watch;
use crate::commands::local::package_install::{
    docker_arch, ensure_cached_repo_checkout, ensure_registry, image_config_name,
    parse_docker_push_digest, parse_repo_spec, registry_pull, registry_push,
    resolve_repo_install_target, rewrite_registry, rewrite_registry_with_tag, short_hash,
    split_ref, strip_registry, unique_suffix, RepoInstallTarget, RepoSpec,
};
use crate::commands::local::{kubectl_apply_stdin, kubectl_command, run_cmd, run_cmd_output};
use clap::Args;
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tar::Archive;

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

    /// Set spec.skipDependencyResolution=true on the generated Configuration
    #[arg(long)]
    pub skip_dependency_resolution: bool,

    /// Kubernetes context to use for all kubectl commands (e.g. "colima")
    #[arg(long)]
    pub context: Option<String>,

    /// How Kubernetes nodes are provisioned: `kind`, `dory`, or `colima`.
    #[arg(long = "cluster-provider", value_enum)]
    pub cluster_provider: Option<ClusterProvider>,

    /// Container engine for kind/tools: `dory`, `colima`, or `docker`.
    #[arg(long = "docker-provider", value_enum)]
    pub docker_provider: Option<DockerProvider>,

    /// Named hops-managed kind cluster. Default `hops` uses context `kind-hops`.
    #[arg(long = "cluster-name", value_name = "NAME")]
    pub cluster_name: Option<String>,

    /// Watch the project directory for changes and re-run install automatically
    #[arg(long, conflicts_with = "repo")]
    pub watch: bool,

    /// Debounce interval for --watch in seconds (default: 15)
    #[arg(long, requires = "watch", default_value = "15")]
    pub debounce: u64,
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

#[derive(Debug, Deserialize)]
struct KubeList<T> {
    items: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct PackageMetadataName {
    name: String,
}

#[derive(Debug, Deserialize)]
struct PackageSpec {
    #[serde(rename = "package")]
    package_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PackageResource {
    metadata: PackageMetadataName,
    spec: Option<PackageSpec>,
}

pub fn run(args: &ConfigArgs) -> Result<(), Box<dyn Error>> {
    let provider_selected = args.cluster_provider.is_some() || args.docker_provider.is_some();
    let backend = backend::activate_with_providers(
        args.cluster_provider,
        args.docker_provider,
        args.cluster_name.as_deref(),
        args.context.as_deref(),
    )?;

    match (args.repo.as_deref(), args.version.as_deref()) {
        (Some(repo), Some(version)) => {
            apply_repo_version(repo, version, args.skip_dependency_resolution)
        }
        (Some(repo), None) => run_repo_install(
            repo,
            args.skip_dependency_resolution,
            backend,
            provider_selected,
            args.context.as_deref(),
        ),
        (None, _) => {
            let path = args.path.as_deref().unwrap_or(".");
            prepare_local_registry(backend, provider_selected, args.context.as_deref())?;
            run_local_path(path, args.skip_dependency_resolution)?;

            if args.watch {
                let path_owned = path.to_string();
                let skip = args.skip_dependency_resolution;
                run_watch(path, args.debounce, move || {
                    run_local_path(&path_owned, skip)
                })?;
            }

            Ok(())
        }
    }
}

fn run_repo_install(
    repo: &str,
    skip_dependency_resolution: bool,
    backend: Backend,
    provider_selected: bool,
    context: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let spec = parse_repo_spec(repo)?;
    match resolve_repo_install_target(&spec)? {
        RepoInstallTarget::SourceBuild => run_repo_clone(
            &spec,
            skip_dependency_resolution,
            backend,
            provider_selected,
            context,
        ),
        RepoInstallTarget::PublishedVersion(version) => {
            apply_repo_version_spec(&spec, &version, skip_dependency_resolution)
        }
    }
}

fn run_repo_clone(
    spec: &RepoSpec,
    skip_dependency_resolution: bool,
    backend: Backend,
    provider_selected: bool,
    context: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let cache_path = ensure_cached_repo_checkout(spec)?;
    prepare_local_registry(backend, provider_selected, context)?;
    run_local_path(&cache_path.to_string_lossy(), skip_dependency_resolution)
}

fn prepare_local_registry(
    backend: Backend,
    provider_selected: bool,
    context: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    ensure_registry()?;
    backend::wire_local_registry_for_target(backend, provider_selected, context)
}

fn apply_repo_version_spec(
    spec: &RepoSpec,
    version: &str,
    skip_dependency_resolution: bool,
) -> Result<(), Box<dyn Error>> {
    let version = version.trim();
    if version.is_empty() {
        return Err("`--version` cannot be empty".into());
    }

    let package_ref = format!("ghcr.io/{}/{}:{}", spec.org, spec.repo, version);
    let config_name = configuration_name_from_package_ref(&package_ref);

    // Delete any existing render Function so Crossplane re-resolves with the
    // correct digest for this version (avoids conflicts when switching between
    // local and published builds).
    let render_source = format!("ghcr.io/{}/{}_render", spec.org, spec.repo);
    let sources: HashSet<String> = [render_source.clone()].into_iter().collect();
    let removed = delete_package_resources_by_source("function.pkg.crossplane.io", &sources)?;
    if removed > 0 {
        log::info!(
            "Deleted {} stale Function package(s) before version install",
            removed
        );
    }

    // Delete any local-registry ImageConfig rewrite left over from a previous
    // `config install --path` so Crossplane pulls from ghcr.io.
    let ic_name = image_config_name(&render_source);
    let ic_check = kubectl_command(&["get", "imageconfig.pkg.crossplane.io", &ic_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if ic_check.map(|s| s.success()).unwrap_or(false) {
        run_cmd(
            "kubectl",
            &["delete", "imageconfig.pkg.crossplane.io", &ic_name],
        )?;
        log::info!("Deleted local ImageConfig rewrite '{}'", ic_name);
    }

    // Delete stale inactive ConfigurationRevisions pointing at the local
    // registry so they don't block dependency resolution for the published version.
    delete_local_registry_config_revisions(&config_name)?;

    apply_configuration(&config_name, &package_ref, skip_dependency_resolution)
}

fn apply_repo_version(
    repo: &str,
    version: &str,
    skip_dependency_resolution: bool,
) -> Result<(), Box<dyn Error>> {
    let spec = parse_repo_spec(repo)?;
    apply_repo_version_spec(&spec, version, skip_dependency_resolution)
}

fn run_local_path(path: &str, skip_dependency_resolution: bool) -> Result<(), Box<dyn Error>> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", path).into());
    }

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
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "uppkg"))
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

    let function_sources: HashSet<String> = loaded
        .iter()
        .filter(|img| !is_configuration_image(&img.source))
        .map(|img| package_source(&img.source))
        .collect();

    let arch = docker_arch().to_string();
    let mut render_rewrites: HashMap<String, RenderRewrite> = HashMap::new();

    // Push non-Configuration images first. For local render functions, capture
    // the pushed digest so we can patch the corresponding configuration package
    // metadata and keep dependency resolution enabled.
    for img in &loaded {
        if is_configuration_image(&img.source) {
            continue;
        }

        let push_ref = rewrite_registry(&img.source, &registry_push());
        let (img_path, tag) = split_ref(&img.source);

        // All non-configuration images are Crossplane Function packages (the
        // configuration filter ran above). Single-function repos historically
        // produced one image named <repo>_render; multi-function repos produce
        // <repo>_<funcname> per function. Both need the OCI-config rebuild +
        // digest capture + ImageConfig rewrite treatment.
        log::info!("Rebuilding {} (fix OCI config)...", push_ref);
        docker_build_from(&img.source, &push_ref)?;

        if tag == arch {
            let digest = docker_push_and_get_digest(&push_ref)?;
            let target_prefix = format!("{}/{}", registry_pull(), strip_registry(img_path));
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
    let mut configurations = Vec::new();
    for img in &loaded {
        if !is_configuration_image(&img.source) {
            continue;
        }

        let dev_tag = dev_tag_for_uppkg(&img.uppkg_path)?;
        let push_ref = rewrite_registry_with_tag(&img.source, &registry_push(), &dev_tag);
        let pull_ref = rewrite_registry_with_tag(&img.source, registry_pull(), &dev_tag);
        log::info!(
            "Using local build version '{}' for {}...",
            dev_tag,
            img.source
        );
        let mut source_to_push = img.source.clone();
        let package_yaml = extract_package_yaml_from_uppkg(&img.uppkg_path, &img.source)?;
        let configuration_name = configuration_name_from_package_ref(&pull_ref);
        configurations.push((configuration_name, pull_ref.clone()));
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
    for (name, pull_ref) in &configurations {
        let existing_package_ref = current_configuration_package_ref(&name)?;
        log_existing_install_replacement(&name, existing_package_ref.as_deref(), pull_ref);

        // Delete inactive ConfigurationRevisions pointing at the remote registry.
        // When switching from a published version to a local build, the old
        // inactive revision's Function dependency has a stale digest that
        // conflicts with the locally-pushed render image.
        delete_remote_registry_config_revisions(&name)?;

        apply_configuration(&name, pull_ref, skip_dependency_resolution)?;
    }

    // Delete existing Function packages only after the new Configuration has
    // been applied. This ensures Crossplane sees the new desired package
    // revision before we force render function recreation.
    if !function_sources.is_empty() {
        let removed_functions =
            delete_package_resources_by_source("function.pkg.crossplane.io", &function_sources)?;
        let removed_function_revisions = delete_package_resources_by_source(
            "functionrevision.pkg.crossplane.io",
            &function_sources,
        )?;
        if removed_functions > 0 || removed_function_revisions > 0 {
            log::info!(
                "Deleted {} Function package(s) and {} FunctionRevision(s) from matching sources after re-apply",
                removed_functions,
                removed_function_revisions
            );
        }
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

fn delete_package_resources_by_source(
    resource: &str,
    sources: &HashSet<String>,
) -> Result<usize, Box<dyn Error>> {
    if sources.is_empty() {
        return Ok(0);
    }

    let raw = run_cmd_output("kubectl", &["get", resource, "-o", "json"])?;
    let list: KubeList<PackageResource> = serde_json::from_str(&raw)?;

    let mut deleted = 0usize;
    for item in list.items {
        let Some(spec) = item.spec else {
            continue;
        };
        let Some(package_ref) = spec.package_ref else {
            continue;
        };
        if !sources.contains(&package_source(&package_ref)) {
            continue;
        }

        run_cmd(
            "kubectl",
            &[
                "delete",
                resource,
                &item.metadata.name,
                "--ignore-not-found",
            ],
        )?;
        deleted += 1;
    }

    Ok(deleted)
}

fn package_source(package_ref: &str) -> String {
    let trimmed = package_ref.trim();
    if let Some((source, _)) = trimmed.split_once('@') {
        return source.to_string();
    }

    if let Some(slash_idx) = trimmed.rfind('/') {
        let suffix = &trimmed[slash_idx + 1..];
        if let Some(colon_idx) = suffix.rfind(':') {
            let idx = slash_idx + 1 + colon_idx;
            return trimmed[..idx].to_string();
        }
    }

    trimmed.to_string()
}

fn package_tag(package_ref: &str) -> Option<&str> {
    if let Some((_, digest)) = package_ref.rsplit_once('@') {
        return Some(digest);
    }

    package_ref.rsplit_once(':').map(|(_, tag)| tag)
}

fn current_configuration_package_ref(name: &str) -> Result<Option<String>, Box<dyn Error>> {
    let output = kubectl_command(&["get", "configuration.pkg.crossplane.io", name, "-o", "json"])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("NotFound") {
            return Ok(None);
        }
        return Err(format!("kubectl get configuration '{}' failed: {}", name, stderr).into());
    }

    let resource: PackageResource = serde_json::from_slice(&output.stdout)?;
    Ok(resource.spec.and_then(|spec| spec.package_ref))
}

fn log_existing_install_replacement(
    name: &str,
    existing_package_ref: Option<&str>,
    new_package_ref: &str,
) {
    let Some(existing_package_ref) = existing_package_ref else {
        return;
    };

    let new_tag = package_tag(new_package_ref).unwrap_or(new_package_ref);
    let existing_tag = package_tag(existing_package_ref).unwrap_or(existing_package_ref);

    if existing_tag == new_tag {
        log::info!(
            "Found existing installation '{}' already using local build version '{}'...",
            name,
            new_tag
        );
        return;
    }

    if existing_tag.starts_with("dev-") {
        log::info!(
            "Found existing installation '{}' using local build version '{}'; replacing with '{}'...",
            name,
            existing_tag,
            new_tag
        );
    } else {
        log::info!(
            "Found existing installation '{}' using package '{}'; replacing with local build version '{}'...",
            name,
            existing_package_ref,
            new_tag
        );
    }
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

    // Extract the source image's filesystem via docker create + export,
    // avoiding multi-stage FROM which breaks when Docker's snapshot cache
    // is stale for images loaded via `docker load`.
    let container_name = format!("hops-extract-{}", unique_suffix());
    let create_out = Command::new("docker")
        .args(["create", "--name", &container_name, source_image, "true"])
        .output()?;
    if !create_out.status.success() {
        return Err(format!(
            "docker create failed: {}",
            String::from_utf8_lossy(&create_out.stderr)
        )
        .into());
    }

    let content_dir = build_dir.join("content");
    fs::create_dir_all(&content_dir)?;

    let export_status = Command::new("sh")
        .args([
            "-c",
            &format!(
                "docker export {} | tar -xf - -C {}",
                container_name,
                content_dir.to_string_lossy()
            ),
        ])
        .status()?;

    // Always remove the temp container.
    let _ = Command::new("docker")
        .args(["rm", "-f", &container_name])
        .output();

    if !export_status.success() {
        let _ = fs::remove_dir_all(&build_dir);
        return Err("docker export failed".into());
    }

    // Replace package.yaml with the patched version.
    fs::write(content_dir.join("package.yaml"), package_yaml)?;

    // Build from scratch using the extracted + patched content.
    fs::write(
        build_dir.join("Dockerfile"),
        "FROM scratch\nCOPY content/ /\n",
    )?;

    let target_tag = format!(
        "hops-local/config-patched-{}:{}",
        short_hash(source_image),
        unique_suffix()
    );

    // --provenance=false --sbom=false disable buildx's attestation manifests.
    // With attestations enabled (modern Docker default), the output is wrapped
    // in an OCI manifest list containing only the host arch + attestation
    // entries. Crossplane's package fetcher (go-containerregistry remote.Image
    // with no platform hint) defaults to linux/amd64 when navigating an index,
    // so it fails with "no child with platform linux/amd64" against our
    // arm64-only list. Without attestations, buildx emits a single manifest,
    // which Crossplane fetches directly without index navigation.
    let status = Command::new("docker")
        .args([
            "build",
            "--provenance=false",
            "--sbom=false",
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

fn dev_tag_for_uppkg(uppkg_path: &Path) -> Result<String, Box<dyn Error>> {
    let mut file = fs::File::open(uppkg_path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];

    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }

    let hex = format!("{:x}", hasher.finalize());
    Ok(format!("dev-{}", &hex[..12]))
}

/// Map an image reference's arch tag (e.g. `:amd64` / `:arm64` from
/// `up project build`) to a Docker `--platform` value.
///
/// Without an explicit platform, buildx on Apple Silicon warns
/// `InvalidBaseImagePlatform` when rebuilding the non-host arch variant
/// (`FROM …:amd64` on arm64, and the reverse on Intel).
fn platform_for_image_ref(src: &str) -> Option<&'static str> {
    let (_, tag) = split_ref(src);
    // Prefer the trailing path segment when the tag is a digest (rare for
    // function images from up, which use :amd64 / :arm64).
    let arch = if tag.starts_with("sha256:") {
        src.rsplit('/').next().unwrap_or(tag)
    } else {
        tag
    };
    match arch {
        "amd64" | "linux/amd64" => Some("linux/amd64"),
        "arm64" | "linux/arm64" | "aarch64" => Some("linux/arm64"),
        _ => None,
    }
}

/// Rebuild a Docker image with just `FROM <src>` to produce a valid OCI config.
/// This fixes images where rootfs.type is empty (a known issue with `up project build`
/// render function images).
fn docker_build_from(src: &str, tag: &str) -> Result<(), Box<dyn Error>> {
    let dockerfile = format!("FROM {}\n", src);
    // See note on `--provenance=false --sbom=false` in
    // `build_patched_configuration_image`: without these, buildx wraps the
    // output in a single-arch manifest list that Crossplane (which fetches
    // package layers as linux/amd64 regardless of host) cannot navigate.
    let mut args: Vec<String> = vec![
        "build".into(),
        "--provenance=false".into(),
        "--sbom=false".into(),
    ];
    if let Some(platform) = platform_for_image_ref(src) {
        args.push("--platform".into());
        args.push(platform.into());
    }
    args.push("-t".into());
    args.push(tag.into());
    args.push("-".into());

    let mut child = Command::new("docker")
        .args(&args)
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

/// Delete inactive ConfigurationRevisions whose package points at the local
/// registry. These are left over from `config install --path` and can block
/// dependency resolution when switching to a published version.
fn delete_local_registry_config_revisions(config_name: &str) -> Result<(), Box<dyn Error>> {
    let output = run_cmd_output(
        "kubectl",
        &[
            "get",
            "configurationrevision.pkg.crossplane.io",
            "-o",
            "jsonpath={range .items[*]}{.metadata.name}|{.spec.image}|{.spec.desiredState}\\n{end}",
        ],
    )?;

    for line in output.lines() {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 3 {
            continue;
        }
        let rev_name = parts[0].trim();
        let package = parts[1].trim();
        let state = parts[2].trim();

        if !rev_name.starts_with(config_name) {
            continue;
        }
        if package.contains(registry_pull()) && state == "Inactive" {
            run_cmd(
                "kubectl",
                &[
                    "delete",
                    "configurationrevision.pkg.crossplane.io",
                    rev_name,
                ],
            )?;
            log::info!("Deleted stale local ConfigurationRevision '{}'", rev_name);
        }
    }
    Ok(())
}

/// Delete inactive ConfigurationRevisions pointing at the remote registry
/// (ghcr.io). When switching from a published version to a local build,
/// these old revisions have stale Function digests that conflict.
fn delete_remote_registry_config_revisions(config_name: &str) -> Result<(), Box<dyn Error>> {
    let output = run_cmd_output(
        "kubectl",
        &[
            "get",
            "configurationrevision.pkg.crossplane.io",
            "-o",
            "jsonpath={range .items[*]}{.metadata.name}|{.spec.image}|{.spec.desiredState}\\n{end}",
        ],
    )?;

    for line in output.lines() {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 3 {
            continue;
        }
        let rev_name = parts[0].trim();
        let package = parts[1].trim();
        let state = parts[2].trim();

        if !rev_name.starts_with(config_name) {
            continue;
        }
        if !package.contains(registry_pull()) && state == "Inactive" {
            run_cmd(
                "kubectl",
                &[
                    "delete",
                    "configurationrevision.pkg.crossplane.io",
                    rev_name,
                ],
            )?;
            log::info!("Deleted stale remote ConfigurationRevision '{}'", rev_name);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn build_configuration_yaml_controls_dependency_resolution_flag() {
        let with_skip = build_configuration_yaml("cfg", "ghcr.io/hops-ops/x:v1", true);
        assert!(with_skip.contains("skipDependencyResolution: true"));

        let without_skip = build_configuration_yaml("cfg", "ghcr.io/hops-ops/x:v1", false);
        assert!(!without_skip.contains("skipDependencyResolution: true"));
    }

    #[test]
    fn source_install_uses_registry_package_identity() {
        assert_eq!(
            configuration_name_from_package_ref(
                "registry.crossplane-system.svc.cluster.local:5000/hops-ops/secret-stack:dev-abc"
            ),
            "hops-ops-secret-stack"
        );
    }

    #[test]
    fn source_install_name_sanitizes_registry_path_components() {
        assert_eq!(
            configuration_name_from_package_ref(
                "registry.crossplane-system.svc.cluster.local:5000/Hops_Ops/Secret.Stack:dev-abc"
            ),
            "hops-ops-secret-stack"
        );
    }

    #[test]
    fn local_registry_wiring_skips_foreign_context_without_provider_selection() {
        assert!(!backend::should_wire_local_registry(
            false,
            Some("kind-hops"),
            Backend::Colima
        ));
    }

    #[test]
    fn package_source_strips_tag_and_digest() {
        assert_eq!(
            package_source("ghcr.io/hops-ops/helm-airflow_render:arm64"),
            "ghcr.io/hops-ops/helm-airflow_render"
        );
        assert_eq!(
            package_source("ghcr.io/hops-ops/helm-airflow_render@sha256:abc123"),
            "ghcr.io/hops-ops/helm-airflow_render"
        );
    }

    #[test]
    fn package_tag_extracts_tag_or_digest() {
        assert_eq!(
            package_tag(
                "registry.crossplane-system.svc.cluster.local:5000/hops-ops/test:dev-123456789abc"
            ),
            Some("dev-123456789abc")
        );
        assert_eq!(
            package_tag("ghcr.io/hops-ops/test@sha256:abcdef"),
            Some("sha256:abcdef")
        );
    }

    #[test]
    fn platform_for_image_ref_from_up_arch_tags() {
        assert_eq!(
            platform_for_image_ref("ghcr.io/hops-ops/config-smoke_render:amd64"),
            Some("linux/amd64")
        );
        assert_eq!(
            platform_for_image_ref("ghcr.io/hops-ops/config-smoke_render:arm64"),
            Some("linux/arm64")
        );
        assert_eq!(
            platform_for_image_ref("ghcr.io/hops-ops/config-smoke:configuration"),
            None
        );
    }
}
