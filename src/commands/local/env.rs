//! `hops local env` — catalog discover / list / enable / disable.

use super::gitops::{self, EnvironmentArgs};
use super::local_state_dir;
use super::workbench::definition::{ClusterOverrides, DEFAULT_ENVIRONMENT_FILE};
use super::workbench::machine::{self, DEFAULT_MACHINE_CLUSTER_NAME};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const CATALOG_DIR: &str = "catalog";

#[derive(Args, Debug)]
pub struct EnvArgs {
    #[command(subcommand)]
    pub command: EnvCommands,
}

#[derive(Subcommand, Debug)]
pub enum EnvCommands {
    /// Copy discovered Environment documents into `~/.hops/local/catalog` (off)
    Discover(DiscoverArgs),
    /// List catalogued Environments
    List,
    /// Reconcile a catalogued Environment
    Enable(NameArgs),
    /// Unregister and prune a catalogued Environment
    Disable(NameArgs),
}

#[derive(Args, Debug)]
pub struct DiscoverArgs {
    /// Root to scan. Defaults to cwd. `$HOME` is rejected.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct NameArgs {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    /// Unique catalog id derived from the Environment file path.
    #[serde(default)]
    pub id: String,
    /// Template `metadata.name` (may repeat across worktrees).
    pub name: String,
    /// Runtime / enable name (checkout or worktree label). Unique per source.
    #[serde(default)]
    pub runtime_name: String,
    pub source: PathBuf,
    pub enabled: bool,
    #[serde(default)]
    pub last_error: Option<String>,
}

pub fn run(args: &EnvArgs, overrides: ClusterOverrides<'_>) -> Result<(), Box<dyn Error>> {
    match &args.command {
        EnvCommands::Discover(discover) => discover_into_catalog(discover),
        EnvCommands::List => list_catalog(),
        EnvCommands::Enable(name) => set_enabled(&name.name, true, overrides),
        EnvCommands::Disable(name) => set_enabled(&name.name, false, overrides),
    }
}

pub fn catalog_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(CATALOG_DIR)
}

pub fn load_entries(state_dir: &Path) -> Result<Vec<CatalogEntry>, Box<dyn Error>> {
    let dir = catalog_dir(state_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<CatalogEntry> = Vec::new();
    for file in fs::read_dir(&dir)? {
        let file = file?;
        let path = file.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(&path)?;
        let mut entry: CatalogEntry = serde_json::from_str(&raw)?;
        fill_identity(&mut entry);
        entries.push(entry);
    }
    entries.sort_by(|a, b| a.runtime_name.cmp(&b.runtime_name));
    Ok(entries)
}

pub fn save_entry(state_dir: &Path, entry: &CatalogEntry) -> Result<(), Box<dyn Error>> {
    let dir = catalog_dir(state_dir);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", slug(&entry.id)));
    fs::write(path, serde_json::to_string_pretty(entry)?)?;
    Ok(())
}

fn fill_identity(entry: &mut CatalogEntry) {
    if entry.id.is_empty() {
        entry.id = catalog_id(&entry.source);
    }
    if entry.runtime_name.is_empty() {
        entry.runtime_name = runtime_name_for_source(&entry.source);
    }
}

fn discover_into_catalog(args: &DiscoverArgs) -> Result<(), Box<dyn Error>> {
    let root = match &args.path {
        Some(path) => path.clone(),
        None => std::env::current_dir()?,
    };
    let root = root.canonicalize().unwrap_or(root);
    reject_home_crawl(&root)?;
    let state_dir = local_state_dir()?;
    let mut found = Vec::new();
    walk_gitops(&root, 0, 6, &mut found)?;
    if found.is_empty() {
        println!("No Environment documents found under {}", root.display());
        return Ok(());
    }
    for source in found {
        let template_name = environment_name_from_file(&source).unwrap_or_else(|| {
            source
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .unwrap_or("environment")
                .to_string()
        });
        let mut entry = CatalogEntry {
            id: catalog_id(&source),
            name: template_name,
            runtime_name: runtime_name_for_source(&source),
            source: source.clone(),
            enabled: false,
            last_error: None,
        };
        if let Some(existing) = load_entries(&state_dir)?
            .into_iter()
            .find(|item| item.source == entry.source)
        {
            entry.enabled = existing.enabled;
            entry.last_error = existing.last_error;
        }
        save_entry(&state_dir, &entry)?;
        println!(
            "catalogued {} [{}] ({}) enabled={}",
            entry.runtime_name,
            entry.name,
            entry.source.display(),
            entry.enabled
        );
    }
    Ok(())
}

fn list_catalog() -> Result<(), Box<dyn Error>> {
    let entries = load_entries(&local_state_dir()?)?;
    if entries.is_empty() {
        println!("No catalogued Environments. Run `hops local env discover`.");
        return Ok(());
    }
    for entry in entries {
        let flag = if entry.enabled { "on " } else { "off" };
        println!(
            "[{flag}] {:<40} {}",
            entry.runtime_name,
            entry.source.display()
        );
    }
    Ok(())
}

fn set_enabled(
    name: &str,
    enabled: bool,
    overrides: ClusterOverrides<'_>,
) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    let mut entries = load_entries(&state_dir)?;
    let index = resolve_catalog_index(&entries, name)?;
    let machine = machine::load(&state_dir)?;
    let machine_name = machine
        .as_ref()
        .map(|record| record.name.as_str())
        .unwrap_or(DEFAULT_MACHINE_CLUSTER_NAME);
    let overrides = ClusterOverrides {
        machine_name: Some(machine_name),
        ..overrides
    };
    let entry = &mut entries[index];
    if !enabled && !entry.enabled {
        println!("already disabled {}", entry.runtime_name);
        return Ok(());
    }
    let env_args = EnvironmentArgs {
        path: Some(entry.source.clone()),
        down: !enabled,
        namespace: None,
        name: Some(entry.runtime_name.clone()),
        once: true,
        watch: false,
        debounce: 1,
        dry_run: false,
    };
    let result = gitops::run_environment_command(
        &gitops::GitopsArgs {
            command: gitops::GitopsCommands::Environment(env_args),
        },
        overrides,
    );
    match result {
        Ok(()) => {
            entry.enabled = enabled;
            entry.last_error = None;
            save_entry(&state_dir, entry)?;
            println!(
                "{} {}",
                if enabled { "enabled" } else { "disabled" },
                entry.runtime_name
            );
            Ok(())
        }
        Err(error) => {
            entry.last_error = Some(error.to_string());
            save_entry(&state_dir, entry)?;
            Err(error)
        }
    }
}

fn reject_home_crawl(root: &Path) -> Result<(), Box<dyn Error>> {
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        if let (Ok(root), Ok(home)) = (root.canonicalize(), home.canonicalize()) {
            if root == home {
                return Err(
                    "refusing to crawl $HOME; pass an explicit project or meta root".into(),
                );
            }
        }
    }
    Ok(())
}

fn walk_gitops(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    found: &mut Vec<PathBuf>,
) -> io::Result<()> {
    if depth > max_depth {
        return Ok(());
    }
    let skip = matches!(
        dir.file_name().and_then(|name| name.to_str()),
        Some(".git" | "node_modules" | "target" | ".kb")
    );
    if skip && depth > 0 {
        return Ok(());
    }
    for name in [DEFAULT_ENVIRONMENT_FILE, ".gitops/local/platform.yaml"] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            found.push(candidate);
        }
    }
    if depth == max_depth {
        return Ok(());
    }
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(_) => return Ok(()),
    };
    for child in read.flatten() {
        let path = child.path();
        if path.is_dir() {
            walk_gitops(&path, depth + 1, max_depth, found)?;
        }
    }
    Ok(())
}

fn environment_name_from_file(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let value: serde_yaml::Value = serde_yaml::from_str(&raw).ok()?;
    value
        .get("metadata")?
        .get("name")?
        .as_str()
        .map(ToOwned::to_owned)
}

fn resolve_catalog_index(entries: &[CatalogEntry], query: &str) -> Result<usize, Box<dyn Error>> {
    let exact: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.runtime_name == query || entry.id == query || entry.source.as_os_str() == query
        })
        .map(|(index, _)| index)
        .collect();
    if exact.len() == 1 {
        return Ok(exact[0]);
    }
    if exact.len() > 1 {
        return Err(format!(
            "Environment {query:?} is ambiguous; use a runtime name from `hops local env list`"
        )
        .into());
    }
    let by_template: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.name == query)
        .map(|(index, _)| index)
        .collect();
    match by_template.as_slice() {
        [index] => Ok(*index),
        [] => Err(format!(
            "Environment {query:?} is not in the catalog; run `hops local env discover`"
        )
        .into()),
        _ => Err(format!(
            "template name {query:?} matches multiple Environments ({}); enable the runtime name from `hops local env list`",
            by_template
                .iter()
                .map(|index| entries[*index].runtime_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into()),
    }
}

fn catalog_id(source: &Path) -> String {
    slug(&source.to_string_lossy())
}

fn runtime_name_for_source(source: &Path) -> String {
    let checkout = source.ancestors().nth(3).unwrap_or(source);
    if let Some(label) = worktree_label(checkout) {
        return slug(&label);
    }
    checkout
        .file_name()
        .and_then(|name| name.to_str())
        .map(slug)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "environment".to_string())
}

fn worktree_label(checkout: &Path) -> Option<String> {
    let mut parts = Vec::new();
    let mut seen = false;
    for component in checkout.components() {
        let name = component.as_os_str();
        if seen {
            parts.push(name.to_string_lossy().into_owned());
        }
        if name == ".worktrees" {
            seen = true;
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("-"))
    }
}

fn slug(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    slug.trim_matches('-').to_string()
}

pub fn print_status_card(state_dir: &Path) -> io::Result<()> {
    let machine = machine::load(state_dir).ok().flatten();
    match machine {
        Some(record) => writeln!(
            io::stdout(),
            "Cluster {} ({}) source={}",
            record.name,
            record.kube_context,
            record.source.display()
        )?,
        None => writeln!(io::stdout(), "Cluster: (none) — run `hops local up`")?,
    }
    Ok(())
}
