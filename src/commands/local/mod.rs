mod aws;
pub mod backend;
mod cloudflare;
mod destroy;
mod doctor;
mod down;
mod github;
mod gitops;
pub mod gitops_write;
mod install;
mod listmonk;
pub mod package_install;
mod reset;
mod resize;
mod start;
mod status;
mod uninstall;
pub mod workbench;
mod zitadel;

use clap::{Args, Subcommand};
use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LOCAL_STATE_DIR: &str = ".hops/local";
const REPO_CACHE_DIR: &str = "repo-cache";

/// metadata.labels key recording who manages a resource (k8s recommended label).
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
/// `MANAGED_BY_LABEL` value that `hops provider install` stamps on the Provider,
/// DeploymentRuntimeConfig, and ClusterRoleBinding it manages — so `hops local
/// doctor` can tell an intentional custom install (e.g. a forked provider built
/// from source) apart from accidental drift.
pub const PROVIDER_INSTALL_MANAGED_BY: &str = "hops-provider-install";

/// Env var checked by kubectl helpers to inject `--context <name>`.
pub const HOPS_KUBE_CONTEXT_ENV: &str = "HOPS_KUBE_CONTEXT";

fn kube_context_from_env() -> Option<String> {
    std::env::var(HOPS_KUBE_CONTEXT_ENV)
        .ok()
        .filter(|ctx| !ctx.is_empty())
}

/// Prepend `--context` to a kubectl arg slice when configured.
fn with_kube_context(args: &[&str]) -> Vec<String> {
    let ctx = kube_context_from_env();
    with_kube_context_value(args, ctx.as_deref())
}

fn with_kube_context_value(args: &[&str], context: Option<&str>) -> Vec<String> {
    let mut out = match context {
        Some(ctx) if !ctx.is_empty() => vec!["--context".to_string(), ctx.to_string()],
        _ => vec![],
    };
    out.extend(args.iter().map(|s| s.to_string()));
    out
}

fn with_helm_kube_context(args: &[&str]) -> Vec<String> {
    let ctx = kube_context_from_env();
    with_helm_kube_context_value(args, ctx.as_deref())
}

fn with_helm_kube_context_value(args: &[&str], context: Option<&str>) -> Vec<String> {
    let Some(ctx) = context.filter(|ctx| !ctx.is_empty()) else {
        return args.iter().map(|s| s.to_string()).collect();
    };
    if args.first() == Some(&"repo") {
        return args.iter().map(|s| s.to_string()).collect();
    }

    let mut out = Vec::with_capacity(args.len() + 2);
    if let Some((command, rest)) = args.split_first() {
        out.push((*command).to_string());
        out.push("--kube-context".to_string());
        out.push(ctx.to_string());
        out.extend(rest.iter().map(|s| s.to_string()));
    } else {
        out.push("--kube-context".to_string());
        out.push(ctx.to_string());
    }
    out
}

/// Build a `Command` for kubectl with `--context` injected when configured.
pub fn kubectl_command(args: &[&str]) -> Command {
    let full = with_kube_context(args);
    let mut cmd = Command::new("kubectl");
    cmd.args(&full);
    cmd
}

#[derive(Args, Debug)]
pub struct LocalArgs {
    #[command(subcommand)]
    pub command: LocalCommands,

    /// Kubernetes context to use for all kubectl commands (e.g. "colima").
    /// Defaults to the selected cluster provider's context. Global: applies to
    /// every `hops local` subcommand and may be given before or after the
    /// subcommand.
    #[arg(long, global = true)]
    pub context: Option<String>,

    /// How Kubernetes nodes are provisioned: `kind`, `dory` (product k3s), `colima`.
    /// Preferred Mac hostPath path: `--cluster-provider kind --docker-provider dory`.
    #[arg(long = "cluster-provider", global = true, value_enum)]
    pub cluster_provider: Option<backend::ClusterProvider>,

    /// Container engine for kind/tools: `dory`, `colima`, `docker`.
    #[arg(long = "docker-provider", global = true, value_enum)]
    pub docker_provider: Option<backend::DockerProvider>,

    /// Named hops-managed kind cluster (`kind create --name`). Default `hops`
    /// → kube context `kind-hops`. Distinct from workspace `--name`.
    #[arg(long = "cluster-name", global = true, value_name = "NAME")]
    pub cluster_name: Option<String>,

    /// Dory desktop integration name (kube context + docker context).
    /// Defaults to `hops-dory`. Persisted under `~/.hops/local/dory-name`.
    /// Only used with cluster-provider dory.
    ///
    /// Named `--dory-name` (not `--name`) so it never collides with workspace
    /// `--name` on `hops local down|status|gitops environment`.
    #[arg(long = "dory-name", global = true, value_name = "NAME")]
    pub dory_name: Option<String>,

    /// Deprecated one-dimensional provider selection. Use
    /// --cluster-provider and --docker-provider instead.
    #[arg(long, global = true, value_enum)]
    pub backend: Option<backend::Backend>,
}

#[derive(Subcommand, Debug)]
pub enum LocalCommands {
    /// Install local cluster-provider tools via Homebrew
    Install,
    /// Reset local Kubernetes state (colima: k8s reset; kind: recreate cluster)
    Reset,
    /// Start local k8s and ensure Crossplane control plane (skips helm when already healthy)
    Start(start::StartArgs),
    /// Resize the local cluster VM without destroying cluster state (colima cluster provider only)
    Resize(resize::ResizeArgs),
    /// Check what `hops local start` set up and report drift
    Doctor,
    /// Bring down a local workbench workspace
    Down(down::DownArgs),
    /// Show local workbench workspace status and app URLs
    Status(status::StatusArgs),
    /// Local gitops: `cluster` (shared CP) or `environment` (app namespaces)
    Gitops(gitops::GitopsArgs),
    /// Configure crossplane-contrib provider-family-aws and AWS ProviderConfig
    Aws(aws::AwsArgs),
    /// Configure Wildbit Cloudflare DNS provider and ProviderConfig
    Cloudflare(cloudflare::CloudflareArgs),
    /// Configure crossplane-contrib provider-upjet-github and GitHub ProviderConfig
    Github(github::GithubArgs),
    /// Configure crossplane-contrib provider-upjet-zitadel and Zitadel ProviderConfig
    Zitadel(zitadel::ZitadelArgs),
    /// Configure hops-ops/provider-listmonk and Listmonk ProviderConfig
    Listmonk(listmonk::ListmonkArgs),
    /// Destroy the local cluster
    Destroy,
    /// Uninstall local cluster-provider tools
    Uninstall(uninstall::UninstallArgs),
}

pub fn run(args: &LocalArgs) -> Result<(), Box<dyn Error>> {
    if let LocalCommands::Gitops(gitops::GitopsArgs {
        command: gitops::GitopsCommands::Cluster(cluster),
    }) = &args.command
    {
        return gitops::run_cluster(
            cluster,
            workbench::definition::ClusterOverrides {
                cluster_provider: args.cluster_provider,
                docker_provider: args.docker_provider,
                legacy_backend: args.backend,
                cluster_name: args.cluster_name.as_deref(),
                context: args.context.as_deref(),
                dory_name: args.dory_name.as_deref(),
            },
        );
    }

    if let Some(name) = args
        .dory_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        backend::persist_dory_context_name(name)?;
    }

    let (cluster_provider, docker_provider) = match args.backend {
        Some(_) if args.cluster_provider.is_some() || args.docker_provider.is_some() => {
            return Err(
                "deprecated --backend cannot be combined with --cluster-provider or --docker-provider"
                    .into(),
            );
        }
        Some(legacy) => {
            let pair = backend::providers::provider_pair_for_legacy_backend(legacy);
            log::warn!(
                "--backend is deprecated; use --cluster-provider {} --docker-provider {}",
                pair.cluster,
                pair.docker
            );
            (Some(pair.cluster), Some(pair.docker))
        }
        None => (args.cluster_provider, args.docker_provider),
    };

    let explicit_context = args.context.as_deref().filter(|ctx| !ctx.is_empty());
    let backend = backend::activate_with_providers(
        cluster_provider,
        docker_provider,
        args.cluster_name.as_deref(),
        explicit_context,
    )?;

    match &args.command {
        LocalCommands::Install => install::run(backend),
        LocalCommands::Reset => reset::run(backend),
        LocalCommands::Start(start_args) => start::run(backend, start_args),
        LocalCommands::Resize(resize_args) => resize::run(backend, resize_args),
        LocalCommands::Doctor => doctor::run(),
        LocalCommands::Down(down_args) => down::run(down_args),
        LocalCommands::Status(status_args) => status::run(status_args),
        LocalCommands::Gitops(gitops_args) => gitops::run_environment_command(gitops_args),
        LocalCommands::Aws(aws_args) => aws::run(aws_args),
        LocalCommands::Cloudflare(cloudflare_args) => cloudflare::run(cloudflare_args),
        LocalCommands::Github(github_args) => github::run(github_args),
        LocalCommands::Zitadel(zitadel_args) => zitadel::run(zitadel_args),
        LocalCommands::Listmonk(listmonk_args) => listmonk::run(listmonk_args),
        LocalCommands::Destroy => destroy::run(backend),
        LocalCommands::Uninstall(uninstall_args) => uninstall::run(backend, uninstall_args),
    }
}

/// Run an external command with inherited stdio. Fails on non-zero exit.
/// For kubectl commands, automatically injects `--context` when configured.
pub fn run_cmd(program: &str, args: &[&str]) -> Result<(), Box<dyn Error>> {
    if program == "kubectl" {
        let full = with_kube_context(args);
        let refs: Vec<&str> = full.iter().map(|s| s.as_str()).collect();
        return run_cmd_with_logged_args(program, &refs, &refs);
    }
    if program == "helm" {
        let full = with_helm_kube_context(args);
        let refs: Vec<&str> = full.iter().map(|s| s.as_str()).collect();
        return run_cmd_with_logged_args(program, &refs, &refs);
    }
    run_cmd_with_logged_args(program, args, args)
}

/// Run an external command and capture stdout.
/// For kubectl/helm commands, automatically injects the active context when
/// configured.
pub fn run_cmd_output(program: &str, args: &[&str]) -> Result<String, Box<dyn Error>> {
    if program == "kubectl" {
        let full = with_kube_context(args);
        log::debug!("Running: {} {}", program, full.join(" "));
        let output = Command::new(program).args(&full).output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("{} exited with {}: {}", program, output.status, stderr).into());
        }
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    if program == "helm" {
        let full = with_helm_kube_context(args);
        log::debug!("Running: {} {}", program, full.join(" "));
        let output = Command::new(program).args(&full).output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("{} exited with {}: {}", program, output.status, stderr).into());
        }
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }

    log::debug!("Running: {} {}", program, args.join(" "));
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{} exited with {}: {}", program, output.status, stderr).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_cmd_with_logged_args(
    program: &str,
    args: &[&str],
    logged_args: &[&str],
) -> Result<(), Box<dyn Error>> {
    log::debug!("Running: {} {}", program, logged_args.join(" "));
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !status.success() {
        return Err(format!("{} exited with {}", program, status).into());
    }
    Ok(())
}

pub fn repo_cache_path(org: &str, repo: &str) -> Result<PathBuf, Box<dyn Error>> {
    Ok(local_state_dir()?.join(REPO_CACHE_DIR).join(org).join(repo))
}

pub(crate) fn local_state_dir() -> Result<PathBuf, Box<dyn Error>> {
    let home = std::env::var("HOME")
        .map_err(|_| "HOME is not set; unable to determine local state directory")?;
    Ok(Path::new(&home).join(LOCAL_STATE_DIR))
}

pub(crate) fn command_exists(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {} >/dev/null 2>&1", program)])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Poll until the Kubernetes API server is reachable.
pub(crate) fn wait_for_kubernetes() -> Result<(), Box<dyn Error>> {
    log::info!("Waiting for Kubernetes API...");
    // ~10 minutes — nested-virt apiserver can stay overloaded after package install.
    for _ in 0..120 {
        if kubectl_command(&["get", "--raw", "/readyz"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            return Ok(());
        }
        // Fall back to a cheap list if /readyz is denied on some setups.
        if kubectl_command(&["get", "ns", "default"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    Err("Timed out waiting for Kubernetes API".into())
}

/// Pipe a YAML string into `kubectl apply -f -`.
/// Automatically injects `--context` when configured.
///
/// Uses `--validate=false` so a slow/overloaded API server (common on nested
/// virt CI while Crossplane is warming) does not fail the apply solely because
/// OpenAPI schema download timed out. Retries a few times for transient
/// connection errors. Captures stderr on failure so callers can classify soft
/// errors (missing CRDs).
pub fn kubectl_apply_stdin(yaml: &str) -> Result<(), Box<dyn Error>> {
    let full = with_kube_context(&["apply", "--validate=false", "-f", "-"]);
    // Nested-virt CI (colima/GHA) can lose the apiserver for minutes after
    // Crossplane/provider install (TLS handshake timeouts). Retry with backoff
    // and re-probe the API between attempts.
    const ATTEMPTS: u32 = 12;
    let mut last_err = String::from("unknown");

    for attempt in 1..=ATTEMPTS {
        let mut child = Command::new("kubectl")
            .args(&full)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if let Some(ref mut stdin) = child.stdin {
            stdin.write_all(yaml.as_bytes())?;
        }

        let output = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stdout.trim().is_empty() {
            print!("{stdout}");
        }
        if output.status.success() {
            if !stderr.trim().is_empty() {
                eprint!("{stderr}");
            }
            return Ok(());
        }
        last_err = format!("{}: {}", output.status, stderr.trim());
        if !stderr.trim().is_empty() {
            eprint!("{stderr}");
        }
        if !is_transient_kubectl_apply_error(&stderr) {
            return Err(format!("kubectl apply exited with {last_err}").into());
        }
        if attempt == ATTEMPTS {
            break;
        }
        log::warn!(
            "kubectl apply failed (attempt {}/{}, {}); waiting for API...",
            attempt,
            ATTEMPTS,
            output.status
        );
        wait_for_kubernetes().map_err(|e| {
            format!("kubectl apply exited with {last_err}; API recovery failed: {e}")
        })?;
        std::thread::sleep(std::time::Duration::from_secs(10));
    }

    Err(format!("kubectl apply exited with {last_err} after retries").into())
}

fn is_transient_kubectl_apply_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    [
        "unable to connect to the server",
        "connection refused",
        "connection reset",
        "connection timed out",
        "i/o timeout",
        "tls handshake timeout",
        "context deadline exceeded",
        "dial tcp",
        "no route to host",
        "service unavailable",
        "gateway timeout",
        "server is currently unable to handle the request",
        "unexpected eof",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Apply a JSON merge patch with `kubectl patch --type merge`.
/// Automatically injects `--context` when configured.
pub fn kubectl_patch_merge(
    resource: &str,
    name: &str,
    namespace: &str,
    patch_json: &str,
) -> Result<(), Box<dyn Error>> {
    let base_args = [
        "patch", resource, name, "-n", namespace, "--type", "merge", "-p", patch_json,
    ];
    let base_logged = [
        "patch",
        resource,
        name,
        "-n",
        namespace,
        "--type",
        "merge",
        "-p",
        "<REDACTED>",
    ];
    let full_args = with_kube_context(&base_args);
    let full_logged = with_kube_context(&base_logged);
    let args_refs: Vec<&str> = full_args.iter().map(|s| s.as_str()).collect();
    let logged_refs: Vec<&str> = full_logged.iter().map(|s| s.as_str()).collect();
    run_cmd_with_logged_args("kubectl", &args_refs, &logged_refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: Vec<String>) -> Vec<String> {
        args
    }

    #[test]
    fn kubectl_args_prepend_context() {
        assert_eq!(
            strings(with_kube_context_value(&["get", "pods"], Some("kind-hops"))),
            vec!["--context", "kind-hops", "get", "pods"]
        );
    }

    #[test]
    fn helm_upgrade_injects_kube_context_after_subcommand() {
        assert_eq!(
            strings(with_helm_kube_context_value(
                &["upgrade", "--install", "crossplane"],
                Some("kind-hops")
            )),
            vec![
                "upgrade",
                "--kube-context",
                "kind-hops",
                "--install",
                "crossplane"
            ]
        );
    }

    #[test]
    fn helm_repo_commands_skip_kube_context() {
        assert_eq!(
            strings(with_helm_kube_context_value(
                &["repo", "update", "crossplane-stable"],
                Some("kind-hops")
            )),
            vec!["repo", "update", "crossplane-stable"]
        );
    }

    /// Regression: workspace `--name` must not populate Dory's `--dory-name`.
    #[test]
    fn workspace_name_does_not_set_dory_name() {
        use clap::Parser;

        #[derive(Parser, Debug)]
        #[command(name = "hops-local-test")]
        struct Cli {
            #[command(flatten)]
            local: LocalArgs,
        }

        let parsed = Cli::try_parse_from([
            "hops-local-test",
            "gitops",
            "environment",
            "./gitops/envs/local",
            "--name",
            "alice",
            "--once",
        ])
        .expect("parse gitops environment --name alice");
        assert!(
            parsed.local.dory_name.is_none(),
            "workspace --name must not set dory_name; got {:?}",
            parsed.local.dory_name
        );
        match parsed.local.command {
            LocalCommands::Gitops(gitops) => match gitops.command {
                gitops::GitopsCommands::Environment(environment) => {
                    assert_eq!(environment.name.as_deref(), Some("alice"));
                    assert!(environment.once);
                }
                other => panic!("expected gitops environment, got {other:?}"),
            },
            other => panic!("expected Gitops, got {other:?}"),
        }
    }

    #[test]
    fn dory_name_flag_is_distinct_from_workspace_name() {
        use clap::Parser;

        #[derive(Parser, Debug)]
        #[command(name = "hops-local-test")]
        struct Cli {
            #[command(flatten)]
            local: LocalArgs,
        }

        let parsed = Cli::try_parse_from([
            "hops-local-test",
            "--dory-name",
            "mine",
            "gitops",
            "environment",
            "./env",
            "--name",
            "bob",
        ])
        .expect("parse --dory-name mine gitops environment --name bob");
        assert_eq!(parsed.local.dory_name.as_deref(), Some("mine"));
        match parsed.local.command {
            LocalCommands::Gitops(gitops) => match gitops.command {
                gitops::GitopsCommands::Environment(environment) => {
                    assert_eq!(environment.name.as_deref(), Some("bob"));
                }
                other => panic!("expected gitops environment, got {other:?}"),
            },
            other => panic!("expected Gitops, got {other:?}"),
        }
    }

    #[test]
    fn gitops_lifecycle_flags_parse_without_interim_commands() {
        use clap::Parser;

        #[derive(Parser, Debug)]
        #[command(name = "hops-local-test")]
        struct Cli {
            #[command(flatten)]
            local: LocalArgs,
        }

        let environment = Cli::try_parse_from([
            "hops-local-test",
            "gitops",
            "environment",
            "--name",
            "feature-auth",
            "--down",
        ])
        .expect("parse Environment teardown without a definition path");
        match environment.local.command {
            LocalCommands::Gitops(gitops::GitopsArgs {
                command: gitops::GitopsCommands::Environment(environment),
            }) => {
                assert!(environment.down);
                assert!(environment.path.is_none());
            }
            other => panic!("expected GitOps Environment, got {other:?}"),
        }

        let cluster = Cli::try_parse_from([
            "hops-local-test",
            "gitops",
            "cluster",
            ".gitops/local/cluster.yaml",
            "--down",
        ])
        .expect("parse Cluster teardown");
        match cluster.local.command {
            LocalCommands::Gitops(gitops::GitopsArgs {
                command: gitops::GitopsCommands::Cluster(cluster),
            }) => assert!(cluster.down),
            other => panic!("expected GitOps Cluster, got {other:?}"),
        }

        for removed in ["up", "open", "stop"] {
            assert!(
                Cli::try_parse_from(["hops-local-test", removed]).is_err(),
                "interim command {removed:?} must stay removed"
            );
        }
    }
}
