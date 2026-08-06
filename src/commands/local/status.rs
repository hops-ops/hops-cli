//! `hops local status` — list workspaces, app URLs, and whether access processes are alive.

use super::workbench::net::{
    format_status_card, host_access_status_line, load_host_access_runtime, plan_host_access,
    HostAccessMode, ServiceEndpoint,
};
use super::workbench::registry::{list_workspaces, load_workspace};
use super::{command_exists, local_state_dir, run_cmd_output};
use clap::Args;
use std::error::Error;

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Show only this workspace.
    #[arg(long)]
    pub name: Option<String>,
}

pub fn run(args: &StatusArgs) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    let workspaces = if let Some(name) = &args.name {
        match load_workspace(&state_dir, name)? {
            Some(r) => vec![r],
            None => {
                return Err(format!(
                    "Workspace `{name}` not found. Run hops local up first."
                )
                .into())
            }
        }
    } else {
        list_workspaces(&state_dir)?
    };

    if workspaces.is_empty() {
        println!("No local workspaces registered.");
        println!("Start one with: hops local up <env-path> [--name <workspace>]");
        return Ok(());
    }

    let kubefwd = command_exists("kubefwd");
    for (i, ws) in workspaces.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let services = discover_services(&ws.namespace).unwrap_or_default();
        let prefer_kubefwd = match ws.host_access_mode.as_deref() {
            Some("map") => false,
            Some("kubefwd") => true,
            _ => kubefwd,
        };
        let port_base = ws.port_base.unwrap_or(18000);
        let plan = plan_host_access(&ws.namespace, &services, prefer_kubefwd, port_base);
        println!("{}", format_status_card(&ws.name, &plan));
        if let Some(d) = &ws.delivery_mode {
            println!("delivery: {d}");
        }
        println!("env:      {}", ws.env_path);
        if let Some(rt) = load_host_access_runtime(&state_dir, &ws.name)? {
            println!("{}", host_access_status_line(&rt));
        } else if plan.mode == HostAccessMode::Map && services.is_empty() {
            println!("note:     no services listed yet — is the workspace up?");
        } else {
            println!("access processes: not recorded (re-run hops local up to start them)");
        }
    }
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
