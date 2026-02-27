use super::{run_cmd, run_cmd_output};
use clap::Args;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::thread;
use std::time::Duration;
use tar::Archive;

#[derive(Args, Debug)]
pub struct UnconfigArgs {
    /// Configuration resource name to remove
    #[arg(long, conflicts_with_all = ["repo", "path"])]
    pub name: Option<String>,

    /// GitHub repository in <org>/<repo> format (derives name as <org>-<repo>)
    #[arg(long, conflicts_with_all = ["name", "path"])]
    pub repo: Option<String>,

    /// Path to an XRD project directory (derives names from _output/*.uppkg)
    #[arg(long, conflicts_with_all = ["name", "repo"])]
    pub path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SourceKey {
    kind: String,
    source: String,
}

#[derive(Debug, Deserialize)]
struct MetadataName {
    name: String,
}

#[derive(Debug, Deserialize)]
struct KubeList<T> {
    items: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct PackageSpec {
    #[serde(rename = "package")]
    package_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PackageResource {
    metadata: MetadataName,
    spec: Option<PackageSpec>,
}

#[derive(Debug, Deserialize)]
struct ImageMatch {
    prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ImageConfigSpec {
    #[serde(rename = "matchImages")]
    match_images: Option<Vec<ImageMatch>>,
}

#[derive(Debug, Deserialize)]
struct ImageConfigResource {
    metadata: MetadataName,
    spec: Option<ImageConfigSpec>,
}

#[derive(Clone, Debug, Deserialize)]
struct LockPackage {
    kind: String,
    name: String,
    source: String,
}

#[derive(Debug, Deserialize)]
struct LockResource {
    packages: Option<Vec<LockPackage>>,
}

#[derive(Debug, Deserialize)]
struct DockerSaveManifestEntry {
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
}

#[derive(Debug)]
struct RepoSpec {
    org: String,
    repo: String,
}

pub fn run(args: &UnconfigArgs) -> Result<(), Box<dyn Error>> {
    let config_names = resolve_configuration_names(args)?;
    if config_names.is_empty() {
        return Err("no target configurations resolved".into());
    }

    let hinted_sources = if let Some(path) = args.path.as_deref() {
        resolve_sources_from_path(path)?
    } else {
        HashSet::new()
    };

    log::info!(
        "Preparing to remove configurations: {}",
        config_names.join(", ")
    );
    let pre_lock = fetch_lock_packages();
    let pre_sources = lock_source_set(&pre_lock);

    delete_configurations(&config_names)?;
    wait_for_configurations_deleted(&config_names)?;

    wait_for_lock_without_configurations(&config_names)?;
    let post_lock = fetch_lock_packages();
    let post_sources = lock_source_set(&post_lock);

    let removed_sources: HashSet<SourceKey> =
        pre_sources.difference(&post_sources).cloned().collect();

    let mut removed_render_sources = HashSet::new();
    for source in &removed_sources {
        if source.kind == "Function" && source.source.contains("_render") {
            removed_render_sources.insert(source.source.clone());
        }
    }

    if removed_sources.is_empty() {
        log::info!(
            "No orphaned package sources detected from lock diff after removing configurations"
        );
    } else {
        prune_packages_for_removed_sources(&removed_sources)?;
    }

    let mut hinted_resource_prunes = 0usize;
    if !hinted_sources.is_empty() {
        hinted_resource_prunes = prune_packages_for_source_hints(&hinted_sources)?;
        if hinted_resource_prunes > 0 {
            log::info!(
                "Pruned {} package resources matching sources derived from --path artifacts",
                hinted_resource_prunes
            );
        }
        for source in &hinted_sources {
            if source.contains("_render") {
                removed_render_sources.insert(source.clone());
            }
        }
    }

    if !removed_render_sources.is_empty() {
        prune_image_configs_for_sources(&removed_render_sources)?;
    }

    log::info!(
        "Removed configurations; lock-diff orphaned sources: {}, path-source package resources pruned: {}",
        removed_sources.len(),
        hinted_resource_prunes
    );
    Ok(())
}

fn resolve_configuration_names(args: &UnconfigArgs) -> Result<Vec<String>, Box<dyn Error>> {
    if let Some(name) = args.name.as_deref() {
        let name = name.trim();
        if name.is_empty() {
            return Err("`--name` cannot be empty".into());
        }
        return Ok(vec![name.to_string()]);
    }

    if let Some(repo) = args.repo.as_deref() {
        let spec = parse_repo_spec(repo)?;
        let name = format!(
            "{}-{}",
            sanitize_name_component(&spec.org),
            sanitize_name_component(&spec.repo)
        );
        return Ok(vec![name]);
    }

    if let Some(path) = args.path.as_deref() {
        return resolve_names_from_path(path);
    }

    Err("pass one of `--name`, `--repo`, or `--path`".into())
}

fn delete_configurations(names: &[String]) -> Result<(), Box<dyn Error>> {
    for name in names {
        log::info!("Deleting Configuration '{}'...", name);
        run_cmd(
            "kubectl",
            &[
                "delete",
                "configuration.pkg.crossplane.io",
                name,
                "--ignore-not-found",
            ],
        )?;
    }
    Ok(())
}

fn wait_for_configurations_deleted(names: &[String]) -> Result<(), Box<dyn Error>> {
    for _ in 0..60 {
        let mut any_exists = false;
        for name in names {
            if run_cmd_output(
                "kubectl",
                &["get", "configuration.pkg.crossplane.io", name, "-o", "name"],
            )
            .is_ok()
            {
                any_exists = true;
                break;
            }
        }

        if !any_exists {
            return Ok(());
        }

        thread::sleep(Duration::from_secs(2));
    }

    Err("timed out waiting for configurations to be deleted".into())
}

fn wait_for_lock_without_configurations(config_names: &[String]) -> Result<(), Box<dyn Error>> {
    for _ in 0..45 {
        let lock = fetch_lock_packages();
        let mut still_present = false;
        for name in config_names {
            let prefix = format!("{}-", name);
            if lock
                .iter()
                .any(|p| p.kind == "Configuration" && p.name.starts_with(&prefix))
            {
                still_present = true;
                break;
            }
        }

        if !still_present {
            return Ok(());
        }

        thread::sleep(Duration::from_secs(2));
    }

    log::warn!("Timed out waiting for lock to drop configuration revisions; continuing cleanup");
    Ok(())
}

fn fetch_lock_packages() -> Vec<LockPackage> {
    let raw = match run_cmd_output(
        "kubectl",
        &["get", "lock.pkg.crossplane.io", "lock", "-o", "json"],
    ) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };

    serde_json::from_str::<LockResource>(&raw)
        .ok()
        .and_then(|l| l.packages)
        .unwrap_or_default()
}

fn lock_source_set(lock: &[LockPackage]) -> HashSet<SourceKey> {
    lock.iter()
        .map(|p| SourceKey {
            kind: p.kind.clone(),
            source: p.source.clone(),
        })
        .collect()
}

fn prune_packages_for_removed_sources(
    removed_sources: &HashSet<SourceKey>,
) -> Result<(), Box<dyn Error>> {
    let mut by_kind: HashMap<&str, HashSet<&str>> = HashMap::new();
    for source in removed_sources {
        by_kind
            .entry(source.kind.as_str())
            .or_default()
            .insert(source.source.as_str());
    }

    prune_resource_group(
        "Configuration",
        "configuration.pkg.crossplane.io",
        "configurationrevision.pkg.crossplane.io",
        by_kind.get("Configuration"),
    )?;
    prune_resource_group(
        "Function",
        "function.pkg.crossplane.io",
        "functionrevision.pkg.crossplane.io",
        by_kind.get("Function"),
    )?;
    prune_resource_group(
        "Provider",
        "provider.pkg.crossplane.io",
        "providerrevision.pkg.crossplane.io",
        by_kind.get("Provider"),
    )?;

    Ok(())
}

fn prune_packages_for_source_hints(sources: &HashSet<String>) -> Result<usize, Box<dyn Error>> {
    if sources.is_empty() {
        return Ok(0);
    }

    let mut deleted = 0usize;
    deleted += delete_resource_by_source("configuration.pkg.crossplane.io", sources)?;
    deleted += delete_resource_by_source("configurationrevision.pkg.crossplane.io", sources)?;
    deleted += delete_resource_by_source("function.pkg.crossplane.io", sources)?;
    deleted += delete_resource_by_source("functionrevision.pkg.crossplane.io", sources)?;
    deleted += delete_resource_by_source("provider.pkg.crossplane.io", sources)?;
    deleted += delete_resource_by_source("providerrevision.pkg.crossplane.io", sources)?;
    Ok(deleted)
}

fn prune_resource_group(
    kind: &str,
    resource: &str,
    revision_resource: &str,
    maybe_sources: Option<&HashSet<&str>>,
) -> Result<(), Box<dyn Error>> {
    let Some(sources) = maybe_sources else {
        return Ok(());
    };
    if sources.is_empty() {
        return Ok(());
    }

    let sources: HashSet<String> = sources.iter().map(|s| s.to_string()).collect();
    let removed_primary = delete_resource_by_source(resource, &sources)?;
    let removed_revisions = delete_resource_by_source(revision_resource, &sources)?;

    if removed_primary > 0 || removed_revisions > 0 {
        log::info!(
            "Pruned orphaned {} package resources: {} primary, {} revisions",
            kind,
            removed_primary,
            removed_revisions
        );
    }

    Ok(())
}

fn delete_resource_by_source(
    resource: &str,
    sources: &HashSet<String>,
) -> Result<usize, Box<dyn Error>> {
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

        if sources.contains(&package_source(&package_ref)) {
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
    }

    Ok(deleted)
}

fn prune_image_configs_for_sources(sources: &HashSet<String>) -> Result<(), Box<dyn Error>> {
    let raw = run_cmd_output(
        "kubectl",
        &["get", "imageconfig.pkg.crossplane.io", "-o", "json"],
    )?;
    let list: KubeList<ImageConfigResource> = serde_json::from_str(&raw)?;

    let mut deleted = 0usize;
    for item in list.items {
        let matches = item
            .spec
            .and_then(|s| s.match_images)
            .map(|matches| {
                matches.into_iter().any(|m| {
                    m.prefix
                        .as_deref()
                        .map(|p| sources.contains(p))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        if matches {
            run_cmd(
                "kubectl",
                &[
                    "delete",
                    "imageconfig.pkg.crossplane.io",
                    &item.metadata.name,
                    "--ignore-not-found",
                ],
            )?;
            deleted += 1;
        }
    }

    if deleted > 0 {
        log::info!("Pruned {} orphaned ImageConfig resource(s)", deleted);
    }

    Ok(())
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

fn resolve_names_from_path(path: &str) -> Result<Vec<String>, Box<dyn Error>> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", path).into());
    }

    let output_dir = dir.join("_output");
    let mut names = HashSet::new();
    for entry in fs::read_dir(&output_dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().map(|e| e == "uppkg").unwrap_or(false) {
            for name in names_from_uppkg_manifest(&path)? {
                names.insert(name);
            }
        }
    }

    if names.is_empty() {
        return Err(format!(
            "no configuration package images found in {}",
            output_dir.display()
        )
        .into());
    }

    let mut names: Vec<String> = names.into_iter().collect();
    names.sort();
    Ok(names)
}

fn resolve_sources_from_path(path: &str) -> Result<HashSet<String>, Box<dyn Error>> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", path).into());
    }

    let output_dir = dir.join("_output");
    let mut sources = HashSet::new();
    for entry in fs::read_dir(&output_dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().map(|e| e == "uppkg").unwrap_or(false) {
            for source in sources_from_uppkg_manifest(&path)? {
                sources.insert(source);
            }
        }
    }

    Ok(sources)
}

fn names_from_uppkg_manifest(uppkg_path: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let manifest_bytes = read_entry_from_tar(uppkg_path, "manifest.json")?;
    let entries: Vec<DockerSaveManifestEntry> = serde_json::from_slice(&manifest_bytes)?;

    let mut names = HashSet::new();
    for entry in entries {
        let Some(tags) = entry.repo_tags else {
            continue;
        };
        for tag in tags {
            let Some(path) = tag.strip_suffix(":configuration") else {
                continue;
            };
            let name = path.rsplit('/').next().unwrap_or(path).trim();
            if !name.is_empty() {
                names.insert(name.to_string());
            }
        }
    }

    let mut names: Vec<String> = names.into_iter().collect();
    names.sort();
    Ok(names)
}

fn sources_from_uppkg_manifest(uppkg_path: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let manifest_bytes = read_entry_from_tar(uppkg_path, "manifest.json")?;
    let entries: Vec<DockerSaveManifestEntry> = serde_json::from_slice(&manifest_bytes)?;

    let mut sources = HashSet::new();
    for entry in entries {
        let Some(tags) = entry.repo_tags else {
            continue;
        };
        for tag in tags {
            let source = package_source(&tag);
            if !source.is_empty() {
                sources.insert(source);
            }
        }
    }

    let mut sources: Vec<String> = sources.into_iter().collect();
    sources.sort();
    Ok(sources)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_source_strips_tag_and_digest() {
        assert_eq!(
            package_source("ghcr.io/hops-ops/aws-auto-eks-cluster:v0.7.0"),
            "ghcr.io/hops-ops/aws-auto-eks-cluster"
        );
        assert_eq!(
            package_source("registry.crossplane-system.svc.cluster.local:5000/hops-ops/stack-aws-observe:configuration"),
            "registry.crossplane-system.svc.cluster.local:5000/hops-ops/stack-aws-observe"
        );
        assert_eq!(
            package_source("ghcr.io/hops-ops/aws-auto-eks-cluster_render@sha256:abc"),
            "ghcr.io/hops-ops/aws-auto-eks-cluster_render"
        );
    }

    #[test]
    fn parse_repo_spec_accepts_slug_and_url() {
        let slug = parse_repo_spec("hops-ops/aws-auto-eks-cluster").unwrap();
        assert_eq!(slug.org, "hops-ops");
        assert_eq!(slug.repo, "aws-auto-eks-cluster");

        let url = parse_repo_spec("https://github.com/hops-ops/aws-auto-eks-cluster.git").unwrap();
        assert_eq!(url.org, "hops-ops");
        assert_eq!(url.repo, "aws-auto-eks-cluster");
    }

    #[test]
    fn sanitize_name_component_normalizes_name() {
        assert_eq!(sanitize_name_component("Hops_Ops"), "hops-ops");
        assert_eq!(sanitize_name_component("aws.auto.eks"), "aws-auto-eks");
        assert_eq!(sanitize_name_component("---"), "xrd");
    }
}
