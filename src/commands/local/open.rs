//! `hops local open` — open the primary UI URL in a browser when possible.

use super::workbench::net::{plan_host_access, ServiceEndpoint};
use super::workbench::registry::{list_workspaces, load_workspace};
use super::{command_exists, local_state_dir, run_cmd, run_cmd_output};
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

    let services = discover_services(&ws.namespace).unwrap_or_default();
    let kubefwd = command_exists("kubefwd");
    let prefer_kubefwd = match ws.host_access_mode.as_deref() {
        Some("map") => false,
        _ => kubefwd,
    };
    let plan = plan_host_access(
        &ws.namespace,
        &services,
        prefer_kubefwd,
        ws.port_base.unwrap_or(18000),
    );

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
        return urls.get(svc).cloned();
    }
    // Prefer UI-ish names
    for (name, url) in urls {
        if name.contains("ui") {
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

fn discover_services(namespace: &str) -> Result<Vec<ServiceEndpoint>, Box<dyn Error>> {
    let json = run_cmd_output(
        "kubectl",
        &["get", "svc", "-n", namespace, "-o", "json"],
    )?;
    let value: serde_json::Value = serde_json::from_str(&json)?;
    let mut out = Vec::new();
    if let Some(items) = value.get("items").and_then(|i| i.as_array()) {
        for item in items {
            let name = item
                .pointer("/metadata/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() || name == "kubernetes" {
                continue;
            }
            let port = item
                .pointer("/spec/ports/0/port")
                .and_then(|v| v.as_u64())
                .unwrap_or(80) as u16;
            out.push(ServiceEndpoint {
                name,
                port,
                protocol: "TCP".into(),
            });
        }
    }
    Ok(out)
}
