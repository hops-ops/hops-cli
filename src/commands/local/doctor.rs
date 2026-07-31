use super::{run_cmd_output, MANAGED_BY_LABEL, PROVIDER_INSTALL_MANAGED_BY};
use std::error::Error;

/// `hops local doctor` — verify what `hops local start` set up on the current
/// cluster and surface drift.
///
/// The motivating failure: a provider's `runtimeConfigRef` can silently revert
/// to `default` (e.g. when a `dependsOn` re-resolution re-owns the Provider),
/// dropping the pinned cluster-admin ServiceAccount. The provider then can't
/// observe XRs through the in-cluster `default` ProviderConfig, breaking the
/// consumer-Observe pattern with no obvious signal. This command makes that
/// drift (and missing DRCs / bindings / ProviderConfigs) visible.
pub fn run() -> Result<(), Box<dyn Error>> {
    match std::env::var(super::HOPS_KUBE_CONTEXT_ENV) {
        Ok(ctx) if !ctx.is_empty() => {
            log::info!("Checking local cluster setup (context: {})...", ctx)
        }
        _ => log::info!("Checking local cluster setup (current kube context)..."),
    }

    let mut d = Doctor::new();

    d.section("Crossplane");
    let xp = deployment_available("crossplane-system", "crossplane");
    d.check(
        "crossplane deployment Available",
        xp,
        if xp {
            String::new()
        } else {
            "crossplane is not Available in crossplane-system".into()
        },
    );

    // The two providers `hops local start` bootstraps, each with its OWN
    // per-provider cluster-admin DRC (never shared).
    check_provider(
        &mut d,
        &ProviderExpectation {
            title: "provider-kubernetes",
            provider_name: "crossplane-contrib-provider-kubernetes",
            drc: "local-dev-kubernetes",
            binding: "local-dev-kubernetes-cluster-admin",
            sa: "local-dev-kubernetes",
            pc_resource: "providerconfig.kubernetes.m.crossplane.io",
            pc_name: "default",
            pc_namespace: "default",
        },
    );
    check_provider(
        &mut d,
        &ProviderExpectation {
            title: "provider-helm",
            provider_name: "crossplane-contrib-provider-helm",
            drc: "local-dev-helm",
            binding: "local-dev-helm-cluster-admin",
            sa: "local-dev-helm",
            pc_resource: "providerconfig.helm.m.crossplane.io",
            pc_name: "default",
            pc_namespace: "default",
        },
    );

    d.section("Registry");
    let reg = deployment_available("crossplane-system", "registry");
    d.check(
        "local package registry Available",
        reg,
        if reg {
            String::new()
        } else {
            "registry deployment not Available in crossplane-system".into()
        },
    );
    let push = super::package_install::registry_push();
    let push_ok = package_push_registry_reachable(&push);
    d.check(
        &format!("package push registry reachable ({push})"),
        push_ok,
        if push_ok {
            String::new()
        } else {
            format!("expected HTTPS registry at {push} for docker push")
        },
    );

    d.print();

    if d.ok() {
        Ok(())
    } else {
        Err(format!(
            "{} check(s) failed. If a provider drifted off its DRC, re-run `hops local start` to re-apply the bootstrap.",
            d.failed_count()
        )
        .into())
    }
}

/// What a bootstrapped provider should look like once `hops local start` ran.
struct ProviderExpectation<'a> {
    title: &'a str,
    provider_name: &'a str,
    drc: &'a str,
    binding: &'a str,
    sa: &'a str,
    pc_resource: &'a str,
    pc_name: &'a str,
    pc_namespace: &'a str,
}

fn check_provider(d: &mut Doctor, e: &ProviderExpectation) {
    d.section(e.title);

    if !exists(&["get", "provider.pkg.crossplane.io", e.provider_name]) {
        d.check(
            "Provider installed",
            false,
            format!("Provider '{}' not found", e.provider_name),
        );
        return;
    }

    let installed = provider_condition(e.provider_name, "Installed");
    d.check(
        "Provider installed",
        installed == "True",
        cond_detail("Installed", &installed),
    );

    let healthy = provider_condition(e.provider_name, "Healthy");
    d.check(
        "Provider healthy",
        healthy == "True",
        cond_detail("Healthy", &healthy),
    );

    // A provider installed via `hops provider install` (e.g. a fork built from
    // source) is labeled and uses the `<provider>-runtime` convention instead of
    // the bootstrap `local-dev-<provider>` DRC. Recognize that so it reads as an
    // intentional custom install, not drift — while still verifying cluster-admin.
    let custom_install = provider_label(e.provider_name, MANAGED_BY_LABEL).as_deref()
        == Some(PROVIDER_INSTALL_MANAGED_BY);
    let (exp_drc, exp_binding, exp_sa) = if custom_install {
        (
            format!("{}-runtime", e.provider_name),
            format!("{}-cluster-admin", e.provider_name),
            e.provider_name.to_string(),
        )
    } else {
        (e.drc.to_string(), e.binding.to_string(), e.sa.to_string())
    };
    d.check(
        "install mode",
        true,
        if custom_install {
            format!(
                "custom install via `hops provider install` (package: {})",
                provider_package(e.provider_name)
            )
        } else {
            "bootstrap (hops local start)".to_string()
        },
    );

    // The Provider must point at its expected per-provider DRC.
    let rc = jsonpath(&[
        "get",
        "provider.pkg.crossplane.io",
        e.provider_name,
        "-o",
        "jsonpath={.spec.runtimeConfigRef.name}",
    ])
    .unwrap_or_default();
    let rc_ok = rc == exp_drc;
    d.check(
        "runtimeConfigRef pinned to its own DRC",
        rc_ok,
        if rc_ok {
            String::new()
        } else {
            format!(
                "runtimeConfigRef is \"{}\" (expected \"{}\") — drifted; provider lacks its cluster-admin ServiceAccount and cannot observe XRs",
                if rc.is_empty() { "<unset>" } else { &rc },
                exp_drc
            )
        },
    );

    let drc_ok = exists(&["get", "deploymentruntimeconfig", exp_drc.as_str()]);
    d.check(
        "DeploymentRuntimeConfig present",
        drc_ok,
        if drc_ok {
            String::new()
        } else {
            format!("DeploymentRuntimeConfig \"{}\" missing", exp_drc)
        },
    );

    // cluster-admin ClusterRoleBinding bound to the pinned SA.
    let role = jsonpath(&[
        "get",
        "clusterrolebinding",
        exp_binding.as_str(),
        "-o",
        "jsonpath={.roleRef.name}",
    ]);
    let subjects = jsonpath(&[
        "get",
        "clusterrolebinding",
        exp_binding.as_str(),
        "-o",
        "jsonpath={.subjects[?(@.kind==\"ServiceAccount\")].name}",
    ]);
    let binding_ok = role.as_deref() == Some("cluster-admin")
        && subjects
            .as_deref()
            .map(|s| s.split_whitespace().any(|n| n == exp_sa.as_str()))
            .unwrap_or(false);
    d.check(
        "cluster-admin binding -> pinned SA",
        binding_ok,
        if binding_ok {
            String::new()
        } else {
            format!(
                "ClusterRoleBinding \"{}\" missing or not binding cluster-admin to ServiceAccount \"{}\"",
                exp_binding, exp_sa
            )
        },
    );

    let pc_ok = exists(&["get", e.pc_resource, e.pc_name, "-n", e.pc_namespace]);
    d.check(
        "ProviderConfig present",
        pc_ok,
        if pc_ok {
            String::new()
        } else {
            format!(
                "{}/{} missing in namespace {}",
                e.pc_resource, e.pc_name, e.pc_namespace
            )
        },
    );
}

fn provider_condition(provider: &str, cond: &str) -> String {
    jsonpath(&[
        "get",
        "provider.pkg.crossplane.io",
        provider,
        "-o",
        &format!(
            "jsonpath={{.status.conditions[?(@.type==\"{}\")].status}}",
            cond
        ),
    ])
    .unwrap_or_default()
}

fn cond_detail(cond: &str, status: &str) -> String {
    if status == "True" {
        String::new()
    } else {
        format!(
            "{}={}",
            cond,
            if status.is_empty() { "<none>" } else { status }
        )
    }
}

/// Read a metadata label off a Provider via `-o json` (robust against the dotted
/// label key that jsonpath escaping mishandles).
fn provider_label(provider: &str, key: &str) -> Option<String> {
    let json = run_cmd_output(
        "kubectl",
        &["get", "provider.pkg.crossplane.io", provider, "-o", "json"],
    )
    .ok()?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    v.get("metadata")?
        .get("labels")?
        .get(key)?
        .as_str()
        .map(|s| s.to_string())
}

/// The Provider's spec.package (image ref) — shown for custom installs.
fn provider_package(provider: &str) -> String {
    jsonpath(&[
        "get",
        "provider.pkg.crossplane.io",
        provider,
        "-o",
        "jsonpath={.spec.package}",
    ])
    .unwrap_or_default()
}

/// Whether `push` is a host-loopback address (kind/colima NodePort).
///
/// Dory pushes via the engine docker bridge (`{dory-k8s-ip}:30500`); that IP is
/// not reachable from the Mac, so host curl is the wrong plane and can hang on
/// TCP timeout (~75s) before the engine probe runs.
fn push_host_is_loopback(push: &str) -> bool {
    let host = push.split(':').next().unwrap_or(push);
    host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1"
}

/// Probe the package push registry on the plane docker will use.
///
/// - Loopback (`localhost:30500`): short host curl (kind/colima).
/// - Otherwise (dory bridge IP): engine-network `docker run` only.
fn package_push_registry_reachable(push: &str) -> bool {
    let url = format!("https://{push}/v2/");
    if push_host_is_loopback(push) {
        return super::run_cmd_output(
            "curl",
            &["-skf", "--connect-timeout", "2", "--max-time", "3", &url],
        )
        .is_ok();
    }
    // Engine dockerd can reach the k3s NodePort on the docker bridge.
    super::run_cmd_output(
        "docker",
        &[
            "run",
            "--rm",
            "alpine:latest",
            "wget",
            "-qO-",
            "--no-check-certificate",
            "--timeout=5",
            &url,
        ],
    )
    .is_ok()
}

/// True when `kubectl get <args>` finds the resource. Uses `--ignore-not-found`
/// so a missing resource is `false` rather than an error.
fn exists(get_args: &[&str]) -> bool {
    let mut args = get_args.to_vec();
    args.extend_from_slice(&["--ignore-not-found", "-o", "name"]);
    run_cmd_output("kubectl", &args)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Run a kubectl query, returning the trimmed stdout or `None` if kubectl errors
/// (e.g. the resource does not exist).
fn jsonpath(args: &[&str]) -> Option<String> {
    run_cmd_output("kubectl", args)
        .ok()
        .map(|s| s.trim().to_string())
}

fn deployment_available(ns: &str, name: &str) -> bool {
    jsonpath(&[
        "get",
        "deployment",
        name,
        "-n",
        ns,
        "-o",
        "jsonpath={.status.conditions[?(@.type==\"Available\")].status}",
    ])
    .map(|s| s == "True")
    .unwrap_or(false)
}

enum Entry {
    Section(String),
    Check {
        label: String,
        ok: bool,
        detail: String,
    },
}

struct Doctor {
    entries: Vec<Entry>,
}

impl Doctor {
    fn new() -> Self {
        Doctor {
            entries: Vec::new(),
        }
    }

    fn section(&mut self, title: &str) {
        self.entries.push(Entry::Section(title.to_string()));
    }

    fn check(&mut self, label: &str, ok: bool, detail: String) {
        self.entries.push(Entry::Check {
            label: label.to_string(),
            ok,
            detail,
        });
    }

    fn failed_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e, Entry::Check { ok: false, .. }))
            .count()
    }

    fn ok(&self) -> bool {
        self.failed_count() == 0
    }

    fn print(&self) {
        for entry in &self.entries {
            match entry {
                Entry::Section(title) => println!("\n{}", title),
                Entry::Check { label, ok, detail } => {
                    let mark = if *ok { "✓" } else { "✗" };
                    if detail.is_empty() {
                        println!("  {} {}", mark, label);
                    } else {
                        println!("  {} {} — {}", mark, label, detail);
                    }
                }
            }
        }
        if self.ok() {
            println!("\nAll checks passed.");
        } else {
            println!("\n{} issue(s) found.", self.failed_count());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_host_loopback_for_kind_colima() {
        assert!(push_host_is_loopback("localhost:30500"));
        assert!(push_host_is_loopback("127.0.0.1:30500"));
        assert!(push_host_is_loopback("[::1]:30500"));
    }

    #[test]
    fn push_host_not_loopback_for_dory_bridge() {
        assert!(!push_host_is_loopback("192.168.215.2:30500"));
        assert!(!push_host_is_loopback("10.43.0.1:30500"));
    }
}
