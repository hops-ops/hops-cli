//! Explicit opt-in direct access to Kubernetes Service FQDNs from the host.

use super::local_state_dir;
use super::workbench::net::{
    discover_workspace_endpoints, ensure_host_access, format_status_card_with_listen,
    host_access_status_line, stop_host_access, url_listen_status,
};
use super::workbench::registry::{activate_workspace_cluster, list_workspaces, load_workspace};
use clap::Args;
use std::error::Error;

#[derive(Args, Debug)]
pub struct DnsArgs {
    /// Enable or repair direct Service DNS for only this Environment.
    #[arg(long)]
    pub name: Option<String>,

    /// Disable direct Service DNS and stop its port-forwards.
    #[arg(long, default_value_t = false)]
    pub down: bool,
}

pub fn run(args: &DnsArgs) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    let workspaces = if let Some(name) = &args.name {
        match load_workspace(&state_dir, name)? {
            Some(record) => vec![record],
            None => return Err(format!("Environment {name:?} is not registered").into()),
        }
    } else {
        list_workspaces(&state_dir)?
    };

    if workspaces.is_empty() {
        return Err(
            "No local Environments are registered; pass --name after reconciling one".into(),
        );
    }

    for (index, workspace) in workspaces.iter().enumerate() {
        if index > 0 {
            println!();
        }
        if args.down {
            stop_host_access(&state_dir, &workspace.name)?;
            println!(
                "direct Service DNS disabled for Environment {}",
                workspace.name
            );
            continue;
        }
        activate_workspace_cluster(workspace).ok_or_else(|| {
            format!(
                "Environment {:?} has no durable cluster binding; reconcile it before enabling direct Service DNS",
                workspace.name
            )
        })?;

        let services = discover_workspace_endpoints(&workspace.namespace)?;
        if services.is_empty() {
            return Err(format!(
                "Environment {:?} has no discoverable Service endpoints",
                workspace.name
            )
            .into());
        }
        let (plan, runtime, changed) =
            ensure_host_access(&workspace.namespace, &services, &state_dir, &workspace.name)?;
        let listen = url_listen_status(&plan);
        println!(
            "{}",
            format_status_card_with_listen(&workspace.name, &plan, &listen)
        );
        println!("{}", host_access_status_line(&runtime));
        if changed {
            println!(
                "direct Service DNS enabled; Hops now owns its local DNS entries and port-forwards"
            );
        }
    }
    Ok(())
}
