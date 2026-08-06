//! `hops local down` — stop workspace host access and optionally purge namespace.

use super::workbench::registry::{list_workspaces, load_workspace, remove_workspace, namespace_for_name};
use super::{local_state_dir, run_cmd};
use clap::Args;
use std::error::Error;

#[derive(Args, Debug)]
pub struct DownArgs {
    /// Workspace name (default: only workspace if exactly one registered).
    #[arg(long)]
    pub name: Option<String>,

    /// Delete the workspace namespace and labeled resources.
    #[arg(long, default_value_t = false)]
    pub purge: bool,
}

pub fn run(args: &DownArgs) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    let name = match &args.name {
        Some(n) => n.clone(),
        None => {
            let all = list_workspaces(&state_dir)?;
            match all.as_slice() {
                [only] => only.name.clone(),
                [] => {
                    return Err(
                        "No workspaces registered. Pass --name or run hops local up first.".into(),
                    )
                }
                _ => {
                    return Err(format!(
                        "Multiple workspaces registered ({}); pass --name <workspace>.",
                        all.iter()
                            .map(|w| w.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                    .into())
                }
            }
        }
    };

    let record = load_workspace(&state_dir, &name)?;
    let namespace = record
        .as_ref()
        .map(|r| r.namespace.clone())
        .unwrap_or_else(|| namespace_for_name(&name));

    log::info!("Bringing down workspace `{name}` (namespace {namespace})");

    // Best-effort: stop any port-forwards / kubefwd for this namespace (pkill by ns).
    let _ = run_cmd(
        "sh",
        &[
            "-c",
            &format!(
                "pkill -f 'kubefwd.*{namespace}' 2>/dev/null || true; \
                 pkill -f 'port-forward.*-n {namespace}' 2>/dev/null || true"
            ),
        ],
    );

    if args.purge {
        log::info!("Purging namespace {namespace}...");
        match run_cmd("kubectl", &["delete", "namespace", &namespace, "--wait=false"]) {
            Ok(()) => log::info!("Namespace {namespace} delete requested."),
            Err(e) => log::warn!("Namespace delete: {e}"),
        }
    } else {
        log::info!("Leaving namespace {namespace} in place (pass --purge to delete).");
    }

    remove_workspace(&state_dir, &name)?;
    log::info!("Workspace `{name}` unregistered.");
    Ok(())
}
