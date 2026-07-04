use super::{kubectl_apply_stdin, repo_cache_path, run_cmd, run_cmd_output};
use notify::{RecursiveMode, Watcher};
use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REGISTRY_YAML: &str = include_str!("../../../bootstrap/registry/registry.yaml");

/// Host address for `docker push` (NodePort exposed by the in-cluster registry)
pub const REGISTRY_PUSH: &str = "localhost:30500";

/// Cluster-internal address used in Crossplane package references
pub const REGISTRY_PULL: &str = "registry.crossplane-system.svc.cluster.local:5000";
pub const REGISTRY_HOSTNAME: &str = "registry.crossplane-system.svc.cluster.local";

#[derive(Clone, Debug)]
pub struct RepoSpec {
    pub org: String,
    pub repo: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoInstallTarget {
    SourceBuild,
    PublishedVersion(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoInstallChoice {
    SourceBuild,
    PublishedVersion,
}

pub fn parse_repo_spec(repo: &str) -> Result<RepoSpec, Box<dyn Error>> {
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

pub fn sanitize_name_component(input: &str) -> String {
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

/// Map Rust arch constant to Docker platform architecture name.
pub fn docker_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

pub fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn short_hash(input: &str) -> String {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    let hex = format!("{:016x}", hasher.finish());
    hex[..8].to_string()
}

/// Split "path:tag" into ("path", "tag").
pub fn split_ref(image: &str) -> (&str, &str) {
    image.rsplit_once(':').unwrap_or((image, "latest"))
}

/// Strip the registry prefix from an image path.
pub fn strip_registry(path: &str) -> &str {
    if let Some(pos) = path.find('/') {
        let prefix = &path[..pos];
        if prefix.contains('.') || prefix.contains(':') {
            return &path[pos + 1..];
        }
    }
    path
}

/// Replace the registry portion of an image reference, preserving the tag.
pub fn rewrite_registry(image: &str, registry: &str) -> String {
    let (path_with_reg, tag) = split_ref(image);
    let path = strip_registry(path_with_reg);
    format!("{}/{}:{}", registry, path, tag)
}

/// Replace both the registry and tag of an image reference.
pub fn rewrite_registry_with_tag(image: &str, registry: &str, tag: &str) -> String {
    let (path_with_reg, _) = split_ref(image);
    let path = strip_registry(path_with_reg);
    format!("{}/{}:{}", registry, path, tag)
}

pub fn parse_docker_push_digest(output: &str) -> Option<String> {
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

pub fn image_config_name(source: &str) -> String {
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

/// Ensure the in-cluster registry is deployed and available.
pub fn ensure_registry() -> Result<(), Box<dyn Error>> {
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

pub fn interactive_stdio_available() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

pub fn parse_repo_install_choice(input: &str) -> Result<RepoInstallChoice, String> {
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "published" | "publish" | "published version" | "version" | "release" | "p" => {
            Ok(RepoInstallChoice::PublishedVersion)
        }
        "source" | "build" | "clone" | "source build" | "s" => Ok(RepoInstallChoice::SourceBuild),
        _ => Err("Enter `published` or `source`.".to_string()),
    }
}

pub fn resolve_published_version_input(
    input: &str,
    default_version: Option<&str>,
) -> Option<String> {
    let trimmed = input.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.to_string());
    }

    default_version
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(str::to_string)
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

pub fn latest_published_version(spec: &RepoSpec) -> Result<Option<String>, Box<dyn Error>> {
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

pub fn resolve_repo_install_target(spec: &RepoSpec) -> Result<RepoInstallTarget, Box<dyn Error>> {
    if !interactive_stdio_available() {
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

pub fn ensure_cached_repo_checkout(spec: &RepoSpec) -> Result<PathBuf, Box<dyn Error>> {
    ensure_cached_repo_checkout_at(spec, None)
}

/// Clone or refresh the cached checkout of `spec.org/spec.repo`. When `branch`
/// is `Some`, the cache is checked out at that branch (initial clone uses
/// `--branch`; existing checkouts do a `fetch + checkout + reset --hard`).
/// When `None`, the default branch is used.
pub fn ensure_cached_repo_checkout_at(
    spec: &RepoSpec,
    branch: Option<&str>,
) -> Result<PathBuf, Box<dyn Error>> {
    let cache_path = repo_cache_path(&spec.org, &spec.repo)?;
    let clone_url = format!("https://github.com/{}/{}", spec.org, spec.repo);

    if cache_path.join(".git").is_dir() {
        log::info!("Updating cached repo at {}...", cache_path.display());
        if let Err(err) = refresh_cached_repo(&cache_path, branch) {
            log::warn!(
                "Failed to update cached repo at {}: {}. Re-cloning...",
                cache_path.display(),
                err
            );
            fs::remove_dir_all(&cache_path)?;
            clone_repo_into_cache(&clone_url, &cache_path, branch)?;
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

    clone_repo_into_cache(&clone_url, &cache_path, branch)?;
    Ok(cache_path)
}

fn clone_repo_into_cache(
    clone_url: &str,
    cache_path: &Path,
    branch: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let parent = cache_path
        .parent()
        .ok_or("repo cache path has no parent directory")?;
    fs::create_dir_all(parent)?;

    let cache_path_str = cache_path.to_string_lossy().to_string();
    log::info!(
        "Cloning {} into local cache at {}{}...",
        clone_url,
        cache_path.display(),
        branch
            .map(|b| format!(" (branch {})", b))
            .unwrap_or_default()
    );
    let mut args: Vec<&str> = vec!["clone"];
    if let Some(b) = branch {
        args.extend_from_slice(&["--branch", b]);
    }
    args.extend_from_slice(&[clone_url, &cache_path_str]);
    run_cmd("git", &args)?;
    Ok(())
}

fn refresh_cached_repo(cache_path: &Path, branch: Option<&str>) -> Result<(), Box<dyn Error>> {
    let cache_path_str = cache_path.to_string_lossy().to_string();
    run_cmd(
        "git",
        &["-C", &cache_path_str, "fetch", "--prune", "origin"],
    )?;
    if let Some(b) = branch {
        // For an explicit branch we do a hard reset to the remote tip. The
        // cache is a non-developer checkout; preserving local divergence isn't
        // a concern and `pull --ff-only` against a branch we haven't tracked
        // yet would fail.
        run_cmd("git", &["-C", &cache_path_str, "checkout", b])?;
        run_cmd(
            "git",
            &[
                "-C",
                &cache_path_str,
                "reset",
                "--hard",
                &format!("origin/{}", b),
            ],
        )?;
    } else {
        run_cmd("git", &["-C", &cache_path_str, "pull", "--ff-only"])?;
    }
    Ok(())
}

fn should_ignore_path(path: &Path) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s == "_output" || s == ".git" || s == "node_modules" || s == ".cache"
    })
}

/// Watch `path` for filesystem events and invoke `rebuild` after a debounced
/// quiet period. Loops until the watcher channel closes (typically Ctrl+C).
pub fn run_watch<F>(path: &str, debounce_secs: u64, mut rebuild: F) -> Result<(), Box<dyn Error>>
where
    F: FnMut() -> Result<(), Box<dyn Error>>,
{
    let dir = Path::new(path).canonicalize()?;
    let debounce = Duration::from_secs(debounce_secs);

    let (tx, rx) = mpsc::channel();
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
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
        })?;
    watcher.watch(&dir, RecursiveMode::Recursive)?;

    log::info!(
        "Watching {} for changes (debounce {}s, Ctrl+C to stop)...",
        dir.display(),
        debounce_secs,
    );

    loop {
        rx.recv().map_err(|_| "watcher channel closed")?;
        wait_for_quiet(&rx, debounce)?;

        log::info!("──────────────────────────────────────────────");
        log::info!("Change detected, rebuilding...");

        match rebuild() {
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
}
