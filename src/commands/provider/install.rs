use crate::commands::local::backend::{self, Backend, ClusterProvider, DockerProvider};
use crate::commands::local::package_install::{
    docker_arch, ensure_cached_repo_checkout_at, ensure_registry, parse_repo_spec, registry_pull,
    registry_push, resolve_repo_install_target, run_watch, sanitize_name_component,
    RepoInstallTarget, RepoSpec,
};
use crate::commands::local::workbench::controller::reject_imperative_owner;
use crate::commands::local::{
    kubectl_apply_stdin, run_cmd, run_cmd_output, MANAGED_BY_LABEL, PROVIDER_INSTALL_MANAGED_BY,
};
use clap::Args;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Args, Debug)]
pub struct ProviderInstallArgs {
    /// Path to the local Provider source directory (defaults to current directory)
    #[arg(long, conflicts_with = "repo")]
    pub path: Option<String>,

    /// GitHub repository in <org>/<repo> format (for example hops-ops/provider-helm)
    #[arg(long, conflicts_with = "path")]
    pub repo: Option<String>,

    /// Version tag to apply directly from ghcr.io without cloning/building (requires --repo)
    #[arg(long, requires = "repo")]
    pub version: Option<String>,

    /// Set spec.skipDependencyResolution=true on the generated Provider
    #[arg(long)]
    pub skip_dependency_resolution: bool,

    /// Optional prefix prepended to the generated dev tag so the resulting
    /// image tag satisfies a Configuration's SemVer dependency constraint
    /// (e.g. `--version-prefix v1` makes the tag `v1-dev-<sha12>`, allowing
    /// Configurations that depend on `provider-foo (>=v1)` to resolve against
    /// a locally-built dev image). Only applies to source builds (not --repo + --version).
    #[arg(long, conflicts_with = "version")]
    pub version_prefix: Option<String>,

    /// Git branch to check out when cloning a source build from `--repo`.
    /// Useful for installing a fork's WIP branch (e.g. `--repo
    /// jonasz-lasut/provider-helm --branch helm-v4`). Ignored when `--path`
    /// or `--repo + --version` is used.
    #[arg(long, requires = "repo", conflicts_with = "version")]
    pub branch: Option<String>,

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

#[derive(Debug, Deserialize)]
struct PackageMetadataYaml {
    metadata: PackageMetadata,
}

#[derive(Debug, Deserialize)]
struct PackageMetadata {
    name: String,
}

pub fn run(args: &ProviderInstallArgs) -> Result<(), Box<dyn Error>> {
    let provider_selected = args.cluster_provider.is_some() || args.docker_provider.is_some();
    let backend = backend::activate_with_providers(
        args.cluster_provider,
        args.docker_provider,
        args.cluster_name.as_deref(),
        args.context.as_deref(),
    )?;
    reject_imperative_owner(&backend::kind::active_cluster_name())?;

    match (args.repo.as_deref(), args.version.as_deref()) {
        (Some(repo), Some(version)) => {
            apply_repo_version(repo, version, args.skip_dependency_resolution)
        }
        (Some(repo), None) => run_repo_install(
            repo,
            args.skip_dependency_resolution,
            args.version_prefix.as_deref(),
            args.branch.as_deref(),
            backend,
            provider_selected,
            args.context.as_deref(),
        ),
        (None, _) => {
            let path = args.path.as_deref().unwrap_or(".");
            let prefix = args.version_prefix.clone();
            prepare_local_registry(backend, provider_selected, args.context.as_deref())?;
            run_local_path(path, args.skip_dependency_resolution, prefix.as_deref())?;

            if args.watch {
                let path_owned = path.to_string();
                let skip = args.skip_dependency_resolution;
                let prefix_owned = prefix;
                run_watch(path, args.debounce, move || {
                    reject_imperative_owner(&backend::kind::active_cluster_name())?;
                    run_local_path(&path_owned, skip, prefix_owned.as_deref())
                })?;
            }

            Ok(())
        }
    }
}

fn run_repo_install(
    repo: &str,
    skip_dependency_resolution: bool,
    version_prefix: Option<&str>,
    branch: Option<&str>,
    backend: Backend,
    provider_selected: bool,
    context: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let spec = parse_repo_spec(repo)?;
    match resolve_repo_install_target(&spec)? {
        RepoInstallTarget::SourceBuild => {
            let cache_path = ensure_cached_repo_checkout_at(&spec, branch)?;
            prepare_local_registry(backend, provider_selected, context)?;
            run_local_path(
                &cache_path.to_string_lossy(),
                skip_dependency_resolution,
                version_prefix,
            )
        }
        RepoInstallTarget::PublishedVersion(version) => {
            apply_repo_version_spec(&spec, &version, skip_dependency_resolution)
        }
    }
}

fn prepare_local_registry(
    backend: Backend,
    provider_selected: bool,
    context: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    ensure_registry()?;
    backend::wire_local_registry_for_target(backend, provider_selected, context)
}

fn apply_repo_version(
    repo: &str,
    version: &str,
    skip_dependency_resolution: bool,
) -> Result<(), Box<dyn Error>> {
    let spec = parse_repo_spec(repo)?;
    apply_repo_version_spec(&spec, version, skip_dependency_resolution)
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
    let provider_name = format!(
        "{}-{}",
        sanitize_name_component(&spec.org),
        sanitize_name_component(&spec.repo)
    );

    log::info!("Applying Provider '{}' from {}", provider_name, package_ref);
    let providers_json = run_cmd_output(
        "kubectl",
        &["get", "providers.pkg.crossplane.io", "-o", "json"],
    )?;
    let resolved = resolve_provider_target(&provider_name, &providers_json, registry_pull())?;
    // Published install pulls directly from ghcr.io — no ImageConfig rewrite,
    // no local registry push, no runtime-image override.
    apply_provider_resources(
        &provider_name,
        &resolved,
        &package_ref,
        None,
        None,
        skip_dependency_resolution,
    )
}

fn run_local_path(
    path: &str,
    skip_dependency_resolution: bool,
    version_prefix: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", path).into());
    }

    let provider_name = read_provider_name(dir)?;
    log::info!("Provider package name: {}", provider_name);

    // Resolve the existing upstream Provider before building so we can:
    //   1. carry the upstream package URL into the new `spec.package` (Crossplane
    //      records the URL-without-tag as the Lock Source; deps declared as
    //      `xpkg.crossplane.io/.../provider-foo` only resolve when the Lock
    //      Source matches that string exactly — ImageConfig's `rewriteImage`
    //      only affects fetching, not Lock matching).
    //   2. derive the upstream major version for the dev tag so the resulting
    //      `vMAJOR.999.999-dev-<sha>` cleanly satisfies `>=vMAJOR` constraints.
    let providers_json = run_cmd_output(
        "kubectl",
        &["get", "providers.pkg.crossplane.io", "-o", "json"],
    )?;
    let resolved = resolve_provider_target(&provider_name, &providers_json, registry_pull())?;
    let upstream_url_prefix = recover_upstream_url_prefix(&resolved, registry_pull())?;
    let upstream_major = parse_major_version(&resolved.existing_package);

    ensure_build_submodule(dir)?;

    log::info!("Building provider binaries (make build) in {}...", path);
    run_make(dir, "build")?;

    log::info!("Building xpkg (make xpkg.build.{})...", provider_name);
    run_make(dir, &format!("xpkg.build.{}", provider_name))?;

    let arch = docker_arch();
    let xpkg_path = find_xpkg_for_provider(dir, &provider_name, arch)?;
    log::info!("Located xpkg: {}", xpkg_path.display());

    let local_image_path_for_tag = format!("{}/hops-ops/{}", registry_pull(), provider_name);
    let dev_tag = dev_tag_for_file(
        &xpkg_path,
        version_prefix,
        upstream_major,
        &local_image_path_for_tag,
    )?;

    let push_xpkg_ref = format!("{}/hops-ops/{}:{}", registry_push(), provider_name, dev_tag);
    let local_pull_xpkg_path = format!("{}/hops-ops/{}", registry_pull(), provider_name);
    log::info!("Pushing xpkg to {}...", push_xpkg_ref);
    crossplane_xpkg_push(&xpkg_path, &push_xpkg_ref)?;

    let runtime_src = find_runtime_image(&provider_name, arch)?;
    let push_runtime_ref = local_runtime_image_ref(&provider_name, arch, &dev_tag);
    log::info!(
        "Tagging runtime image {} as {}...",
        runtime_src,
        push_runtime_ref
    );
    run_cmd("docker", &["tag", &runtime_src, &push_runtime_ref])?;
    log::info!("Pushing {}...", push_runtime_ref);
    run_cmd("docker", &["push", &push_runtime_ref])?;

    // Apply the Provider with the UPSTREAM URL + dev tag in `spec.package` so
    // Crossplane's dep manager records the upstream URL in the Lock. Fetching
    // is redirected to the local registry via the paired ImageConfig.
    let spec_package = format!("{}:{}", upstream_url_prefix, dev_tag);
    apply_provider_resources(
        &provider_name,
        &resolved,
        &spec_package,
        Some((&upstream_url_prefix, &local_pull_xpkg_path)),
        Some(&push_runtime_ref),
        skip_dependency_resolution,
    )
}

fn read_provider_name(dir: &Path) -> Result<String, Box<dyn Error>> {
    let crossplane_yaml = dir.join("package").join("crossplane.yaml");
    if !crossplane_yaml.is_file() {
        return Err(format!(
            "{} not found; expected Provider package metadata",
            crossplane_yaml.display()
        )
        .into());
    }

    let raw = fs::read_to_string(&crossplane_yaml)?;
    let parsed: PackageMetadataYaml = serde_yaml::from_str(&raw)
        .map_err(|e| format!("failed to parse {}: {}", crossplane_yaml.display(), e))?;

    let name = parsed.metadata.name.trim().to_string();
    if name.is_empty() {
        return Err(format!("{} has no metadata.name", crossplane_yaml.display()).into());
    }
    Ok(name)
}

fn ensure_build_submodule(dir: &Path) -> Result<(), Box<dyn Error>> {
    let build_dir = dir.join("build");
    if build_dir.join("makelib").exists() || build_dir.join("Makefile").exists() {
        return Ok(());
    }

    if !dir.join(".gitmodules").is_file() {
        log::debug!(
            "No .gitmodules in {}; skipping submodule init",
            dir.display()
        );
        return Ok(());
    }

    log::info!("Initializing git submodules in {}...", dir.display());
    let status = Command::new("git")
        .args(["submodule", "update", "--init", "--recursive"])
        .current_dir(dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        return Err(format!("git submodule update exited with {}", status).into());
    }
    Ok(())
}

fn run_make(dir: &Path, target: &str) -> Result<(), Box<dyn Error>> {
    let status = Command::new("make")
        .arg(target)
        .current_dir(dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        return Err(format!("make {} exited with {}", target, status).into());
    }
    Ok(())
}

fn find_xpkg_for_provider(
    dir: &Path,
    provider_name: &str,
    arch: &str,
) -> Result<std::path::PathBuf, Box<dyn Error>> {
    let xpkg_dir = dir
        .join("_output")
        .join("xpkg")
        .join(format!("linux_{}", arch));

    if !xpkg_dir.is_dir() {
        return Err(format!("xpkg output directory {} not found", xpkg_dir.display()).into());
    }

    let prefix = format!("{}-", provider_name);
    let mut candidates: Vec<_> = fs::read_dir(&xpkg_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let p = e.path();
            p.extension().is_some_and(|ext| ext == "xpkg")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
        })
        .map(|e| e.path())
        .collect();

    if candidates.is_empty() {
        return Err(format!("no {}*.xpkg found in {}", prefix, xpkg_dir.display()).into());
    }

    candidates.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
    Ok(candidates.pop().unwrap())
}

fn find_runtime_image(provider_name: &str, arch: &str) -> Result<String, Box<dyn Error>> {
    let suffix = format!("/{}-{}:latest", provider_name, arch);

    let raw = run_cmd_output(
        "docker",
        &["images", "--format", "{{.Repository}}:{{.Tag}}"],
    )?;

    let mut matches: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("build-") && line.ends_with(&suffix))
        .collect();

    matches.sort();
    matches
        .pop()
        .map(str::to_string)
        .ok_or_else(|| format!("no runtime image matching build-*{} found", suffix).into())
}

fn local_runtime_image_ref(provider_name: &str, arch: &str, tag: &str) -> String {
    // Provider runtime pods are pulled by the node runtime, not Crossplane's
    // package manager, so they need the node-pullable local registry address.
    format!(
        "{}/hops-ops/{}-{}:{}",
        registry_push(),
        provider_name,
        arch,
        tag
    )
}

fn crossplane_xpkg_push(xpkg_path: &Path, push_ref: &str) -> Result<(), Box<dyn Error>> {
    let xpkg_str = xpkg_path.to_string_lossy().to_string();
    run_cmd(
        "crossplane",
        &[
            "xpkg",
            "push",
            "-f",
            &xpkg_str,
            push_ref,
            "--insecure-skip-tls-verify",
        ],
    )
}

fn dev_tag_for_file(
    path: &Path,
    version_prefix: Option<&str>,
    upstream_major: Option<u64>,
    image_path: &str,
) -> Result<String, Box<dyn Error>> {
    let mut file = fs::File::open(path)?;
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
    let short = &hex[..12];

    // Explicit --version-prefix (highest precedence):
    //   - full semver (vN.N.N) -> used verbatim (stable release pin)
    //   - bare major (vN)      -> auto-incrementing vN.999.<PATCH> (stable
    //                             SemVer, satisfies `>=vN`, monotonically
    //                             newer each push). Falls back to
    //                             vN.999.999-dev-<sha> if the registry can't
    //                             be queried.
    //   - other prefix         -> "<prefix>-dev-<sha>" (legacy behavior; not
    //                             valid SemVer, but kept for backward compat)
    if let Some(prefix) = version_prefix.filter(|p| !p.is_empty()) {
        if is_full_semver(prefix) {
            return Ok(prefix.to_string());
        }
        if let Some(major) = parse_bare_major(prefix) {
            return Ok(next_incrementing_tag(image_path, major, short));
        }
        return Ok(format!("{}-dev-{}", prefix, short));
    }

    // No --version-prefix: derive the major from the upstream Provider's
    // current tag so the dev tag stays valid SemVer and satisfies the most
    // common `>=vMAJOR` Configuration dep constraints.
    if let Some(major) = upstream_major {
        return Ok(next_incrementing_tag(image_path, major, short));
    }

    // Last resort: a bare dev tag. Won't satisfy `>=vN` SemVer constraints,
    // but never has — keep as-is for hands-off ad-hoc builds.
    Ok(format!("dev-{}", short))
}

/// Produce a stable, monotonically-increasing tag of the form `vN.999.<P>`
/// where `<P>` is one greater than the highest existing patch among tags
/// matching `vN.999.<int>` in the local registry. This keeps tags as valid
/// (non-prerelease) SemVer so they satisfy `>=vN` constraints in Crossplane's
/// dep manager (Masterminds/semver excludes prereleases by default).
///
/// Falls back to `vN.999.999-dev-<sha>` if the registry can't be reached —
/// the dev-sha form preserves traceability and is still informative even if
/// it doesn't satisfy `>=vN` (constraint authors can then add `-0`).
fn next_incrementing_tag(image_path: &str, major: u64, sha_short: &str) -> String {
    match next_local_patch_for_major(image_path, major) {
        Ok(patch) => format!("v{}.999.{}", major, patch),
        Err(err) => {
            log::warn!(
                "Could not enumerate local registry tags for {} to compute next patch ({}); falling back to prerelease tag",
                image_path,
                err
            );
            format!("v{}.999.999-dev-{}", major, sha_short)
        }
    }
}

/// Query the local registry's tag-list endpoint and return the next available
/// patch number for `vMAJOR.999.<patch>`. Starts at 1 when no tags match the
/// pattern. Treats a 404 (image-path-not-yet-pushed) as "start at 1".
fn next_local_patch_for_major(image_path: &str, major: u64) -> Result<u32, Box<dyn Error>> {
    // image_path is the cluster-internal path (e.g.
    // "registry.crossplane-system.svc.cluster.local:5000/hops-ops/provider-helm").
    // The tags-list endpoint is reachable on the host via the NodePort.
    let path = image_path
        .split_once('/')
        .map(|(_, rest)| rest)
        .unwrap_or(image_path);
    let url = format!("https://{}/v2/{}/tags/list", registry_push(), path);
    // Local registry uses a hops-managed self-signed cert; skip TLS verify for tag listing.
    let output = Command::new("curl")
        .args(["-skf", "-o", "-", &url])
        .output()?;
    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        // curl exit 22 → HTTP 4xx (likely 404 because the image hasn't been
        // pushed yet). Treat as "no tags" so the first push gets patch 1.
        if code == 22 {
            return Ok(1);
        }
        return Err(format!(
            "curl exited with code {} fetching {} (stderr: {})",
            code,
            url,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let body = String::from_utf8_lossy(&output.stdout);
    let parsed: TagsListResponse = serde_json::from_str(&body)
        .map_err(|e| format!("failed to parse tags-list JSON: {}: body={}", e, body))?;

    let prefix = format!("v{}.999.", major);
    let max_patch: u32 = parsed
        .tags
        .iter()
        .filter_map(|t| t.strip_prefix(&prefix))
        .filter_map(|s| s.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    Ok(max_patch + 1)
}

#[derive(Debug, Deserialize)]
struct TagsListResponse {
    #[serde(default)]
    tags: Vec<String>,
}

fn is_full_semver(s: &str) -> bool {
    let trimmed = s.strip_prefix('v').unwrap_or(s);
    let parts: Vec<&str> = trimmed.splitn(3, '.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Parse a `vN` (or `N`) bare-major version string, e.g. `v1` -> `Some(1)`.
fn parse_bare_major(s: &str) -> Option<u64> {
    let trimmed = s.strip_prefix('v').unwrap_or(s);
    if trimmed.is_empty() || trimmed.contains('.') || trimmed.contains('-') {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

/// Extract the major version from a full package reference like
/// `xpkg.crossplane.io/.../provider-helm:v1.2.0` -> `Some(1)`.
/// Handles digest-tagged refs (returns None) and tags without `v` prefix.
fn parse_major_version(package_ref: &str) -> Option<u64> {
    let (_, tag) = split_package_ref(package_ref)?;
    if tag.starts_with("sha256:") {
        return None;
    }
    let trimmed = tag.strip_prefix('v').unwrap_or(tag);
    // Stop at the first non-digit so v1, v1.2.3, v1-dev-abc all yield 1.
    let major: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    major.parse::<u64>().ok()
}

/// Split a package reference into `(url_prefix_without_tag, tag)`. Returns
/// `None` when the reference has no `:` (treat as untagged, no split).
/// Distinguishes `registry:5000/path:tag` (split at the last `:`) from
/// `registry:5000/path` (port colon, no tag).
fn split_package_ref(package_ref: &str) -> Option<(&str, &str)> {
    let (prefix, suffix) = package_ref.rsplit_once(':')?;
    // If the suffix contains `/`, the `:` we found is part of a port number
    // earlier in the URL, not a tag separator. Treat as no tag.
    if suffix.contains('/') {
        return None;
    }
    Some((prefix, suffix))
}

/// Find the upstream URL prefix to use as the Provider's `spec.package` URL
/// (without tag). On the first install the existing Provider's `spec.package`
/// IS the upstream URL — we just strip the tag. On re-runs the existing
/// Provider may already have been patched in a previous (broken) install to
/// point at the local registry; in that case we look up a paired ImageConfig
/// whose `rewriteImage.prefix` matches and recover the original upstream URL
/// from its `matchImages.prefix`.
fn recover_upstream_url_prefix(
    resolved: &ResolvedProvider,
    local_registry_host: &str,
) -> Result<String, Box<dyn Error>> {
    let (prefix, _tag) = split_package_ref(&resolved.existing_package).ok_or_else(|| {
        format!(
            "existing Provider '{}' has no tag in spec.package ({}); cannot \
             determine the upstream URL prefix",
            resolved.existing_name, resolved.existing_package
        )
    })?;

    if !prefix.contains(local_registry_host) {
        // Fresh install path — the existing Provider still has the upstream URL.
        return Ok(prefix.to_string());
    }

    // Re-run path — look up an ImageConfig that rewrites to this local prefix.
    let ic_json = run_cmd_output(
        "kubectl",
        &["get", "imageconfig.pkg.crossplane.io", "-o", "json"],
    )?;
    if let Some(upstream) = find_upstream_for_rewrite(&ic_json, prefix)? {
        return Ok(upstream);
    }

    Err(format!(
        "existing Provider '{}' already points at the local registry ({}) and \
         no ImageConfig rewrites to this prefix — delete the Provider and \
         re-apply the upstream manifest, then re-run `hops provider install`.",
        resolved.existing_name, prefix
    )
    .into())
}

fn find_upstream_for_rewrite(
    ic_json: &str,
    local_prefix: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    let trimmed = ic_json.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let parsed: ImageConfigList = serde_json::from_str(trimmed)
        .map_err(|e| format!("failed to parse ImageConfig list JSON: {}", e))?;
    for ic in parsed.items {
        let Some(spec) = ic.spec else { continue };
        let rewrite_prefix = spec
            .rewrite_image
            .as_ref()
            .and_then(|r| r.prefix.as_deref())
            .unwrap_or_default();
        if rewrite_prefix.is_empty() || !local_prefix.starts_with(rewrite_prefix) {
            continue;
        }
        for m in &spec.match_images {
            if let Some(prefix) = m.prefix.as_deref() {
                return Ok(Some(prefix.to_string()));
            }
        }
    }
    Ok(None)
}

#[derive(Debug, Deserialize)]
struct ImageConfigList {
    items: Vec<ImageConfigResource>,
}

#[derive(Debug, Deserialize)]
struct ImageConfigResource {
    spec: Option<ImageConfigSpec>,
}

#[derive(Debug, Deserialize)]
struct ImageConfigSpec {
    #[serde(rename = "matchImages")]
    match_images: Vec<ImageConfigMatch>,
    #[serde(rename = "rewriteImage")]
    rewrite_image: Option<ImageConfigRewrite>,
}

#[derive(Debug, Deserialize)]
struct ImageConfigMatch {
    prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ImageConfigRewrite {
    prefix: Option<String>,
}

fn image_config_name_for(upstream_prefix: &str) -> String {
    // Match the `<host>-<path-with-dashes>` shape used elsewhere for these
    // rewrite ImageConfigs (see config/install.rs::image_config_name).
    let mut s = String::with_capacity(upstream_prefix.len() + 16);
    s.push_str("hops-local-rewrite-");
    for ch in upstream_prefix.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => s.push(ch),
            _ => s.push('-'),
        }
    }
    // Collapse any run of dashes and trim trailing/leading dashes for k8s name
    // hygiene.
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch == '-' {
            if !prev_dash {
                out.push(ch);
            }
            prev_dash = true;
        } else {
            out.push(ch);
            prev_dash = false;
        }
    }
    out.trim_matches('-').to_lowercase()
}

fn build_image_config_yaml(name: &str, match_prefix: &str, rewrite_prefix: &str) -> String {
    format!(
        "apiVersion: pkg.crossplane.io/v1beta1
kind: ImageConfig
metadata:
  name: {name}
spec:
  matchImages:
    - type: Prefix
      prefix: {match_prefix}
  rewriteImage:
    prefix: {rewrite_prefix}
"
    )
}

/// Apply a Provider plus its supporting DeploymentRuntimeConfig and
/// ClusterRoleBinding by patching an existing Provider in place.
///
/// `hops provider install` only patches Providers that already exist in the
/// cluster (e.g. installed via Crossplane bootstrap, GitOps, or kubectl apply).
/// It never bootstraps a new Provider -- a sibling Provider would fight the
/// upstream one for CRD ownership and stay `Healthy=False`. See
/// `resolve_provider_target`.
///
/// When `runtime_image` is provided the DRC overrides the package-runtime
/// container image (used for source builds); otherwise the DRC only sets up a
/// cluster-admin ServiceAccount (used for published versions).
///
/// The DRC and ClusterRoleBinding are always named after the existing Provider
/// (`<provider>-runtime` / `<provider>-cluster-admin`). We deliberately do NOT
/// reuse the existing Provider's `runtimeConfigRef.name` — a different Provider
/// may already reference that DRC, and overwriting a shared DRC with this
/// provider's image silently corrupts the other provider's pod.
fn apply_provider_resources(
    provider_name: &str,
    resolved: &ResolvedProvider,
    spec_package: &str,
    image_config_rewrite: Option<(&str, &str)>,
    runtime_image: Option<&str>,
    skip_dependency_resolution: bool,
) -> Result<(), Box<dyn Error>> {
    if resolved.existing_name != provider_name {
        log::info!(
            "Reusing existing Provider '{}' (matches package substring '{}'); patching in place",
            resolved.existing_name,
            provider_name
        );
    } else {
        log::info!(
            "Reusing existing Provider '{}'; patching in place",
            resolved.existing_name
        );
    }

    let target_name = resolved.existing_name.clone();
    let drc_name = format!("{}-runtime", target_name);
    if let Some(prev) = &resolved.existing_drc_name {
        if prev != &drc_name {
            log::warn!(
                "Provider '{}' previously referenced DRC '{}'; switching to owned DRC '{}'. \
                 The old DRC is not deleted (it may still be referenced by another Provider).",
                target_name,
                prev,
                drc_name
            );
        }
    }
    let sa_name = target_name.clone();
    let crb_name = format!("{}-cluster-admin", target_name);

    // Apply the ImageConfig BEFORE the Provider (for local-build installs) so
    // Crossplane has the rewrite in place when it next reconciles the Provider
    // revision. For ghcr.io published-version installs we skip this entirely
    // since Crossplane pulls upstream directly.
    if let Some((upstream_url_prefix, local_image_path)) = image_config_rewrite {
        let ic_name = image_config_name_for(upstream_url_prefix);
        log::info!(
            "Applying ImageConfig '{}' ({} -> {})...",
            ic_name,
            upstream_url_prefix,
            local_image_path
        );
        kubectl_apply_stdin(&build_image_config_yaml(
            &ic_name,
            upstream_url_prefix,
            local_image_path,
        ))?;
    }

    log::info!("Applying DeploymentRuntimeConfig '{}'...", drc_name);
    kubectl_apply_stdin(&build_runtime_config_yaml(
        &drc_name,
        &sa_name,
        runtime_image,
    ))?;

    log::info!("Applying ClusterRoleBinding '{}'...", crb_name);
    kubectl_apply_stdin(&build_cluster_role_binding_yaml(&crb_name, &sa_name))?;

    log::info!("Applying Provider '{}'...", target_name);
    kubectl_apply_stdin(&build_provider_yaml(
        &target_name,
        spec_package,
        &drc_name,
        skip_dependency_resolution,
    ))?;

    Ok(())
}

/// Outcome of inspecting the cluster for a pre-existing Provider we will patch.
/// Constructed only when exactly one matching Provider is found; the 0- and
/// 2+-match cases are surfaced as errors from `resolve_provider_target`.
#[derive(Debug, PartialEq, Eq)]
struct ResolvedProvider {
    /// Existing Provider's `metadata.name` to patch in place.
    existing_name: String,
    /// Existing Provider's full `spec.package` (URL + tag/digest). Carries the
    /// original upstream URL prefix on the first install; carries a local
    /// registry URL when re-running over a previously patched Provider — we
    /// recover the upstream URL via an ImageConfig lookup in that case.
    existing_package: String,
    /// Existing Provider's `spec.runtimeConfigRef.name`, if any. Reusing it
    /// avoids orphaning the DRC the upstream Provider already references; if
    /// `None` we derive `<existing_name>-runtime` rather than fabricating a
    /// sibling against the requested package name.
    existing_drc_name: Option<String>,
}

/// Locate the existing Provider we should patch in place.
///
/// `hops provider install` is strictly a patch operation: the upstream Provider
/// is expected to already be installed in the cluster (Crossplane bootstrap,
/// GitOps, or `kubectl apply`). We never bootstrap a sibling Provider because a
/// second resource managing the same CRDs would race the existing one and stay
/// `Healthy=False`.
///
/// Pure function over `kubectl get providers.pkg.crossplane.io -o json` output
/// so it is testable.
///
/// Selection rules (mirrors hops-ops/provider-kubernetes-patch-test-env/patch.sh):
/// - First try to find Providers whose `spec.package` contains `provider_name`
///   AND does NOT live in `local_registry_host` (i.e. the upstream package).
/// - If that returns no results, fall back to Providers we previously patched
///   (same substring match but `spec.package` *does* live in the local
///   registry) so re-running the install is idempotent.
/// - Exactly one match in either bucket -> patch it.
/// - Zero matches -> error: refuse to bootstrap a sibling Provider.
/// - Two-or-more upstream matches -> error and ask the human to disambiguate;
///   we can't safely guess which Provider owns these CRDs.
fn resolve_provider_target(
    provider_name: &str,
    providers_json: &str,
    local_registry_host: &str,
) -> Result<ResolvedProvider, Box<dyn Error>> {
    let trimmed = providers_json.trim();
    if trimmed.is_empty() {
        // `kubectl get` returns nothing when the CRD exists but the resource
        // list is empty -- treat as no matches and fail loudly below.
        return Err(no_existing_provider_error(provider_name));
    }

    let parsed: ProviderList = serde_json::from_str(trimmed)
        .map_err(|e| format!("failed to parse providers list JSON: {}", e))?;

    let mut upstream: Vec<ProviderEntry> = Vec::new();
    let mut already_patched: Vec<ProviderEntry> = Vec::new();

    for item in parsed.items.into_iter() {
        let Some(name) = item.metadata.name.clone() else {
            continue;
        };
        let Some(spec) = &item.spec else {
            continue;
        };
        let Some(package) = &spec.package else {
            continue;
        };

        if !package.contains(provider_name) {
            continue;
        }

        let drc_name = spec
            .runtime_config_ref
            .as_ref()
            .and_then(|r| r.name.clone());
        let entry = ProviderEntry {
            name,
            package: package.clone(),
            drc_name,
        };

        if package.contains(local_registry_host) {
            already_patched.push(entry);
        } else {
            upstream.push(entry);
        }
    }

    let pick = if !upstream.is_empty() {
        upstream
    } else {
        already_patched
    };

    match pick.len() {
        0 => Err(no_existing_provider_error(provider_name)),
        1 => {
            let entry = pick.into_iter().next().unwrap();
            Ok(ResolvedProvider {
                existing_name: entry.name,
                existing_package: entry.package,
                existing_drc_name: entry.drc_name,
            })
        }
        _ => {
            let names: Vec<String> = pick
                .iter()
                .map(|p| format!("  - {} (package={})", p.name, p.package))
                .collect();
            Err(format!(
                "multiple existing Providers match '{}'; refusing to guess which to patch:\n{}\n\
                 Delete the unwanted Providers (or extend `hops provider install` with a \
                 --target-provider flag) before retrying.",
                provider_name,
                names.join("\n")
            )
            .into())
        }
    }
}

fn no_existing_provider_error(provider_name: &str) -> Box<dyn Error> {
    format!(
        "no existing Provider matching '{}' found in cluster; install the upstream Provider first \
         (via Crossplane bootstrap, GitOps, or kubectl apply) and re-run. hops provider install \
         patches existing Providers in place -- it does not bootstrap new ones.",
        provider_name
    )
    .into()
}

#[derive(Debug, Deserialize)]
struct ProviderList {
    #[serde(default)]
    items: Vec<ProviderItem>,
}

#[derive(Debug, Deserialize)]
struct ProviderItem {
    #[serde(default)]
    metadata: ProviderItemMetadata,
    spec: Option<ProviderItemSpec>,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderItemMetadata {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProviderItemSpec {
    package: Option<String>,
    #[serde(rename = "runtimeConfigRef")]
    runtime_config_ref: Option<ProviderItemRuntimeConfigRef>,
}

#[derive(Debug, Deserialize)]
struct ProviderItemRuntimeConfigRef {
    name: Option<String>,
}

#[derive(Debug)]
struct ProviderEntry {
    name: String,
    package: String,
    drc_name: Option<String>,
}

fn build_provider_yaml(
    name: &str,
    package_ref: &str,
    runtime_config_name: &str,
    skip_dependency_resolution: bool,
) -> String {
    let mut yaml = format!(
        "apiVersion: pkg.crossplane.io/v1
kind: Provider
metadata:
  name: {name}
  labels:
    {MANAGED_BY_LABEL}: {PROVIDER_INSTALL_MANAGED_BY}
spec:
  package: {package_ref}
  packagePullPolicy: Always
  runtimeConfigRef:
    apiVersion: pkg.crossplane.io/v1beta1
    kind: DeploymentRuntimeConfig
    name: {runtime_config_name}\n"
    );

    if skip_dependency_resolution {
        yaml.push_str("  skipDependencyResolution: true\n");
    }

    yaml
}

fn build_runtime_config_yaml(
    name: &str,
    service_account: &str,
    runtime_image: Option<&str>,
) -> String {
    let mut yaml = format!(
        "apiVersion: pkg.crossplane.io/v1beta1
kind: DeploymentRuntimeConfig
metadata:
  name: {name}
  labels:
    {MANAGED_BY_LABEL}: {PROVIDER_INSTALL_MANAGED_BY}
spec:
  serviceAccountTemplate:
    metadata:
      name: {service_account}\n"
    );

    if let Some(image) = runtime_image {
        yaml.push_str(&format!(
            "  deploymentTemplate:
    spec:
      selector: {{}}
      template:
        spec:
          containers:
          - name: package-runtime
            image: {image}\n"
        ));
    }

    yaml
}

fn build_cluster_role_binding_yaml(name: &str, service_account: &str) -> String {
    format!(
        "apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: {name}
  labels:
    {MANAGED_BY_LABEL}: {PROVIDER_INSTALL_MANAGED_BY}
subjects:
- kind: ServiceAccount
  name: {service_account}
  namespace: crossplane-system
roleRef:
  kind: ClusterRole
  name: cluster-admin
  apiGroup: rbac.authorization.k8s.io
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_provider_yaml_includes_runtime_config_ref() {
        let yaml = build_provider_yaml(
            "provider-helm",
            "registry.example/provider-helm:dev-abc",
            "provider-helm-runtime",
            false,
        );
        assert!(yaml.contains("kind: Provider"));
        assert!(yaml.contains("name: provider-helm"));
        assert!(yaml.contains("package: registry.example/provider-helm:dev-abc"));
        assert!(yaml.contains("name: provider-helm-runtime"));
        assert!(yaml.contains("app.kubernetes.io/managed-by: hops-provider-install"));
        assert!(!yaml.contains("skipDependencyResolution"));
    }

    #[test]
    fn build_provider_yaml_emits_skip_dependency_resolution_when_set() {
        let yaml = build_provider_yaml("p", "r:tag", "p-runtime", true);
        assert!(yaml.contains("skipDependencyResolution: true"));
    }

    #[test]
    fn build_runtime_config_yaml_overrides_image_when_provided() {
        let with_image =
            build_runtime_config_yaml("p-runtime", "p", Some("registry.example/p-arm64:dev-abc"));
        assert!(with_image.contains("name: package-runtime"));
        assert!(with_image.contains("image: registry.example/p-arm64:dev-abc"));

        let without = build_runtime_config_yaml("p-runtime", "p", None);
        assert!(!without.contains("package-runtime"));
        assert!(!without.contains("deploymentTemplate"));
        assert!(without.contains("serviceAccountTemplate"));
        assert!(without.contains("app.kubernetes.io/managed-by: hops-provider-install"));
    }

    #[test]
    fn local_runtime_image_ref_uses_selected_nodeport_registry() {
        let image = local_runtime_image_ref("provider-helm", "arm64", "v1.999.3");
        assert_eq!(
            image,
            format!("{}/hops-ops/provider-helm-arm64:v1.999.3", registry_push())
        );
        assert!(!image.contains(registry_pull()));
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
    fn build_cluster_role_binding_yaml_targets_service_account() {
        let yaml = build_cluster_role_binding_yaml("p-cluster-admin", "p");
        assert!(yaml.contains("kind: ClusterRoleBinding"));
        assert!(yaml.contains("name: p-cluster-admin"));
        assert!(yaml.contains("name: p\n  namespace: crossplane-system"));
        assert!(yaml.contains("name: cluster-admin"));
        assert!(yaml.contains("app.kubernetes.io/managed-by: hops-provider-install"));
    }

    const TEST_LOCAL_REGISTRY: &str = "registry.crossplane-system.svc.cluster.local:5000";

    fn provider_json(items: &[(&str, &str, Option<&str>)]) -> String {
        let entries: Vec<String> = items
            .iter()
            .map(|(name, package, drc)| {
                let drc_block = match drc {
                    Some(d) => format!(
                        ",\"runtimeConfigRef\":{{\"apiVersion\":\"pkg.crossplane.io/v1beta1\",\"kind\":\"DeploymentRuntimeConfig\",\"name\":\"{}\"}}",
                        d
                    ),
                    None => String::new(),
                };
                format!(
                    "{{\"metadata\":{{\"name\":\"{}\"}},\"spec\":{{\"package\":\"{}\"{}}}}}",
                    name, package, drc_block
                )
            })
            .collect();
        format!("{{\"items\":[{}]}}", entries.join(","))
    }

    fn assert_no_existing_provider_error(err: Box<dyn Error>, provider_name: &str) {
        let msg = err.to_string();
        assert!(
            msg.contains(&format!(
                "no existing Provider matching '{}'",
                provider_name
            )),
            "missing 'no existing Provider matching' phrase: {}",
            msg
        );
        assert!(
            msg.contains("install the upstream Provider first"),
            "missing remediation hint: {}",
            msg
        );
        assert!(
            msg.contains("does not bootstrap new ones"),
            "missing scope clarification: {}",
            msg
        );
    }

    #[test]
    fn resolve_provider_target_errors_when_list_is_empty() {
        let err = resolve_provider_target("provider-helm", "{\"items\":[]}", TEST_LOCAL_REGISTRY)
            .expect_err("expected no-existing-Provider error");
        assert_no_existing_provider_error(err, "provider-helm");
    }

    #[test]
    fn resolve_provider_target_errors_on_empty_input() {
        // kubectl returns an empty body when the CRD has no resources; we must
        // still refuse to bootstrap a sibling Provider.
        let err = resolve_provider_target("provider-helm", "", TEST_LOCAL_REGISTRY)
            .expect_err("expected no-existing-Provider error");
        assert_no_existing_provider_error(err, "provider-helm");
    }

    #[test]
    fn resolve_provider_target_picks_upstream_match_and_drc() {
        let json = provider_json(&[
            (
                "crossplane-contrib-provider-helm",
                "xpkg.upbound.io/crossplane-contrib/provider-helm:v1.0.0",
                Some("crossplane-contrib-provider-helm-drc"),
            ),
            (
                "provider-aws-s3",
                "xpkg.upbound.io/upbound/provider-aws-s3:v1.0.0",
                None,
            ),
        ]);

        let resolved =
            resolve_provider_target("provider-helm", &json, TEST_LOCAL_REGISTRY).expect("resolve");
        assert_eq!(
            resolved.existing_name.as_str(),
            "crossplane-contrib-provider-helm"
        );
        assert_eq!(
            resolved.existing_drc_name.as_deref(),
            Some("crossplane-contrib-provider-helm-drc")
        );
    }

    #[test]
    fn resolve_provider_target_errors_when_no_substring_match() {
        let json = provider_json(&[(
            "provider-aws-s3",
            "xpkg.upbound.io/upbound/provider-aws-s3:v1.0.0",
            None,
        )]);

        let err = resolve_provider_target("provider-helm", &json, TEST_LOCAL_REGISTRY)
            .expect_err("expected no-existing-Provider error");
        assert_no_existing_provider_error(err, "provider-helm");
    }

    #[test]
    fn resolve_provider_target_falls_back_to_locally_patched_provider() {
        // Re-run case: the upstream Provider's package has already been
        // rewritten to point at the local registry, so it lives in the
        // "already_patched" bucket. We must still find it instead of erroring.
        let json = provider_json(&[(
            "crossplane-contrib-provider-helm",
            &format!("{}/hops-ops/provider-helm:dev-abc123", TEST_LOCAL_REGISTRY),
            Some("crossplane-contrib-provider-helm-drc"),
        )]);

        let resolved =
            resolve_provider_target("provider-helm", &json, TEST_LOCAL_REGISTRY).expect("resolve");
        assert_eq!(
            resolved.existing_name.as_str(),
            "crossplane-contrib-provider-helm"
        );
        assert_eq!(
            resolved.existing_drc_name.as_deref(),
            Some("crossplane-contrib-provider-helm-drc")
        );
    }

    #[test]
    fn resolve_provider_target_prefers_upstream_when_both_buckets_have_one() {
        // Defensive: an unusual cluster could have both an upstream Provider
        // and a previously-patched one with different metadata.name. Prefer
        // the upstream so we don't strand the original install.
        let json = provider_json(&[
            (
                "old-local-provider-helm",
                &format!("{}/hops-ops/provider-helm:dev-old", TEST_LOCAL_REGISTRY),
                None,
            ),
            (
                "crossplane-contrib-provider-helm",
                "xpkg.upbound.io/crossplane-contrib/provider-helm:v1.0.0",
                None,
            ),
        ]);

        let resolved =
            resolve_provider_target("provider-helm", &json, TEST_LOCAL_REGISTRY).expect("resolve");
        assert_eq!(
            resolved.existing_name.as_str(),
            "crossplane-contrib-provider-helm"
        );
    }

    #[test]
    fn resolve_provider_target_errors_on_multiple_upstream_matches() {
        let json = provider_json(&[
            (
                "crossplane-contrib-provider-helm",
                "xpkg.upbound.io/crossplane-contrib/provider-helm:v1.0.0",
                None,
            ),
            (
                "fork-provider-helm",
                "xpkg.upbound.io/example/provider-helm:v2.0.0",
                None,
            ),
        ]);

        let err = resolve_provider_target("provider-helm", &json, TEST_LOCAL_REGISTRY)
            .expect_err("expected multi-match error");
        let msg = err.to_string();
        assert!(msg.contains("crossplane-contrib-provider-helm"));
        assert!(msg.contains("fork-provider-helm"));
        assert!(msg.contains("multiple existing Providers"));
    }

    #[test]
    fn resolve_provider_target_skips_items_without_package() {
        // An item without `spec.package` (e.g. status-only or malformed) must
        // not crash parsing or be treated as a match.
        let raw = r#"{"items":[
            {"metadata":{"name":"weird"},"spec":{}},
            {"metadata":{"name":"crossplane-contrib-provider-helm"},"spec":{"package":"xpkg.upbound.io/crossplane-contrib/provider-helm:v1.0.0"}}
        ]}"#;
        let resolved =
            resolve_provider_target("provider-helm", raw, TEST_LOCAL_REGISTRY).expect("resolve");
        assert_eq!(
            resolved.existing_name.as_str(),
            "crossplane-contrib-provider-helm"
        );
    }

    #[test]
    fn resolve_provider_target_returns_clear_error_on_invalid_json() {
        let err = resolve_provider_target("provider-helm", "not-json", TEST_LOCAL_REGISTRY)
            .expect_err("expected parse error");
        assert!(err
            .to_string()
            .contains("failed to parse providers list JSON"));
    }
}
