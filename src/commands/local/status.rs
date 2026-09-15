//! `hops local status` — read-only workspace health and access state.

use super::workbench::ingress::{
    discover_ingress_routes, format_ingress_status, ingress_access_matches_plan,
    ingress_routes_from_value, load_ingress_access_runtime, plan_from_routes, IngressAccessRuntime,
};
use super::workbench::net::{
    format_status_card_with_listen, host_access_needs_heal, host_access_status_line,
    load_host_access_runtime, plan_from_runtime as host_plan_from_runtime, url_listen_status,
};
use super::workbench::machine;
use super::workbench::registry::{
    activate_workspace_cluster, list_workspaces, load_workspace, WorkspaceRecord,
};
use super::{local_state_dir, run_cmd_output, HOPS_KUBE_CONTEXT_ENV};
use clap::Args;
use std::collections::BTreeSet;
use std::error::Error;
use std::path::Path;

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Show only this workspace.
    #[arg(long)]
    pub name: Option<String>,

    /// Print only public *.localhost URLs from HTTPRoutes.
    #[arg(long, default_value_t = false)]
    pub urls: bool,

    /// Include stale workspaces and missing kube contexts.
    #[arg(long, default_value_t = false)]
    pub all: bool,

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

    if args.urls {
        return print_urls(&workspaces, args.all);
    }

    print_cluster_section(&state_dir)?;
    println!();

    if args.all {
        return print_verbose(&state_dir, &workspaces, args);
    }

    let mut all_ok = true;
    let mut shown = 0usize;
    for ws in workspaces.iter() {
        if args.name.is_none() && !workspace_is_live(ws) {
            continue;
        }
        let _ = activate_workspace_cluster(ws);
        let pods = match discover_pods(&ws.namespace) {
            Ok(pods) => pods,
            Err(_) => {
                if args.name.is_some() {
                    println!("{}: cluster unreachable", ws.name);
                    all_ok = false;
                }
                continue;
            }
        };
        let running = pods.iter().any(|p| p.phase == "Running");
        if args.name.is_none() && !running {
            continue;
        }
        let urls = discover_ingress_routes(&ws.namespace)
            .ok()
            .and_then(|routes| plan_from_routes(&ws.namespace, &routes).ok())
            .map(|plan| plan.urls.into_values().collect::<Vec<_>>())
            .unwrap_or_default();
        if shown > 0 {
            println!();
        }
        shown += 1;
        let ready = pods.iter().filter(|p| p.ready).count();
        let total = pods
            .iter()
            .filter(|p| p.phase == "Running" || p.phase == "Pending")
            .count();
        let cluster = ws.cluster_name.as_deref().unwrap_or("-");
        println!("{}  {cluster}  {ready}/{total} ready", ws.name);
        if urls.is_empty() {
            println!("  (no public URLs)");
        } else {
            for url in &urls {
                println!("  {url}");
            }
        }
        for p in pods.iter().filter(|p| p.phase == "Running" && !p.ready) {
            all_ok = false;
            println!(
                "  not ready: {} {}/{}",
                p.name, p.ready_containers, p.total_containers
            );
        }
        if !running {
            all_ok = false;
            println!("  (no running pods)");
        }
    }

    if shown == 0 {
        println!(
            "No running workspaces. Pass --all for stale records, or --urls for HTTPRoute URLs."
        );
    }

    if args.check && !all_ok {
        return Err("one or more workspaces are not ready (see above)".into());
    }
    Ok(())
}

fn print_cluster_section(state_dir: &Path) -> Result<(), Box<dyn Error>> {
    let Some(record) = machine::load(state_dir)? else {
        println!("cluster  (none)  run `hops local up`");
        return Ok(());
    };
    println!("cluster  {}  {}", record.name, record.kube_context);
    std::env::set_var(HOPS_KUBE_CONTEXT_ENV, &record.kube_context);
    if let Ok(nodes) = kubectl_json(&["get", "nodes", "-o", "json"]) {
        for item in items(&nodes) {
            let name = meta_name(item);
            let version = item
                .pointer("/status/nodeInfo/kubeletVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("-");
            let ready = condition_ready(item, "Ready");
            println!("  node            {name}  {version}  {ready}");
        }
    } else {
        println!("  (cluster unreachable)");
        return Ok(());
    }
    if let Ok(authstacks) = kubectl_json(&["get", "authstack", "-A", "-o", "json"]) {
        for item in items(&authstacks) {
            let name = meta_name(item);
            let ready = condition_ready(item, "Ready");
            println!("  authstack       {name}  {ready}");
        }
    }
    if let Ok(configs) = kubectl_json(&["get", "configurations.pkg.crossplane.io", "-o", "json"]) {
        for item in items(&configs) {
            let name = meta_name(item);
            let package = package_ref(item);
            let ready = pkg_ready(item);
            println!("  configuration   {name}  {package}  {ready}");
        }
    }
    if let Ok(providers) = kubectl_json(&["get", "providers.pkg.crossplane.io", "-o", "json"]) {
        for item in items(&providers) {
            let name = meta_name(item);
            let package = package_ref(item);
            let ready = pkg_ready(item);
            println!("  provider        {name}  {package}  {ready}");
        }
    }
    Ok(())
}

fn kubectl_json(args: &[&str]) -> Result<serde_json::Value, Box<dyn Error>> {
    let raw = run_cmd_output("kubectl", args)?;
    Ok(serde_json::from_str(&raw)?)
}

fn items(value: &serde_json::Value) -> &[serde_json::Value] {
    value
        .get("items")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

fn meta_name(item: &serde_json::Value) -> &str {
    item.pointer("/metadata/name")
        .and_then(|v| v.as_str())
        .unwrap_or("-")
}

fn package_ref(item: &serde_json::Value) -> String {
    let raw = item
        .pointer("/spec/package")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    raw.rsplit('/').next().unwrap_or(raw).to_string()
}

fn condition_ready(item: &serde_json::Value, ty: &str) -> &'static str {
    let Some(conditions) = item.pointer("/status/conditions").and_then(|v| v.as_array()) else {
        return "-";
    };
    for condition in conditions {
        if condition.get("type").and_then(|v| v.as_str()) == Some(ty) {
            return if condition.get("status").and_then(|v| v.as_str()) == Some("True") {
                "Ready"
            } else {
                "NotReady"
            };
        }
    }
    "-"
}

fn pkg_ready(item: &serde_json::Value) -> &'static str {
    let healthy = condition_ready(item, "Healthy");
    let installed = condition_ready(item, "Installed");
    if healthy == "Ready" && installed == "Ready" {
        "Ready"
    } else if installed == "Ready" {
        "Installed"
    } else {
        "NotReady"
    }
}

fn print_verbose(
    state_dir: &Path,
    workspaces: &[WorkspaceRecord],
    args: &StatusArgs,
) -> Result<(), Box<dyn Error>> {
    let mut all_ok = true;
    for (i, ws) in workspaces.iter().enumerate() {
        if i > 0 {
            println!();
        }
        if let Some(cn) = ws.cluster_name.as_deref() {
            let ctx = ws.kube_context.as_deref().unwrap_or("-");
            println!("cluster:  {cn} (context {ctx})");
        }
        let _ = activate_workspace_cluster(ws);
        let host_access = load_host_access_runtime(state_dir, &ws.name)?;
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
            println!("service access: disabled (enable explicitly with `hops local fwd`)");
            Default::default()
        };

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
        println!("{}", delivery_status_line(state_dir, &ws.name));
        println!("env:      {}", ws.env_path);

        if let Some(rt) = &host_access {
            println!("{}", host_access_status_line(&rt));
        }

        let ingress_runtime = load_ingress_access_runtime(state_dir, &ws.name)?;
        match discover_ingress_routes(&ws.namespace) {
            Ok(routes) => match plan_from_routes(&ws.namespace, &routes) {
                Ok(plan) => {
                    if plan.urls.is_empty() {
                        println!("ingress:  (no HTTPRoute hostnames)");
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

fn print_urls(workspaces: &[WorkspaceRecord], all: bool) -> Result<(), Box<dyn Error>> {
    let mut seen_ctx = BTreeSet::new();
    let mut urls = BTreeSet::new();
    for ws in workspaces {
        if !all && !workspace_is_live(ws) {
            continue;
        }
        let ctx = ws.kube_context.as_deref().unwrap_or("");
        if ctx.is_empty() || !seen_ctx.insert(ctx.to_string()) {
            continue;
        }
        if !kube_context_exists(ctx) {
            continue;
        }
        let _ = activate_workspace_cluster(ws);
        match run_cmd_output("kubectl", &["get", "httproute", "-A", "-o", "json"]) {
            Ok(json) => {
                let value: serde_json::Value = serde_json::from_str(&json)?;
                for route in ingress_routes_from_value("", &value) {
                    if route.hostname.ends_with(".localhost") {
                        urls.insert(format!("https://{}", route.hostname));
                    }
                }
            }
            Err(_) => continue,
        }
    }
    if urls.is_empty() {
        if let Ok(json) = run_cmd_output("kubectl", &["get", "httproute", "-A", "-o", "json"]) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) {
                for route in ingress_routes_from_value("", &value) {
                    if route.hostname.ends_with(".localhost") {
                        urls.insert(format!("https://{}", route.hostname));
                    }
                }
            }
        }
    }
    if urls.is_empty() {
        println!("(no HTTPRoute *.localhost hostnames on live clusters)");
        return Ok(());
    }
    for url in urls {
        println!("{url}");
    }
    Ok(())
}

fn workspace_is_live(ws: &WorkspaceRecord) -> bool {
    match ws.kube_context.as_deref().filter(|ctx| !ctx.is_empty()) {
        Some(ctx) => kube_context_exists(ctx),
        None => true,
    }
}

fn kube_context_exists(ctx: &str) -> bool {
    run_cmd_output("kubectl", &["config", "get-contexts", "-o", "name"])
        .ok()
        .is_some_and(|out| out.lines().any(|line| line.trim() == ctx))
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
