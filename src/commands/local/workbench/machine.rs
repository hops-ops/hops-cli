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
