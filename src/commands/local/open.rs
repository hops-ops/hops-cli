//! `hops local open` — open the primary UI URL in a browser when possible.

use super::workbench::net::{discover_workspace_endpoints, plan_host_access};
use super::workbench::registry::{list_workspaces, load_workspace};
use super::{command_exists, local_state_dir, run_cmd};
use clap::Args;
use std::error::Error;

#[derive(Args, Debug)]
pub struct OpenArgs {
    /// Workspace name (default: only workspace if exactly one).
    #[arg(long)]
    pub name: Option<String>,

    /// Service to open (default: first *ui* service, else first service).
    #[arg(long)]
    pub service: Option<String>,
}

pub fn run(args: &OpenArgs) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    let ws = match &args.name {
        Some(n) => load_workspace(&state_dir, n)?.ok_or_else(|| {
            format!("Workspace `{n}` not found. Run hops local up first.")
        })?,
        None => {
            let all = list_workspaces(&state_dir)?;
            match all.as_slice() {
                [only] => only.clone(),
                [] => {
                    return Err(
                        "No workspaces registered. Run hops local up <env-path> first.".into(),
                    )
                }
                many => {
                    return Err(format!(
                        "Multiple workspaces ({}); pass --name.",
                        many.iter()
                            .map(|w| w.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                    .into())
                }
            }
        }
    };

    let services = discover_workspace_endpoints(&ws.namespace).unwrap_or_default();
    let plan = plan_host_access(&ws.namespace, &services);

    let url = pick_url(&plan.urls, args.service.as_deref()).ok_or_else(|| {
        "No service URL available. Is the workspace up? Try hops local status.".to_string()
    })?;

    println!("Opening {url}");
    open_browser(&url)?;
    Ok(())
}

fn pick_url(
    urls: &std::collections::BTreeMap<String, String>,
    service: Option<&str>,
) -> Option<String> {
    if let Some(svc) = service {
        // Accept bare name, or ns/name key.
        if let Some(u) = urls.get(svc) {
            return Some(u.clone());
        }
        for (key, url) in urls {
            if key == svc || key.ends_with(&format!("/{svc}")) || key.contains(svc) {
                return Some(url.clone());
            }
        }
        return None;
    }
    // Prefer UI-ish names in the workspace namespace first.
    for (name, url) in urls {
        if name.contains("ui") && !name.contains("login") {
            return Some(url.clone());
        }
    }
    urls.values().next().cloned()
}

fn open_browser(url: &str) -> Result<(), Box<dyn Error>> {
    // macOS open, Linux xdg-open; fall back to printing.
    if cfg!(target_os = "macos") {
        match run_cmd("open", &[url]) {
            Ok(()) => return Ok(()),
            Err(e) => log::warn!("open failed: {e}"),
        }
    } else if command_exists("xdg-open") {
        match run_cmd("xdg-open", &[url]) {
            Ok(()) => return Ok(()),
            Err(e) => log::warn!("xdg-open failed: {e}"),
        }
    }
    println!("Open this URL in your browser: {url}");
    Ok(())
}


