//! dory backend: k3s in a container on dory's shared-VM dockerd
//! (https://augani.github.io/dory), driven headlessly through the `dory` CLI
//! (`dory k8s enable|disable|status`) and the engine docker socket.
//!
//! Registry plumbing differs from colima/kind because the cluster container
//! has create-time config shared with the Dory app:
//! - published NodePorts are static config: hops writes `~/.dory/k8s/ports`
//!   BEFORE `dory k8s enable`, so GUI-side recreates keep the same port.
//! - trust is static config: hops writes `~/.dory/k8s/registries.yaml`
//!   BEFORE `dory k8s enable`; dory bind-mounts it and k3s reads it at boot,
//!   aliasing both pull names to the registry Service hostname over HTTP.
//! - name resolution is dynamic: `wire_registry` syncs the hostname ->
//!   ClusterIP in the node container's /etc/hosts on every start (same
//!   re-wire-on-start model as the other backends).
//! - the host reaches `localhost:30500` because `dory k8s enable --publish
//!   30500:30500` publishes the NodePort and the Dory app's port forwarder
//!   maps published ports to host loopback.

use super::SizeArgs;
use crate::commands::local::package_install::{REGISTRY_HOSTNAME, REGISTRY_PULL, REGISTRY_PUSH};
use crate::commands::local::{command_exists, run_cmd, run_cmd_output};
use std::error::Error;
use std::path::PathBuf;

const NODE_CONTAINER: &str = "dory-k8s";
/// hops' registry NodePort, published on the cluster container at create.
const REGISTRY_PORT_PUBLISH: &str = "30500:30500";
const REGISTRIES_BEGIN: &str = "# BEGIN hops-managed (do not edit inside)";
const REGISTRIES_END: &str = "# END hops-managed";

fn home() -> Result<PathBuf, Box<dyn Error>> {
    Ok(PathBuf::from(std::env::var("HOME").map_err(|_| {
        "HOME is not set; unable to locate dory's state directory"
    })?))
}

fn engine_socket() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join(".dory/engine.sock"))
}

/// dory's side-file kubeconfig (context name `dory`). Current dory also
/// merges the context into ~/.kube/config at enable time; the side file is
/// the pre-merge fallback and dory's own `--kubeconfig` input.
pub fn kubeconfig_path() -> Option<String> {
    home()
        .ok()
        .map(|h| h.join(".kube/dory-config").to_string_lossy().into_owned())
}

fn registries_yaml_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join(".dory/k8s/registries.yaml"))
}

fn ports_file_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join(".dory/k8s/ports"))
}

/// k3s' native registry config: alias both pull names to the registry
/// Service hostname over plain HTTP. The hostname resolves through the
/// node's /etc/hosts, which `wire_registry` keeps pointed at the ClusterIP.
fn registries_yaml() -> String {
    format!(
        "# Written by `hops local start --backend dory`.\n\
         # k3s reads this at boot; edits require `hops local reset`.\n\
         # Hops manages only the marked block; keep user mirrors outside it.\n\
         mirrors:\n\
         {block}",
        block = registries_yaml_block(),
    )
}

fn registries_yaml_block() -> String {
    format!(
        "  {begin}\n\
         \x20\x20# hops: mirror {pull}\n\
         \x20\x20\"{pull}\":\n\
         \x20\x20  endpoint:\n\
         \x20\x20    - \"http://{pull}\"\n\
         \x20\x20# hops: mirror {push}\n\
         \x20\x20\"{push}\":\n\
         \x20\x20  endpoint:\n\
         \x20\x20    - \"http://{pull}\"\n\
         \x20\x20{end}\n",
        begin = REGISTRIES_BEGIN,
        end = REGISTRIES_END,
        pull = REGISTRY_PULL,
        push = REGISTRY_PUSH,
    )
}

fn legacy_registries_yaml() -> String {
    format!(
        "# Written by `hops local start --backend dory`.\n\
         # k3s reads this at boot; edits require `hops local reset`.\n\
         mirrors:\n\
         \x20 \"{pull}\":\n\
         \x20   endpoint:\n\
         \x20     - \"http://{pull}\"\n\
         \x20 \"{push}\":\n\
         \x20   endpoint:\n\
         \x20     - \"http://{pull}\"\n",
        pull = REGISTRY_PULL,
        push = REGISTRY_PUSH,
    )
}

/// Run docker against dory's engine socket (the daemon the Dory app manages).
fn engine_docker(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let sock = format!("unix://{}", engine_socket()?.display());
    let mut full = vec!["-H", sock.as_str()];
    full.extend_from_slice(args);
    run_cmd("docker", &full)
}

fn engine_docker_output(args: &[&str]) -> Result<String, Box<dyn Error>> {
    let sock = format!("unix://{}", engine_socket()?.display());
    let mut full = vec!["-H", sock.as_str()];
    full.extend_from_slice(args);
    run_cmd_output("docker", &full)
}

pub fn install() -> Result<(), Box<dyn Error>> {
    log::info!("Installing Dory via Homebrew...");
    run_cmd("brew", &["install", "--cask", "Augani/dory/dory"])?;
    log::info!("Dory installed; launch the Dory app once so it provisions its engine");
    Ok(())
}

pub fn uninstall() -> Result<(), Box<dyn Error>> {
    log::info!("Uninstalling Dory...");
    run_cmd("brew", &["uninstall", "--cask", "dory"])?;
    log::info!("Dory uninstalled");
    Ok(())
}

pub fn start(size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    if size.any_set() {
        return Err(format!(
            "the dory backend's VM is sized by the Dory app, not hops; drop{}",
            size.command_suffix()
        )
        .into());
    }

    preflight()?;
    write_ports_file()?;
    write_registries_yaml()?;

    // Creates, restarts, or reuses the dory-k8s container as needed. If the
    // running container has create-time config drift, dory exits 3 rather than
    // destroying state; surface that plus the hops reset path.
    run_dory_enable(false)
}

pub fn stop() -> Result<(), Box<dyn Error>> {
    log::info!("Stopping dory k8s node '{}'...", NODE_CONTAINER);
    engine_docker(&["stop", NODE_CONTAINER])?;
    log::info!("dory cluster stopped");
    Ok(())
}

pub fn destroy() -> Result<(), Box<dyn Error>> {
    log::info!("Deleting dory k8s cluster...");
    run_cmd("dory", &["k8s", "disable"])?;
    log::info!("dory cluster deleted");
    Ok(())
}

/// The cluster container IS the cluster, so reset means recreate.
pub fn reset() -> Result<(), Box<dyn Error>> {
    preflight()?;
    run_cmd("dory", &["k8s", "disable"])?;
    write_ports_file()?;
    write_registries_yaml()?;
    run_dory_enable(true)
}

pub fn resize(_size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    Err("the dory backend has no hops-managed VM to resize; \
         adjust resources in the Dory app instead"
        .into())
}

/// Whether the hops-relevant dory cluster exists (running or stopped).
/// Missing app/engine/CLI reads as "no cluster".
pub fn cluster_exists() -> bool {
    let Ok(sock) = engine_socket() else {
        return false;
    };
    if !sock.exists() {
        return false;
    }
    engine_docker_output(&["inspect", "-f", "{{.State.Running}}", NODE_CONTAINER]).is_ok()
}

fn preflight() -> Result<(), Box<dyn Error>> {
    if !command_exists("dory") {
        return Err("the `dory` CLI is not on PATH; link it from the dory repo \
             (`ln -sf <dory>/scripts/dory /opt/homebrew/bin/dory`)"
            .into());
    }
    let sock = engine_socket()?;
    if !sock.exists() {
        return Err(format!(
            "dory's engine socket ({}) is missing; launch the Dory app and wait for its engine to start",
            sock.display()
        )
        .into());
    }
    if !command_exists("docker") {
        return Err("docker CLI not found; install it (dory provides the daemon)".into());
    }
    Ok(())
}

/// Write the static registry trust config read by k3s at boot. Must exist
/// before `dory k8s enable` because dory binds it at container create.
fn write_registries_yaml() -> Result<(), Box<dyn Error>> {
    let path = registries_yaml_path()?;
    let current = match std::fs::read_to_string(&path) {
        Ok(content) => Some(content),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.into()),
    };
    let desired = merge_registries_yaml(current.as_deref())?;
    if current.as_deref() == Some(desired.as_str()) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    log::info!("Writing k3s registry config: {}", path.display());
    std::fs::write(&path, desired)?;
    Ok(())
}

fn write_ports_file() -> Result<(), Box<dyn Error>> {
    let path = ports_file_path()?;
    let current = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };
    let desired = ports_file_with_publish(&current);
    if current == desired {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    log::info!(
        "Ensuring dory k8s port config includes {}: {}",
        REGISTRY_PORT_PUBLISH,
        path.display()
    );
    std::fs::write(&path, desired)?;
    Ok(())
}

/// Seconds after the Dory app (re)creates its engine socket during which it is
/// still provisioning the engine. A dockerd restart at the end of that window
/// SIGTERMs every container — including a k3s node enabled meanwhile, which
/// reports Ready and then dies under the bootstrap (observed ~90s on Dory
/// 0.2.0; padded for slower machines).
const ENGINE_LAUNCH_WINDOW_SECS: u64 = 180;

/// Age of the current engine session: the app recreates the engine socket at
/// launch, so its mtime marks when provisioning began. None when unreadable.
fn engine_session_age() -> Option<std::time::Duration> {
    let sock = engine_socket().ok()?;
    let modified = std::fs::metadata(&sock).ok()?.modified().ok()?;
    std::time::SystemTime::now().duration_since(modified).ok()
}

/// Time left inside the app's provisioning window for a given engine session
/// age; None once the window has passed.
fn launch_window_remaining(age: std::time::Duration) -> Option<std::time::Duration> {
    std::time::Duration::from_secs(ENGINE_LAUNCH_WINDOW_SECS)
        .checked_sub(age)
        .filter(|remaining| !remaining.is_zero())
}

fn node_running() -> bool {
    engine_docker_output(&["inspect", "-f", "{{.State.Running}}", NODE_CONTAINER])
        .map(|state| state.trim() == "true")
        .unwrap_or(false)
}

/// Hold a freshly-enabled cluster under observation while the Dory app may
/// still be provisioning its engine, re-enabling if the engine restart takes
/// the node down. Immediate no-op when the window has already passed, so
/// steady-state starts pay one container inspect and nothing more.
fn hold_through_engine_launch_window() -> Result<(), Box<dyn Error>> {
    let in_window = |age: Option<std::time::Duration>| {
        age.map(|a| launch_window_remaining(a).is_some())
            .unwrap_or(false)
    };
    if in_window(engine_session_age()) {
        log::info!(
            "Dory engine session is younger than {}s; watching the k8s node through the app's provisioning window...",
            ENGINE_LAUNCH_WINDOW_SECS
        );
    }
    let mut reenables = 0;
    loop {
        match (node_running(), in_window(engine_session_age())) {
            (true, false) => return Ok(()),
            (true, true) => {}
            (false, _) => {
                if reenables >= 3 {
                    return Err("the dory engine keeps stopping the k8s node during app startup; \
                         wait for the Dory app to finish provisioning, then re-run `hops local start --backend dory`"
                        .into());
                }
                reenables += 1;
                log::warn!(
                    "dory engine restart stopped the k8s node; re-enabling ({}/3)...",
                    reenables
                );
                let args = dory_enable_args(false);
                run_cmd("dory", &args)?;
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

fn run_dory_enable(recreate: bool) -> Result<(), Box<dyn Error>> {
    let args = dory_enable_args(recreate);
    match run_cmd("dory", &args) {
        Ok(()) => hold_through_engine_launch_window(),
        Err(err) if !recreate && err.to_string().contains("exit status: 3") => Err(format!(
            "{}\nhint: run `hops local reset --backend dory` to recreate the dory cluster and apply create-time config drift",
            err
        )
        .into()),
        Err(err) => Err(err),
    }
}

fn dory_enable_args(recreate: bool) -> Vec<&'static str> {
    let mut args = vec!["k8s", "enable"];
    if recreate {
        args.push("--recreate");
    }
    args.extend(["--publish", REGISTRY_PORT_PUBLISH]);
    args
}

fn merge_registries_yaml(existing: Option<&str>) -> Result<String, Box<dyn Error>> {
    let Some(existing) = existing else {
        return Ok(registries_yaml());
    };
    if existing.trim().is_empty() || existing == legacy_registries_yaml() {
        return Ok(registries_yaml());
    }

    match (
        existing.find(REGISTRIES_BEGIN),
        existing.find(REGISTRIES_END),
    ) {
        (Some(begin), Some(end)) if begin <= end => replace_registries_block(existing, begin, end),
        (Some(_), Some(_)) | (Some(_), None) | (None, Some(_)) => {
            Err(format!("malformed dory registries.yaml managed block; expected `{REGISTRIES_BEGIN}` before `{REGISTRIES_END}`").into())
        }
        (None, None) => insert_registries_block(existing),
    }
}

fn replace_registries_block(
    existing: &str,
    begin: usize,
    end: usize,
) -> Result<String, Box<dyn Error>> {
    let line_start = existing[..begin].rfind('\n').map_or(0, |idx| idx + 1);
    let line_end = existing[end..]
        .find('\n')
        .map_or(existing.len(), |idx| end + idx + 1);
    let mut merged = String::with_capacity(existing.len() + registries_yaml_block().len());
    merged.push_str(&existing[..line_start]);
    merged.push_str(&registries_yaml_block());
    merged.push_str(&existing[line_end..]);
    Ok(merged)
}

fn insert_registries_block(existing: &str) -> Result<String, Box<dyn Error>> {
    if contains_hops_mirror_key(existing) {
        return Err("dory registries.yaml already contains hops registry mirror entries without managed markers; remove those entries or wrap them in the hops-managed block"
            .into());
    }

    let mut merged = ensure_trailing_newline(existing);
    if let Some(insert_at) = mirrors_line_insert_position(&merged) {
        merged.insert_str(insert_at, &registries_yaml_block());
        return Ok(merged);
    }

    merged.push_str("mirrors:\n");
    merged.push_str(&registries_yaml_block());
    Ok(merged)
}

fn contains_hops_mirror_key(content: &str) -> bool {
    content.contains(&format!("\"{}\":", REGISTRY_PULL))
        || content.contains(&format!("\"{}\":", REGISTRY_PUSH))
}

fn mirrors_line_insert_position(content: &str) -> Option<usize> {
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        let without_newline = line.trim_end_matches('\n').trim_end_matches('\r');
        if is_top_level_mirrors_line(without_newline) {
            return Some(offset + line.len());
        }
        offset += line.len();
    }
    None
}

fn is_top_level_mirrors_line(line: &str) -> bool {
    line.starts_with("mirrors:") && line.split('#').next().unwrap_or("").trim_end() == "mirrors:"
}

fn ensure_trailing_newline(content: &str) -> String {
    let mut out = content.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
struct PortPublish {
    host: u16,
    container: u16,
    proto: String,
}

fn ports_file_with_publish(existing: &str) -> String {
    if ports_file_contains_publish(existing, REGISTRY_PORT_PUBLISH) {
        return existing.to_string();
    }

    let mut out = ensure_trailing_newline(existing);
    out.push_str(REGISTRY_PORT_PUBLISH);
    out.push('\n');
    out
}

fn ports_file_contains_publish(existing: &str, desired: &str) -> bool {
    let Some(desired) = parse_port_publish(desired) else {
        return false;
    };
    existing
        .lines()
        .filter_map(parse_port_publish)
        .any(|port| port == desired)
}

fn parse_port_publish(line: &str) -> Option<PortPublish> {
    let cleaned: String = line
        .split('#')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    let (ports, proto) = cleaned
        .split_once('/')
        .map_or((cleaned.as_str(), "tcp"), |(ports, proto)| (ports, proto));
    if proto != "tcp" && proto != "udp" {
        return None;
    }
    let (host, container) = ports.split_once(':')?;
    let host = host.parse().ok().filter(|port| *port > 0)?;
    let container = container.parse().ok().filter(|port| *port > 0)?;
    Some(PortPublish {
        host,
        container,
        proto: proto.to_string(),
    })
}

/// Keep the node container's /etc/hosts pointing the registry hostname at
/// the current ClusterIP (docker regenerates /etc/hosts on restart, and the
/// ClusterIP changes if the Service is recreated — hence re-run per start).
pub fn wire_registry(cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    let hostname = REGISTRY_HOSTNAME;
    let current_ip = engine_docker_output(&[
        "exec",
        NODE_CONTAINER,
        "sh",
        "-c",
        &format!("awk '$2 == \"{}\" {{print $1; exit}}' /etc/hosts", hostname),
    ])
    .unwrap_or_default();
    if current_ip.trim() == cluster_ip {
        return Ok(());
    }

    log::info!("Updating node hosts entry: {} -> {}", hostname, cluster_ip);

    // /etc/hosts is a bind mount inside the container: `sed -i` fails with
    // "Resource busy" (rename over a mount point), so rewrite it in place.
    engine_docker(&[
        "exec",
        NODE_CONTAINER,
        "sh",
        "-c",
        &format!(
            "awk '$2 != \"{host}\"' /etc/hosts > /tmp/hosts.new && \
             cat /tmp/hosts.new > /etc/hosts && rm -f /tmp/hosts.new && \
             echo '{ip} {host}' >> /etc/hosts",
            host = hostname,
            ip = cluster_ip
        ),
    ])?;

    Ok(())
}

/// Make `--context dory` resolvable. Current dory merges the context into
/// ~/.kube/config at enable time, so normally there is nothing to do —
/// mutating KUBECONFIG for every child process is then pure noise. Older
/// dory versions only write the side file; for those, prepend it to
/// KUBECONFIG (preserving whatever the user already has).
pub fn export_kubeconfig_env() {
    if effective_kubeconfig_has_dory_context() {
        return;
    }
    let Some(dory_cfg) = kubeconfig_path() else {
        return;
    };
    let existing = std::env::var("KUBECONFIG").unwrap_or_default();
    if existing.split(':').any(|p| p == dory_cfg) {
        return;
    }
    let rest = if existing.is_empty() {
        match home() {
            Ok(h) => h.join(".kube/config").to_string_lossy().into_owned(),
            Err(_) => return,
        }
    } else {
        existing
    };
    std::env::set_var("KUBECONFIG", format!("{}:{}", dory_cfg, rest));
}

/// Whether the kubeconfig(s) kubectl will read without our help — the
/// $KUBECONFIG chain when set, else ~/.kube/config — already define a
/// `dory` entry (i.e. dory's kubectl-merge ran against a file in scope).
fn effective_kubeconfig_has_dory_context() -> bool {
    let paths: Vec<PathBuf> = match std::env::var("KUBECONFIG") {
        Ok(chain) if !chain.is_empty() => chain.split(':').map(PathBuf::from).collect(),
        _ => match home() {
            Ok(h) => vec![h.join(".kube/config")],
            Err(_) => return false,
        },
    };
    paths.iter().any(|path| {
        std::fs::read_to_string(path)
            .map(|content| has_dory_entry(&content))
            .unwrap_or(false)
    })
}

/// Line-anchored scan for a kubeconfig entry named `dory` — the mapping
/// form (`name: dory`, as kubectl writes context/cluster names) or the
/// sequence-item form (`- name: dory`, the users list). `name: dory-prod`
/// or `username: dory` must not count.
fn has_dory_entry(kubeconfig: &str) -> bool {
    kubeconfig.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == "name: dory" || trimmed == "- name: dory"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_window_remaining_covers_only_the_provisioning_window() {
        use std::time::Duration;

        assert_eq!(
            launch_window_remaining(Duration::ZERO),
            Some(Duration::from_secs(ENGINE_LAUNCH_WINDOW_SECS))
        );
        assert_eq!(
            launch_window_remaining(Duration::from_secs(ENGINE_LAUNCH_WINDOW_SECS - 1)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            launch_window_remaining(Duration::from_secs(ENGINE_LAUNCH_WINDOW_SECS)),
            None
        );
        assert_eq!(launch_window_remaining(Duration::from_secs(3600)), None);
    }

    #[test]
    fn has_dory_entry_matches_mapping_and_sequence_forms_only() {
        let merged = "contexts:\n- context:\n    cluster: dory\n    user: dory\n  name: dory\n";
        let users_list = "users:\n- name: dory\n  user: {}\n";
        let near_misses = "name: dory-prod\nusername: dory\n# name: dory\nfullname: dory\n";

        assert!(has_dory_entry(merged));
        assert!(has_dory_entry(users_list));
        assert!(!has_dory_entry(near_misses));
        assert!(!has_dory_entry(""));
    }

    #[test]
    fn registries_yaml_aliases_both_pull_names_to_the_service_over_http() {
        let yaml = registries_yaml();

        assert!(yaml.contains(REGISTRIES_BEGIN));
        assert!(yaml.contains(REGISTRIES_END));
        assert!(yaml.contains("\"registry.crossplane-system.svc.cluster.local:5000\":"));
        assert!(yaml.contains("\"localhost:30500\":"));
        // Both mirrors resolve to the same HTTP endpoint on the Service name.
        assert_eq!(
            yaml.matches("- \"http://registry.crossplane-system.svc.cluster.local:5000\"")
                .count(),
            2
        );
        assert!(!yaml.contains("https://"));
    }

    #[test]
    fn registries_yaml_replaces_only_hops_managed_block() {
        let existing = format!(
            "configs:\n  example: value\nmirrors:\n  {begin}\n  old: value\n  {end}\n  \"user.local:5000\":\n    endpoint:\n      - \"http://user.local:5000\"\n",
            begin = REGISTRIES_BEGIN,
            end = REGISTRIES_END,
        );

        let merged = merge_registries_yaml(Some(&existing)).unwrap();

        assert!(merged.starts_with("configs:\n  example: value\nmirrors:\n"));
        assert!(merged.contains("  \"user.local:5000\":\n"));
        assert!(!merged.contains("old: value"));
        assert!(merged.contains(&format!("  \"{}\":", REGISTRY_PULL)));
        assert_eq!(merged.matches(REGISTRIES_BEGIN).count(), 1);
        assert_eq!(merged.matches(REGISTRIES_END).count(), 1);
    }

    #[test]
    fn registries_yaml_inserts_block_under_existing_mirrors_key() {
        let existing = "mirrors:\n  \"user.local:5000\":\n    endpoint:\n      - \"http://user.local:5000\"\nconfigs:\n  another: value\n";

        let merged = merge_registries_yaml(Some(existing)).unwrap();

        let block_pos = merged.find(REGISTRIES_BEGIN).unwrap();
        let user_pos = merged.find("\"user.local:5000\"").unwrap();
        assert!(block_pos < user_pos);
        assert!(merged.contains("configs:\n  another: value\n"));
    }

    #[test]
    fn registries_yaml_appends_mirrors_section_when_missing() {
        let existing = "configs:\n  example: value\n";

        let merged = merge_registries_yaml(Some(existing)).unwrap();

        assert!(merged.starts_with(existing));
        assert!(merged.contains("\nmirrors:\n"));
        assert!(merged.contains(REGISTRIES_BEGIN));
    }

    #[test]
    fn registries_yaml_upgrades_legacy_hops_file_to_managed_block() {
        let merged = merge_registries_yaml(Some(&legacy_registries_yaml())).unwrap();

        assert_eq!(merged, registries_yaml());
        assert_eq!(
            merged.matches(&format!("\"{}\":", REGISTRY_PULL)).count(),
            1
        );
        assert_eq!(
            merged.matches(&format!("\"{}\":", REGISTRY_PUSH)).count(),
            1
        );
    }

    #[test]
    fn ports_file_appends_registry_port_without_touching_existing_lines() {
        let existing = "# user port\n8080:80/udp\n";

        let merged = ports_file_with_publish(existing);

        assert_eq!(merged, "# user port\n8080:80/udp\n30500:30500\n");
    }

    #[test]
    fn ports_file_absent_writes_only_registry_port() {
        assert_eq!(ports_file_with_publish(""), "30500:30500\n");
    }

    #[test]
    fn ports_file_treats_tcp_variant_as_already_present() {
        let existing = " 30500 : 30500 / tcp  # registry\n";

        assert_eq!(ports_file_with_publish(existing), existing);
    }

    #[test]
    fn dory_enable_args_only_recreate_for_reset_path() {
        assert_eq!(
            dory_enable_args(false),
            vec!["k8s", "enable", "--publish", REGISTRY_PORT_PUBLISH]
        );
        assert_eq!(
            dory_enable_args(true),
            vec![
                "k8s",
                "enable",
                "--recreate",
                "--publish",
                REGISTRY_PORT_PUBLISH
            ]
        );
    }

    #[test]
    fn start_rejects_size_flags() {
        let size = SizeArgs {
            cpus: Some(4),
            memory: None,
            disk: None,
        };

        let err = start(&size).expect_err("size flags must be rejected");

        assert!(err.to_string().contains("--cpus 4"));
        assert!(err.to_string().contains("Dory app"));
    }
}
