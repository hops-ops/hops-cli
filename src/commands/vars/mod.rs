mod init;
mod list;
mod sync;

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CONFIG_FILE: &str = ".hops.yaml";
const DEFAULT_VARS_DIR: &str = "vars";
const DEFAULT_GITHUB_SUBDIR: &str = "github";
const DEFAULT_GITHUB_SHARED_SUBDIR: &str = "_shared";

#[derive(Args, Debug)]
pub struct VarsArgs {
    #[command(subcommand)]
    pub command: VarsCommands,
}

#[derive(Subcommand, Debug)]
pub enum VarsCommands {
    /// Initialize repo vars configuration (.hops.yaml + vars/ dir)
    Init(init::InitArgs),
    /// List local and remote vars
    List(list::ListArgs),
    /// Sync vars to the configured target
    Sync(sync::SyncArgs),
}

pub fn run(args: &VarsArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        VarsCommands::Init(init_args) => init::run(init_args),
        VarsCommands::List(list_args) => list::run(list_args),
        VarsCommands::Sync(sync_args) => sync::run(sync_args),
    }
}

// =============================================================================
// Config types — read from `.hops.yaml`. The `vars:` block is independent of
// the `secrets:` block; both can coexist in the same file because serde_yaml
// ignores unknown top-level fields by default.
// =============================================================================

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct RepoConfig {
    #[serde(default)]
    pub vars: VarsConfig,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct VarsConfig {
    pub dir: Option<String>,
    #[serde(default)]
    pub github: GithubVarsConfig,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct GithubVarsConfig {
    pub owner: Option<String>,
    pub path: Option<String>,
    #[serde(default)]
    pub shared: GithubSharedVarsConfig,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct GithubSharedVarsConfig {
    pub path: Option<String>,
    pub repos: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub(crate) struct GithubVarsRuntimeConfig {
    pub owner: Option<String>,
    pub path: String,
    pub shared_path: String,
    pub shared_repos: Vec<String>,
}

pub(crate) fn load_config() -> Result<RepoConfig, Box<dyn Error>> {
    let path = Path::new(CONFIG_FILE);
    if !path.exists() {
        return Ok(RepoConfig::default());
    }
    let content = fs::read_to_string(path)?;
    Ok(serde_yaml::from_str(&content)?)
}

pub(crate) fn configured_vars_dir() -> Result<PathBuf, Box<dyn Error>> {
    let config = load_config()?;
    Ok(PathBuf::from(
        config
            .vars
            .dir
            .unwrap_or_else(|| DEFAULT_VARS_DIR.to_string()),
    ))
}

pub(crate) fn configured_github_settings() -> Result<GithubVarsRuntimeConfig, Box<dyn Error>> {
    let config = load_config()?;
    Ok(GithubVarsRuntimeConfig {
        owner: config.vars.github.owner,
        path: config
            .vars
            .github
            .path
            .unwrap_or_else(|| DEFAULT_GITHUB_SUBDIR.to_string()),
        shared_path: config
            .vars
            .github
            .shared
            .path
            .unwrap_or_else(|| DEFAULT_GITHUB_SHARED_SUBDIR.to_string()),
        shared_repos: config.vars.github.shared.repos.unwrap_or_default(),
    })
}

// =============================================================================
// Shared shell helpers
// =============================================================================

pub(crate) fn require_command(program: &str) -> Result<(), Box<dyn Error>> {
    let status = Command::new("sh")
        .args(["-c", &format!("command -v {} >/dev/null 2>&1", program)])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Required command not found in PATH: {}", program).into())
    }
}

pub(crate) fn run_command_output(
    program: &str,
    args: &[&str],
) -> Result<Vec<u8>, Box<dyn Error>> {
    log::debug!("Running: {} {}", program, args.join(" "));
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{} exited with {}: {}", program, output.status, stderr).into());
    }
    Ok(output.stdout)
}

pub(crate) fn run_command_output_string(
    program: &str,
    args: &[&str],
) -> Result<String, Box<dyn Error>> {
    Ok(String::from_utf8(run_command_output(program, args)?)?)
}

// =============================================================================
// File → variable name/value collection.
//
// One file per variable: filename is the variable name (normalized to
// uppercase + underscores), file contents (trimmed) are the value. No JSON or
// dotenv support in v1 — keep the surface narrow until a real use case shows up.
// =============================================================================

pub(crate) fn collect_dir_vars(
    root: &Path,
) -> Result<Vec<(String, String, String)>, Box<dyn Error>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    if !root.is_dir() {
        return Err(format!("expected directory: {}", root.display()).into());
    }
    let mut out = Vec::new();
    walk_dir(root, root, &mut out)?;
    Ok(out)
}

fn walk_dir(
    root: &Path,
    current: &Path,
    out: &mut Vec<(String, String, String)>,
) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(current)? {
        let path = entry?.path();
        if path.is_dir() {
            walk_dir(root, &path, out)?;
        } else if path.is_file() {
            let name = var_name_from_path(root, &path)?;
            let value = fs::read_to_string(&path)?.trim().to_string();
            out.push((name, value, path.display().to_string()));
        }
    }
    Ok(())
}

fn var_name_from_path(root: &Path, path: &Path) -> Result<String, Box<dyn Error>> {
    let relative = path.strip_prefix(root)?;
    let raw = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("__");
    Ok(normalize_var_name(&raw))
}

pub(crate) fn normalize_var_name(value: &str) -> String {
    let mut out = String::new();
    let mut prev_underscore = false;
    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_uppercase()
        } else {
            '_'
        };
        if mapped == '_' {
            if !prev_underscore {
                out.push(mapped);
            }
            prev_underscore = true;
        } else {
            out.push(mapped);
            prev_underscore = false;
        }
    }
    out.trim_matches('_').to_string()
}
