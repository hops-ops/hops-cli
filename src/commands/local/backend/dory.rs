//! dory backend: product k3s in Dory's shared Apple Silicon engine
//! (https://augani.github.io/dory), driven headlessly through `dory k8s
//! enable|disable|status` and the engine docker socket.
//!
//! Design (first principles):
//! - Dory owns the VM, dockerd, networking, and k3s lifecycle.
//! - hops owns Crossplane + the package bridge.
//! - Packages do **not** use an in-cluster NodePort registry (kind/colima
//!   shape). Instead hops runs a normal `registry:2` container on the engine
//!   and teaches k3s to pull via `host.dory.internal` (Dory's host path).
//! - No hops-managed `~/.dory/k8s/ports` or registries files at cluster
//!   create — create shape matches stock Dory (API on 6443 only).

use super::SizeArgs;
use crate::commands::local::{command_exists, run_cmd, run_cmd_output};
use std::error::Error;
use std::path::PathBuf;

const NODE_CONTAINER: &str = "dory-k8s";
/// Engine-side package registry container (sibling of k3s, not in-cluster).
const PACKAGE_REGISTRY_NAME: &str = "hops-local-registry";
/// Host publish for `docker` / `crossplane xpkg` push (same port as other backends).
const PACKAGE_REGISTRY_HOST_PORT: &str = "30500";
/// How the k3s node (and pods that can resolve Dory host DNS) reach the engine registry.
pub const PACKAGE_REGISTRY_PULL: &str = "host.dory.internal:30500";

/// Seconds after the Dory app (re)creates its engine socket during which it is
/// still provisioning. A dockerd restart at the end SIGTERMs containers —
/// including a k3s node enabled in that window.
const ENGINE_LAUNCH_WINDOW_SECS: u64 = 180;

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
/// the pre-merge fallback.
pub fn kubeconfig_path() -> Option<String> {
    home()
        .ok()
        .map(|h| h.join(".kube/dory-config").to_string_lossy().into_owned())
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
    run_dory_enable(false)?;
    Ok(())
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
    // Best-effort: drop the package registry container with the cluster.
    let _ = engine_docker(&["rm", "-f", PACKAGE_REGISTRY_NAME]);
    log::info!("dory cluster deleted");
    Ok(())
}

/// The cluster container IS the cluster, so reset means recreate.
pub fn reset() -> Result<(), Box<dyn Error>> {
    preflight()?;
    run_cmd("dory", &["k8s", "disable"])?;
    run_dory_enable(true)
}

pub fn resize(_size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    Err("the dory backend has no hops-managed VM to resize; \
         adjust resources in the Dory app instead"
        .into())
}

/// Whether the hops-relevant dory cluster exists (running or stopped).
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
        return Err(
            "the `dory` CLI is not on PATH; install Dory (`brew install --cask Augani/dory/dory`) \
             or link a build that supports `dory k8s enable` \
             (`ln -sf <dory>/scripts/dory /opt/homebrew/bin/dory`)"
                .into(),
        );
    }
    // Require the headless lifecycle subcommand (stock cask without the scriptable
    // k8s surface only offers kubectl passthrough).
    if !dory_supports_k8s_enable() {
        return Err(
            "`dory k8s enable` is not available on this CLI. Install a Dory build with \
             scriptable Kubernetes (feat/scriptable-k8s), or use `--backend kind` against \
             Dory's docker socket."
                .into(),
        );
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

fn dory_supports_k8s_enable() -> bool {
    // Stock cask `dory k8s` is kubectl-only; scriptable builds reject unknown
    // enable flags with a dedicated usage line.
    std::process::Command::new("dory")
        .args(["k8s", "enable", "--__hops_probe__"])
        .output()
        .map(|o| {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            combined.contains("usage: dory k8s enable")
        })
        .unwrap_or(false)
}

fn engine_session_age() -> Option<std::time::Duration> {
    let sock = engine_socket().ok()?;
    let modified = std::fs::metadata(&sock).ok()?.modified().ok()?;
    std::time::SystemTime::now().duration_since(modified).ok()
}

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

/// Hold a freshly-enabled cluster while the Dory app may still be provisioning.
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
                run_cmd("dory", &dory_enable_args(false))?;
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

fn run_dory_enable(recreate: bool) -> Result<(), Box<dyn Error>> {
    let args = dory_enable_args(recreate);
    match run_cmd("dory", &args) {
        Ok(()) => hold_through_engine_launch_window(),
        // Exit 1 after "did not become Ready" is common on stop→start.
        Err(err) if !recreate && should_recreate_on_enable_error(&err.to_string()) => {
            log::warn!("dory k8s enable failed ({err}); recreating cluster once...");
            let _ = run_cmd("dory", &["k8s", "disable"]);
            run_dory_enable(true)
        }
        Err(err) => Err(err),
    }
}

fn should_recreate_on_enable_error(msg: &str) -> bool {
    msg.contains("exit status: 1")
}

fn dory_enable_args(recreate: bool) -> Vec<&'static str> {
    if recreate {
        vec!["k8s", "enable", "--recreate"]
    } else {
        vec!["k8s", "enable"]
    }
}

/// No create-time registry trust on dory — the package bridge configures the
/// running node after enable (see `ensure_package_bridge`).
pub fn ensure_registry_trust() -> Result<(), Box<dyn Error>> {
    Ok(())
}

/// In-cluster NodePort wiring is unused on dory.
pub fn wire_registry(_cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    Ok(())
}

/// Engine-side package registry + k3s mirror config for host.dory.internal.
///
/// Host pushes to `localhost:30500`; the node pulls via `host.dory.internal:30500`
/// (Dory's container→host path). No in-cluster registry Deployment.
pub fn ensure_package_bridge() -> Result<(), Box<dyn Error>> {
    preflight_engine_only()?;
    ensure_engine_registry()?;
    ensure_k3s_registry_mirrors()?;
    Ok(())
}

fn preflight_engine_only() -> Result<(), Box<dyn Error>> {
    let sock = engine_socket()?;
    if !sock.exists() {
        return Err(format!(
            "dory's engine socket ({}) is missing; launch the Dory app",
            sock.display()
        )
        .into());
    }
    if !command_exists("docker") {
        return Err("docker CLI not found".into());
    }
    Ok(())
}

fn ensure_engine_registry() -> Result<(), Box<dyn Error>> {
    let running = engine_docker_output(&[
        "inspect",
        "-f",
        "{{.State.Running}}",
        PACKAGE_REGISTRY_NAME,
    ])
    .unwrap_or_default();
    if running.trim() == "true" {
        return Ok(());
    }

    // Reuse a stopped container if present.
    if engine_docker_output(&["inspect", "-f", "{{.Id}}", PACKAGE_REGISTRY_NAME]).is_ok() {
        log::info!("Starting engine package registry '{}'...", PACKAGE_REGISTRY_NAME);
        engine_docker(&["start", PACKAGE_REGISTRY_NAME])?;
        return wait_registry_http();
    }

    log::info!(
        "Creating engine package registry on localhost:{} (pull via {})...",
        PACKAGE_REGISTRY_HOST_PORT,
        PACKAGE_REGISTRY_PULL
    );
    let _ = engine_docker(&["pull", "registry:2"]);
    engine_docker(&[
        "run",
        "-d",
        "--restart",
        "unless-stopped",
        "--name",
        PACKAGE_REGISTRY_NAME,
        "-p",
        &format!("{PACKAGE_REGISTRY_HOST_PORT}:5000"),
        "registry:2",
    ])?;
    wait_registry_http()
}

fn wait_registry_http() -> Result<(), Box<dyn Error>> {
    for _ in 0..30 {
        if run_cmd_output(
            "curl",
            &[
                "-sf",
                &format!("http://127.0.0.1:{PACKAGE_REGISTRY_HOST_PORT}/v2/"),
            ],
        )
        .is_ok()
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    Err(format!(
        "timed out waiting for engine registry on localhost:{PACKAGE_REGISTRY_HOST_PORT}"
    )
    .into())
}

/// Write k3s registries.yaml inside the node so containerd can pull HTTP from
/// host.dory.internal (and treat localhost:30500 the same for runtime images
/// that still reference REGISTRY_PUSH). Restarts the node container once.
fn ensure_k3s_registry_mirrors() -> Result<(), Box<dyn Error>> {
    if !node_running() {
        return Err("dory k8s node is not running; run `hops local start --backend dory` first".into());
    }

    let yaml = format!(
        "mirrors:\n\
         \x20 \"{pull}\":\n\
         \x20   endpoint:\n\
         \x20     - \"http://{pull}\"\n\
         \x20 \"localhost:{port}\":\n\
         \x20   endpoint:\n\
         \x20     - \"http://{pull}\"\n",
        pull = PACKAGE_REGISTRY_PULL,
        port = PACKAGE_REGISTRY_HOST_PORT,
    );

    let current = engine_docker_output(&[
        "exec",
        NODE_CONTAINER,
        "sh",
        "-c",
        "cat /etc/rancher/k3s/registries.yaml 2>/dev/null || true",
    ])
    .unwrap_or_default();
    if current.trim() == yaml.trim() {
        return Ok(());
    }

    log::info!(
        "Configuring k3s registry mirrors for {} (engine package bridge)...",
        PACKAGE_REGISTRY_PULL
    );
    // Write in-place inside the node (no create-time bind required).
    engine_docker(&[
        "exec",
        NODE_CONTAINER,
        "sh",
        "-c",
        &format!(
            "mkdir -p /etc/rancher/k3s && cat > /etc/rancher/k3s/registries.yaml <<'EOF'\n{yaml}EOF"
        ),
    ])?;
    log::info!("Restarting dory k8s node so k3s reloads registries.yaml...");
    engine_docker(&["restart", NODE_CONTAINER])?;
    // Wait for API again.
    for _ in 0..90 {
        if run_cmd_output(
            "kubectl",
            &["get", "--raw", "/readyz"],
        )
        .map(|s| s.trim() == "ok" || s.contains("ok"))
        .unwrap_or(false)
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    Err("timed out waiting for dory k8s after registries reload".into())
}

/// Make `--context dory` resolvable. Current dory merges into ~/.kube/config;
/// older builds only write the side file.
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
    fn dory_enable_args_only_recreate_for_reset_path() {
        assert_eq!(dory_enable_args(false), vec!["k8s", "enable"]);
        assert_eq!(
            dory_enable_args(true),
            vec!["k8s", "enable", "--recreate"]
        );
    }

    #[test]
    fn recreate_on_enable_matches_ready_timeout_exit() {
        assert!(should_recreate_on_enable_error(
            "dory exited with exit status: 1"
        ));
        assert!(!should_recreate_on_enable_error(
            "dory exited with exit status: 2"
        ));
        assert!(!should_recreate_on_enable_error("connection refused"));
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

    #[test]
    fn package_registry_pull_uses_dory_host_gateway() {
        assert_eq!(PACKAGE_REGISTRY_PULL, "host.dory.internal:30500");
        assert!(!PACKAGE_REGISTRY_PULL.contains("cluster.local"));
    }
}
