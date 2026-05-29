use super::run_cmd_output;
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

    // Drift detector: the Provider must point at its OWN per-provider DRC.
    let rc = jsonpath(&[
        "get",
        "provider.pkg.crossplane.io",
        e.provider_name,
        "-o",
        "jsonpath={.spec.runtimeConfigRef.name}",
    ])
    .unwrap_or_default();
    let rc_ok = rc == e.drc;
    d.check(
        "runtimeConfigRef pinned to its own DRC",
        rc_ok,
        if rc_ok {
            String::new()
        } else {
            format!(
                "runtimeConfigRef is \"{}\" (expected \"{}\") — drifted; provider lacks its cluster-admin ServiceAccount and cannot observe XRs",
                if rc.is_empty() { "<unset>" } else { &rc },
                e.drc
            )
        },
    );

    let drc_ok = exists(&["get", "deploymentruntimeconfig", e.drc]);
    d.check(
        "DeploymentRuntimeConfig present",
        drc_ok,
        if drc_ok {
            String::new()
        } else {
            format!("DeploymentRuntimeConfig \"{}\" missing", e.drc)
        },
    );

    // cluster-admin ClusterRoleBinding bound to the pinned SA.
    let role = jsonpath(&[
        "get",
        "clusterrolebinding",
        e.binding,
        "-o",
        "jsonpath={.roleRef.name}",
    ]);
    let subjects = jsonpath(&[
        "get",
        "clusterrolebinding",
        e.binding,
        "-o",
        "jsonpath={.subjects[?(@.kind==\"ServiceAccount\")].name}",
    ]);
    let binding_ok = role.as_deref() == Some("cluster-admin")
        && subjects
            .as_deref()
            .map(|s| s.split_whitespace().any(|n| n == e.sa))
            .unwrap_or(false);
    d.check(
        "cluster-admin binding -> pinned SA",
        binding_ok,
        if binding_ok {
            String::new()
        } else {
            format!(
                "ClusterRoleBinding \"{}\" missing or not binding cluster-admin to ServiceAccount \"{}\"",
                e.binding, e.sa
            )
        },
    );

    let pc_ok = exists(&[
        "get",
        e.pc_resource,
        e.pc_name,
        "-n",
        e.pc_namespace,
    ]);
    d.check(
        "ProviderConfig present",
        pc_ok,
        if pc_ok {
            String::new()
        } else {
            format!("{}/{} missing in namespace {}", e.pc_resource, e.pc_name, e.pc_namespace)
        },
    );
}

fn provider_condition(provider: &str, cond: &str) -> String {
    jsonpath(&[
        "get",
        "provider.pkg.crossplane.io",
        provider,
        "-o",
        &format!("jsonpath={{.status.conditions[?(@.type==\"{}\")].status}}", cond),
    ])
    .unwrap_or_default()
}

fn cond_detail(cond: &str, status: &str) -> String {
    if status == "True" {
        String::new()
    } else {
        format!("{}={}", cond, if status.is_empty() { "<none>" } else { status })
    }
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
