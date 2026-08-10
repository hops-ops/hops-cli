//! `hops local status` — workspace health: pods, URLs, delivery, host access.
//!
//! Self-heals a dead DNS supervisor / port-forwards by default so status is usable truth.

use super::workbench::net::{
    discover_workspace_endpoints, ensure_host_access, format_status_card_with_listen,
    host_access_status_line, load_host_access_runtime, plan_host_access, url_listen_status,
};
use super::workbench::registry::{list_workspaces, load_workspace};
use super::{local_state_dir, run_cmd_output};
use clap::Args;
use std::error::Error;
use std::path::Path;

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Show only this workspace.
    #[arg(long)]
    pub name: Option<String>,

    /// Do not restart dead host-access processes (DNS supervisor / port-forwards).
    /// By default status self-heals so FQDN URLs stay usable after pod rollouts.
    #[arg(long, default_value_t = false)]
    pub no_heal: bool,

    /// Exit 1 if any workspace is not usable (pods not Ready or URLs not listening).
    #[arg(long, default_value_t = false)]
    pub check: bool,
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

    let mut all_ok = true;
    for (i, ws) in workspaces.iter().enumerate() {
        if i > 0 {
            println!();
        }
        if let Some(cn) = ws.cluster_name.as_deref() {
            let ctx = ws.kube_context.as_deref().unwrap_or("-");
            println!("cluster:  {cn} (context {ctx})");
        }
        let services = discover_workspace_endpoints(&ws.namespace).unwrap_or_default();

        let (plan, healed) = if !args.no_heal && !services.is_empty() {
            match ensure_host_access(&ws.namespace, &services, &state_dir, &ws.name) {
                Ok((plan, _rt, healed)) => {
                    if healed {
                        println!("note:     host access restarted (self-heal)");
                    }
                    (plan, healed)
                }
                Err(e) => {
                    log::warn!("host access heal failed: {e}");
                    (plan_host_access(&ws.namespace, &services), false)
                }
            }
        } else {
            (plan_host_access(&ws.namespace, &services), false)
        };
        let _ = healed;

        let listen = url_listen_status(&plan);
        println!(
            "{}",
            format_status_card_with_listen(&ws.name, &plan, &listen)
        );

        // Pods
        match discover_pods(&ws.namespace) {
            Ok(pods) if !pods.is_empty() => {
                println!("pods:");
                for p in &pods {
                    let mark = if p.ready { "ok" } else { "NOT READY" };
                    println!(
                        "  - {}: {} {}/{}  [{mark}]",
                        p.name, p.phase, p.ready_containers, p.total_containers
                    );
                    if !p.ready {
                        all_ok = false;
                    }
                }
            }
            Ok(_) => {
                println!("pods:     (none in namespace)");
                all_ok = false;
            }
            Err(e) => {
                println!("pods:     (kubectl error: {e})");
                all_ok = false;
            }
        }

        if let Some(d) = &ws.delivery_mode {
            println!("delivery: {d}");
        }
        println!("{}", delivery_status_line(&state_dir, &ws.name));
        println!("env:      {}", ws.env_path);

        if let Some(rt) = load_host_access_runtime(&state_dir, &ws.name)? {
            println!("{}", host_access_status_line(&rt));
        } else if services.is_empty() {
            println!("note:     no services listed yet — is the workspace up?");
        } else {
            println!("access processes: not recorded (re-run hops local up to start them)");
        }

        // URL listen summary for --check (cluster FQDN endpoints)
        for (name, ok) in &listen {
            if !ok {
                all_ok = false;
                println!(
                    "warn:     {name} FQDN not listening (port-forward dead or app not ready)"
                );
            }
        }
    }

    if args.check && !all_ok {
        return Err("one or more workspaces are not ready (see above)".into());
    }
    Ok(())
}

#[derive(Debug)]
struct PodStatus {
    name: String,
    phase: String,
    ready: bool,
    ready_containers: u32,
    total_containers: u32,
}

fn discover_pods(namespace: &str) -> Result<Vec<PodStatus>, Box<dyn Error>> {
    let json = run_cmd_output("kubectl", &["get", "pods", "-n", namespace, "-o", "json"])?;
    let value: serde_json::Value = serde_json::from_str(&json)?;
    let mut out = Vec::new();
    if let Some(items) = value.get("items").and_then(|i| i.as_array()) {
        for item in items {
            let name = item
                .pointer("/metadata/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let phase = item
                .pointer("/status/phase")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string();
            let mut ready_containers = 0u32;
            let mut total_containers = 0u32;
            if let Some(cs) = item.pointer("/status/containerStatuses").and_then(|v| v.as_array())
            {
                total_containers = cs.len() as u32;
                for c in cs {
                    if c.get("ready").and_then(|v| v.as_bool()).unwrap_or(false) {
                        ready_containers += 1;
                    }
                }
            }
            let ready = phase == "Running" && ready_containers == total_containers && total_containers > 0;
            out.push(PodStatus {
                name,
                phase,
                ready,
                ready_containers,
                total_containers,
            });
        }
    }
    Ok(out)
}

fn delivery_status_line(state_dir: &Path, workspace: &str) -> String {
    let path = state_dir
        .join("runtime")
        .join(format!("{workspace}.delivery.json"));
    if !path.exists() {
        return "delivery processes: none recorded".into();
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return "delivery processes: (unreadable state)".into();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return "delivery processes: (invalid state)".into();
    };
    let pids: Vec<u32> = v
        .get("syncPids")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64().map(|n| n as u32))
                .collect()
        })
        .unwrap_or_default();
    let mutagen: usize = v
        .get("mutagenSessions")
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let alive: Vec<u32> = pids
        .iter()
        .copied()
        .filter(|p| super::workbench::net::pid_is_alive(*p))
        .collect();
    if mutagen > 0 {
        format!("delivery processes: {mutagen} mutagen session(s); tar watchers alive={}", alive.len())
    } else if alive.is_empty() {
        "delivery processes: watcher not running (re-run hops local up --delivery sync)".into()
    } else {
        format!(
            "delivery processes: tar watcher alive (pids {})",
            alive
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}
