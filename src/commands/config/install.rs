use crate::commands::local::{
    kubectl_apply_stdin, kubectl_command, repo_cache_path, run_cmd, run_cmd_output,
    sync_registry_hosts_entry, HOPS_KUBE_CONTEXT_ENV,
};
use clap::Args;
use flate2::read::GzDecoder;
use notify::{RecursiveMode, Watcher};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Cursor, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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

    /// Force reload from source by recreating ConfigurationRevision(s) before apply (path/repo only)
    #[arg(long, conflicts_with = "version")]
    pub reload: bool,

    /// Set spec.skipDependencyResolution=true on the generated Configuration
    #[arg(long)]
    pub skip_dependency_resolution: bool,

    /// Kubernetes context to use for all kubectl commands (e.g. "colima")
    #[arg(long)]
    pub context: Option<String>,

    /// Watch the project directory for changes and re-run install automatically
    #[arg(long, conflicts_with = "repo")]
    pub watch: bool,

    /// Debounce interval for --watch in seconds (default: 15)
    #[arg(long, requires = "watch", default_value = "15")]
    pub debounce: u64,
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

#[derive(Debug, Deserialize)]
struct KubeList<T> {
    items: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct OwnerReference {
    kind: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RevisionMetadata {
    name: String,
    #[serde(rename = "ownerReferences")]
    owner_references: Option<Vec<OwnerReference>>,
    labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct ConfigurationRevisionResource {
    metadata: RevisionMetadata,
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum RepoInstallTarget {
    SourceBuild,
    PublishedVersion(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepoInstallChoice {
    SourceBuild,
    PublishedVersion,
}

pub fn run(args: &ConfigArgs) -> Result<(), Box<dyn Error>> {
    validate_reload_args(args)?;

    if let Some(ctx) = &args.context {
        std::env::set_var(HOPS_KUBE_CONTEXT_ENV, ctx);
    }

    match (args.repo.as_deref(), args.version.as_deref()) {
        (Some(repo), Some(version)) => {
            apply_repo_version(repo, version, args.skip_dependency_resolution)
        }
        (Some(repo), None) => run_repo_install(repo, args.reload, args.skip_dependency_resolution),
        (None, _) => {
            let path = args.path.as_deref().unwrap_or(".");
            run_local_path(path, args.reload, args.skip_dependency_resolution)?;

            if args.watch {
                run_watch(path, args.reload, args.skip_dependency_resolution, args.debounce)?;
            }

            Ok(())
        }
    }
}

fn should_ignore_path(path: &Path) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s == "_output" || s == ".git" || s == "node_modules" || s == ".cache"
    })
}

fn run_watch(
    path: &str,
    reload: bool,
    skip_dependency_resolution: bool,
    debounce_secs: u64,
) -> Result<(), Box<dyn Error>> {
    let dir = Path::new(path).canonicalize()?;
    let debounce = Duration::from_secs(debounce_secs);

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(event) => {
                let dominated_by_ignored = event.paths.iter().all(|p| should_ignore_path(p));
                log::debug!(
                    "watch event: kind={:?} paths={:?} filtered={}",
                    event.kind,
                    event.paths,
                    dominated_by_ignored,
                );
                if !dominated_by_ignored {
                    let _ = tx.send(());
                }
            }
            Err(e) => log::debug!("watch error: {:?}", e),
        }
    })?;
    watcher.watch(&dir, RecursiveMode::Recursive)?;

    log::info!(
        "Watching {} for changes (debounce {}s, Ctrl+C to stop)...",
        dir.display(),
        debounce_secs,
    );

    loop {
        // Block until the first filesystem event arrives.
        rx.recv().map_err(|_| "watcher channel closed")?;

        // Debounce: wait until no new events arrive for the full debounce window.
        wait_for_quiet(&rx, debounce)?;

        log::info!("──────────────────────────────────────────────");
        log::info!("Change detected, rebuilding...");

        match run_local_path(path, reload, skip_dependency_resolution) {
            Ok(()) => log::info!("Rebuild succeeded."),
            Err(e) => log::error!("Rebuild failed: {}", e),
        }

        log::info!(
            "Watching for changes (debounce {}s, Ctrl+C to stop)...",
            debounce_secs,
        );
    }
}

fn wait_for_quiet(rx: &mpsc::Receiver<()>, debounce: Duration) -> Result<(), Box<dyn Error>> {
    let mut deadline = Instant::now() + debounce;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        match rx.recv_timeout(remaining) {
            Ok(()) => deadline = Instant::now() + debounce,
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("watcher channel closed".into());
            }
        }
    }
}

fn validate_reload_args(args: &ConfigArgs) -> Result<(), Box<dyn Error>> {
    if args.reload && args.version.is_some() {
        return Err("`--reload` can only be used with source builds (`--path` or `--repo` without `--version`)".into());
    }
    Ok(())
}

fn run_repo_install(
    repo: &str,
    reload: bool,
    skip_dependency_resolution: bool,
) -> Result<(), Box<dyn Error>> {
    let spec = parse_repo_spec(repo)?;
    match resolve_repo_install_target(&spec, reload)? {
        RepoInstallTarget::SourceBuild => run_repo_clone(&spec, reload, skip_dependency_resolution),
        RepoInstallTarget::PublishedVersion(version) => {
            apply_repo_version_spec(&spec, &version, skip_dependency_resolution)
        }
    }
}

fn run_repo_clone(
    spec: &RepoSpec,
    reload: bool,
    skip_dependency_resolution: bool,
) -> Result<(), Box<dyn Error>> {
    let cache_path = ensure_cached_repo_checkout(&spec)?;
    run_local_path(
        &cache_path.to_string_lossy(),
        reload,
        skip_dependency_resolution,
    )
}

fn resolve_repo_install_target(
    spec: &RepoSpec,
    reload: bool,
) -> Result<RepoInstallTarget, Box<dyn Error>> {
    if reload || !interactive_stdio_available() {
        return Ok(RepoInstallTarget::SourceBuild);
    }

    match prompt_for_repo_install_choice(spec)? {
        RepoInstallChoice::SourceBuild => Ok(RepoInstallTarget::SourceBuild),
        RepoInstallChoice::PublishedVersion => {
            let suggested = latest_published_version(spec).ok().flatten();
            let version = prompt_for_published_version(spec, suggested.as_deref())?;
            Ok(RepoInstallTarget::PublishedVersion(version))
        }
    }
}

fn interactive_stdio_available() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

fn prompt_for_repo_install_choice(spec: &RepoSpec) -> Result<RepoInstallChoice, Box<dyn Error>> {
    let repo_slug = format!("{}/{}", spec.org, spec.repo);

    loop {
        print!("Install {repo_slug} from source or use a published version? [published/source]: ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        match parse_repo_install_choice(&input) {
            Ok(choice) => return Ok(choice),
            Err(message) => {
                eprintln!("{message}");
            }
        }
    }
}

fn prompt_for_published_version(
    spec: &RepoSpec,
    default_version: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let repo_slug = format!("{}/{}", spec.org, spec.repo);

    loop {
        let prompt = match default_version {
            Some(default) => format!(
                "Enter published version/tag for {repo_slug} [{default}] (for example `pr-<gitsha>`): "
            ),
            None => format!(
                "Enter published version/tag for {repo_slug} (for example `v0.11.0` or `pr-<gitsha>`): "
            ),
        };
        print!("{prompt}");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        match resolve_published_version_input(&input, default_version) {
            Some(version) => return Ok(version),
            None => {
                eprintln!(
                    "Published version cannot be empty. Enter a tag like `v0.11.0` or `pr-<gitsha>`."
                );
            }
        }
    }
}

fn parse_repo_install_choice(input: &str) -> Result<RepoInstallChoice, String> {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "published" | "publish" | "published version" | "version" | "release" | "p" => {
            Ok(RepoInstallChoice::PublishedVersion)
        }
        "source" | "build" | "clone" | "source build" | "s" => Ok(RepoInstallChoice::SourceBuild),
        _ => Err("Enter `published` or `source`.".to_string()),
    }
}

fn resolve_published_version_input(input: &str, default_version: Option<&str>) -> Option<String> {
    let trimmed = input.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.to_string());
    }

    default_version
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(str::to_string)
}

fn latest_published_version(spec: &RepoSpec) -> Result<Option<String>, Box<dyn Error>> {
    let repo_url = format!("https://github.com/{}/{}", spec.org, spec.repo);
    let output = run_cmd_output(
        "git",
        &[
            "ls-remote",
            "--sort=-version:refname",
            "--refs",
            "--tags",
            &repo_url,
        ],
    )?;

    for line in output.lines() {
        let Some((_, ref_name)) = line.split_once('\t') else {
            continue;
        };
        let Some(tag) = ref_name.strip_prefix("refs/tags/") else {
            continue;
        };
        let version = tag.trim();
        if !version.is_empty() {
            return Ok(Some(version.to_string()));
        }
    }

    Ok(None)
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
    let config_name = format!(
        "{}-{}",
        sanitize_name_component(&spec.org),
        sanitize_name_component(&spec.repo)
    );

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

    apply_configuration(
        &config_name,
        &package_ref,
        skip_dependency_resolution,
        false,
    )
}

fn ensure_cached_repo_checkout(spec: &RepoSpec) -> Result<PathBuf, Box<dyn Error>> {
    let cache_path = repo_cache_path(&spec.org, &spec.repo)?;
    let clone_url = format!("https://github.com/{}/{}", spec.org, spec.repo);

    if cache_path.join(".git").is_dir() {
        log::info!("Updating cached repo at {}...", cache_path.display());
        if let Err(err) = refresh_cached_repo(&cache_path) {
            log::warn!(
                "Failed to update cached repo at {}: {}. Re-cloning...",
                cache_path.display(),
                err
            );
            fs::remove_dir_all(&cache_path)?;
            clone_repo_into_cache(&clone_url, &cache_path)?;
        }
        return Ok(cache_path);
    }

    if cache_path.exists() {
        log::warn!(
            "Removing non-git cache directory at {} before cloning...",
            cache_path.display()
        );
        fs::remove_dir_all(&cache_path)?;
    }

    clone_repo_into_cache(&clone_url, &cache_path)?;
    Ok(cache_path)
}

fn clone_repo_into_cache(clone_url: &str, cache_path: &Path) -> Result<(), Box<dyn Error>> {
    let parent = cache_path
        .parent()
        .ok_or("repo cache path has no parent directory")?;
    fs::create_dir_all(parent)?;

    let cache_path_str = cache_path.to_string_lossy().to_string();
    log::info!(
        "Cloning {} into local cache at {}...",
        clone_url,
        cache_path.display()
    );
    run_cmd("git", &["clone", clone_url, &cache_path_str])?;
    Ok(())
}

fn refresh_cached_repo(cache_path: &Path) -> Result<(), Box<dyn Error>> {
    let cache_path_str = cache_path.to_string_lossy().to_string();
    run_cmd(
        "git",
        &["-C", &cache_path_str, "fetch", "--prune", "origin"],
    )?;
    run_cmd("git", &["-C", &cache_path_str, "pull", "--ff-only"])?;
    Ok(())
}

fn apply_repo_version(
    repo: &str,
    version: &str,
    skip_dependency_resolution: bool,
) -> Result<(), Box<dyn Error>> {
    let spec = parse_repo_spec(repo)?;
    apply_repo_version_spec(&spec, version, skip_dependency_resolution)
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

fn run_local_path(
    path: &str,
    reload: bool,
    skip_dependency_resolution: bool,
) -> Result<(), Box<dyn Error>> {
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

        let dev_tag = dev_tag_for_uppkg(&img.uppkg_path)?;
        let push_ref = rewrite_registry_with_tag(&img.source, REGISTRY_PUSH, &dev_tag);
        let pull_ref = rewrite_registry_with_tag(&img.source, REGISTRY_PULL, &dev_tag);
        log::info!(
            "Using local build version '{}' for {}...",
            dev_tag,
            img.source
        );
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
        let path = strip_registry(img_path);
        let name = path.replace('/', "-");
        let existing_package_ref = current_configuration_package_ref(&name)?;
        log_existing_install_replacement(&name, existing_package_ref.as_deref(), pull_ref);

        // Delete inactive ConfigurationRevisions pointing at the remote registry.
        // When switching from a published version to a local build, the old
        // inactive revision's Function dependency has a stale digest that
        // conflicts with the locally-pushed render image.
        delete_remote_registry_config_revisions(&name)?;

        apply_configuration(&name, pull_ref, skip_dependency_resolution, reload)?;
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
    reload: bool,
) -> Result<(), Box<dyn Error>> {
    if reload {
        force_reload_configuration_revisions(name)?;
    }

    log::info!("Applying Configuration '{}'...", name);
    kubectl_apply_stdin(&build_configuration_yaml(
        name,
        package_ref,
        skip_dependency_resolution,
    ))?;
    Ok(())
}

fn force_reload_configuration_revisions(name: &str) -> Result<(), Box<dyn Error>> {
    let revisions = list_configuration_revisions_for(name)?;
    if revisions.is_empty() {
        return Ok(());
    }

    log::info!(
        "Reload requested; deleting {} ConfigurationRevision(s) for '{}' before re-applying...",
        revisions.len(),
        name
    );
    for rev in &revisions {
        run_cmd(
            "kubectl",
            &[
                "delete",
                "configurationrevision.pkg.crossplane.io",
                rev,
                "--ignore-not-found",
            ],
        )?;
    }

    for _ in 0..60 {
        let remaining = list_configuration_revisions_for(name)?;
        if !remaining.is_empty() {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        return Ok(());
    }

    Err(format!(
        "Timed out waiting for ConfigurationRevision deletion for '{}' before reload",
        name
    )
    .into())
}

fn list_configuration_revisions_for(config_name: &str) -> Result<Vec<String>, Box<dyn Error>> {
    let raw = run_cmd_output(
        "kubectl",
        &[
            "get",
            "configurationrevision.pkg.crossplane.io",
            "-o",
            "json",
        ],
    )?;
    let list: KubeList<ConfigurationRevisionResource> = serde_json::from_str(&raw)?;

    let mut revisions = Vec::new();
    for item in list.items {
        if revision_belongs_to_configuration(&item.metadata, config_name) {
            revisions.push(item.metadata.name);
        }
    }

    Ok(revisions)
}

fn revision_belongs_to_configuration(metadata: &RevisionMetadata, config_name: &str) -> bool {
    if metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get("pkg.crossplane.io/package"))
        .map(|v| v == config_name)
        .unwrap_or(false)
    {
        return true;
    }

    metadata
        .owner_references
        .as_ref()
        .map(|owners| {
            owners.iter().any(|owner| {
                owner.kind.as_deref() == Some("Configuration")
                    && owner.name.as_deref() == Some(config_name)
            })
        })
        .unwrap_or(false)
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

fn rewrite_registry_with_tag(image: &str, registry: &str, tag: &str) -> String {
    let (path_with_reg, _) = split_ref(image);
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
        if package.contains(REGISTRY_PULL) && state == "Inactive" {
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
        if !package.contains(REGISTRY_PULL) && state == "Inactive" {
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
    fn parse_repo_install_choice_accepts_expected_inputs() {
        assert_eq!(
            parse_repo_install_choice("published").unwrap(),
            RepoInstallChoice::PublishedVersion
        );
        assert_eq!(
            parse_repo_install_choice("release").unwrap(),
            RepoInstallChoice::PublishedVersion
        );
        assert_eq!(
            parse_repo_install_choice("").unwrap(),
            RepoInstallChoice::PublishedVersion
        );
        assert_eq!(
            parse_repo_install_choice("source").unwrap(),
            RepoInstallChoice::SourceBuild
        );
        assert_eq!(
            parse_repo_install_choice("clone").unwrap(),
            RepoInstallChoice::SourceBuild
        );
    }

    #[test]
    fn parse_repo_install_choice_rejects_unknown_input() {
        assert!(parse_repo_install_choice("banana").is_err());
    }

    #[test]
    fn resolve_published_version_input_prefers_explicit_value() {
        assert_eq!(
            resolve_published_version_input("pr-123abc", Some("v0.11.0")).as_deref(),
            Some("pr-123abc")
        );
    }

    #[test]
    fn resolve_published_version_input_uses_default_for_blank_input() {
        assert_eq!(
            resolve_published_version_input("   ", Some("v0.11.0")).as_deref(),
            Some("v0.11.0")
        );
        assert_eq!(resolve_published_version_input("", None), None);
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

    #[test]
    fn validate_reload_args_rejects_reload_with_version() {
        let args = ConfigArgs {
            path: None,
            repo: Some("hops-ops/helm-certmanager".to_string()),
            version: Some("v0.1.0".to_string()),
            reload: true,
            skip_dependency_resolution: false,
            context: None,
            watch: false,
            debounce: 15,
        };
        assert!(validate_reload_args(&args).is_err());
    }

    #[test]
    fn validate_reload_args_accepts_source_reload() {
        let args = ConfigArgs {
            path: Some("/tmp/project".to_string()),
            repo: None,
            version: None,
            reload: true,
            skip_dependency_resolution: false,
            context: None,
            watch: false,
            debounce: 15,
        };
        assert!(validate_reload_args(&args).is_ok());
    }

    #[test]
    fn revision_belongs_to_configuration_by_label() {
        let metadata = RevisionMetadata {
            name: "cfg-abc".to_string(),
            owner_references: None,
            labels: Some(HashMap::from([(
                "pkg.crossplane.io/package".to_string(),
                "cfg".to_string(),
            )])),
        };
        assert!(revision_belongs_to_configuration(&metadata, "cfg"));
        assert!(!revision_belongs_to_configuration(&metadata, "other"));
    }

    #[test]
    fn revision_belongs_to_configuration_by_owner_reference() {
        let metadata = RevisionMetadata {
            name: "cfg-def".to_string(),
            owner_references: Some(vec![OwnerReference {
                kind: Some("Configuration".to_string()),
                name: Some("cfg".to_string()),
            }]),
            labels: None,
        };
        assert!(revision_belongs_to_configuration(&metadata, "cfg"));
        assert!(!revision_belongs_to_configuration(&metadata, "other"));
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
}
