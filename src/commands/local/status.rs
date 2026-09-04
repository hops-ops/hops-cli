//! `hops local status` — read-only workspace health and access state.

use super::workbench::ingress::{
    discover_ingress_routes, format_ingress_status, ingress_access_matches_plan,
    load_ingress_access_runtime, plan_from_routes, IngressAccessRuntime,
};
use super::workbench::net::{
    format_status_card_with_listen, host_access_needs_heal, host_access_status_line,
    load_host_access_runtime, plan_from_runtime as host_plan_from_runtime, url_listen_status,
};
use super::workbench::registry::{activate_workspace_cluster, list_workspaces, load_workspace};
use super::{local_state_dir, run_cmd_output};
use clap::Args;
use std::error::Error;
use std::path::Path;

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Show only this workspace.
    #[arg(long)]
    pub name: Option<String>,

    /// Deprecated compatibility flag; status is always read-only.
    #[arg(long, default_value_t = false, hide = true)]
    pub no_heal: bool,

    /// Exit 1 if pods, ingress, or enabled optional access are unhealthy.
    #[arg(long, default_value_t = false)]
    pub check: bool,
}

pub fn run(args: &StatusArgs) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    let workspaces = if let Some(name) = &args.name {
        match load_workspace(&state_dir, name)? {
            Some(r) => vec![r],
            None => return Err(format!("Workspace `{name}` is not registered.").into()),
        }
    } else {
        list_workspaces(&state_dir)?
    };

    if workspaces.is_empty() {
        println!("No local workspaces registered.");
        println!("Apply one with: hops local gitops environment <env-path> --name <environment>");
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
        // Target the workspace's bound cluster before kubectl discovery.
        let _ = activate_workspace_cluster(ws);
        let host_access = load_host_access_runtime(&state_dir, &ws.name)?;
        let listen = if let Some(runtime) = &host_access {
            let plan = host_plan_from_runtime(runtime);
            let listen = url_listen_status(&plan);
            println!(
                "{}",
                format_status_card_with_listen(&ws.name, &plan, &listen)
            );
            if host_access_needs_heal(runtime) {
                all_ok = false;
            }
            listen
        } else {
            println!("workspace: {}", ws.name);
            println!("namespace: {}", ws.namespace);
            println!("service access: disabled (enable explicitly with `hops local dns`)");
            Default::default()
        };

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

        if let Some(rt) = &host_access {
            println!("{}", host_access_status_line(&rt));
        }

        let ingress_runtime = load_ingress_access_runtime(&state_dir, &ws.name)?;
        match discover_ingress_routes(&ws.namespace) {
            Ok(routes) => match plan_from_routes(&ws.namespace, &routes) {
                Ok(plan) => {
                    if plan.urls.is_empty() {
                        println!("ingress:  (no HTTPRoute hostnames)");
                        if ingress_runtime.is_some() {
                            all_ok = false;
                            println!("warn:     stale ingress runtime is still recorded");
                        }
                    } else if let Some(runtime) = &ingress_runtime {
                        if !ingress_access_matches_plan(&plan, runtime) {
                            all_ok = false;
                        }
                        println!("{}", format_ingress_status(&plan, runtime));
                    } else {
                        all_ok = false;
                        let runtime = IngressAccessRuntime {
                            namespace: ws.namespace.clone(),
                            ..Default::default()
                        };
                        println!("{}", format_ingress_status(&plan, &runtime));
                    }
                }
                Err(error) => {
                    all_ok = false;
                    println!("ingress:  invalid ({error})");
                }
            },
            Err(error) => {
                all_ok = false;
                println!("ingress:  unavailable ({error})");
            }
        }

        if host_access.is_some() {
            for (name, ok) in &listen {
                if !ok {
                    all_ok = false;
                    println!("warn:     {name} optional Service access is not listening");
                }
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
            if let Some(cs) = item
                .pointer("/status/containerStatuses")
                .and_then(|v| v.as_array())
            {
                total_containers = cs.len() as u32;
                for c in cs {
                    if c.get("ready").and_then(|v| v.as_bool()).unwrap_or(false) {
                        ready_containers += 1;
                    }
                }
            }
            let ready =
                phase == "Running" && ready_containers == total_containers && total_containers > 0;
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
        format!(
            "delivery processes: {mutagen} mutagen session(s); tar watchers alive={}",
            alive.len()
        )
    } else if alive.is_empty() {
        "delivery processes: watcher not running".into()
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
