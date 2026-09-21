//! Machine-level Cluster identity persisted under `~/.hops/local/`.
//!
//! `hops local up` uses this record to reconnect instead of creating a second
//! kind cluster from a leaf `.gitops/local/cluster.yaml`.

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_MACHINE_CLUSTER_NAME: &str = "hops";
const RECORD_FILE: &str = "cluster.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineClusterRecord {
    pub name: String,
    pub kube_context: String,
    pub source: PathBuf,
    /// Host directory bind-mounted into the kind node (Cluster.spec.mountRoot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_domain: Option<String>,
}

pub fn record_path(state_dir: &Path) -> PathBuf {
    state_dir.join(RECORD_FILE)
}

pub fn load(state_dir: &Path) -> Result<Option<MachineClusterRecord>, Box<dyn Error>> {
    let path = record_path(state_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let record: MachineClusterRecord =
        serde_json::from_str(&raw).map_err(|error| format!("parse {}: {error}", path.display()))?;
    if record.name.trim().is_empty() {
        return Err(format!("{}: Cluster name must not be empty", path.display()).into());
    }
    Ok(Some(record))
}

pub fn save(state_dir: &Path, record: &MachineClusterRecord) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(state_dir)?;
    let path = record_path(state_dir);
    let raw = serde_json::to_string_pretty(record)?;
    fs::write(&path, raw).map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(())
}

pub fn kube_context_for_name(name: &str) -> String {
    format!("kind-{name}")
}

/// Prefer `~/dev` when it exists; otherwise `$HOME`.
pub fn default_host_path() -> PathBuf {
    let home = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    let dev = home.join("dev");
    if dev.is_dir() {
        dev
    } else {
        home
    }
}

pub fn prompt_host_path(default: &Path) -> Result<PathBuf, Box<dyn Error>> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(default.to_path_buf());
    }
    let raw: String = dialoguer::Input::new()
        .with_prompt("Directory to mount into the cluster (hostPath)")
        .default(default.display().to_string())
        .interact_text()?;
    expand_host_path(&raw)
}

pub fn expand_host_path(raw: &str) -> Result<PathBuf, Box<dyn Error>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("hostPath must not be empty".into());
    }
    let path = if trimmed == "$HOME" || trimmed == "~" {
        PathBuf::from(std::env::var("HOME")?)
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        PathBuf::from(std::env::var("HOME")?).join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    if !path.exists() {
        return Err(format!("hostPath does not exist: {}", path.display()).into());
    }
    if !path.is_dir() {
        return Err(format!("hostPath is not a directory: {}", path.display()).into());
    }
    Ok(path.canonicalize().unwrap_or(path))
}
