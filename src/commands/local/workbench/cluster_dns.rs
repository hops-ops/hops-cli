//! Loopback IPs + `/etc/hosts` so Service FQDNs resolve on the host.
//!
//! For each Service in a hops-local workspace namespace:
//! 1. Allocate a unique loopback IP in `127.53.0.0/16`
//! 2. (macOS) alias it on `lo0` (privileged)
//! 3. Maintain a managed block in `/etc/hosts` mapping the **real k8s FQDN**
//!    `svc.namespace.svc.cluster.local` → that IP
//! 4. Callers port-forward with `--address <ip>` on the **cluster service port**
//!
//! Privilege is expected: one admin elevation installs hosts + aliases so
//! in-cluster URLs work on the laptop. Daily `up`/`status` reuses state when
//! possible; elevation only when the OS config is missing/stale.
//!
//! Result: `curl http://e2e-ui-api.dogfood.svc.cluster.local:8791`

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Managed block markers in `/etc/hosts` (and the runtime mirror).
pub const HOSTS_BEGIN: &str = "# BEGIN hops-local-dns (managed by hops local — do not edit)";
pub const HOSTS_END: &str = "# END hops-local-dns";

/// Loopback range reserved for hops (avoids common 127.0.0.1 tooling).
pub const DNS_IP_PREFIX: &str = "127.53";

/// macOS stub resolver for `*.svc.cluster.local` (bypasses mDNS 5s timeout).
pub const MACOS_LOCAL_DNS_PORT: u16 = 53535;
pub const MACOS_RESOLVER_PATH: &str = "/etc/resolver/svc.cluster.local";

/// Kubernetes-style FQDN used by in-cluster clients and our hosts entries.
pub fn cluster_dns_name(service: &str, namespace: &str) -> String {
    format!("{service}.{namespace}.svc.cluster.local")
}

/// Short form also useful in browsers / curl.
pub fn short_dns_name(service: &str, namespace: &str) -> String {
    format!("{service}.{namespace}")
}

/// URL using cluster DNS + real service port.
pub fn format_dns_url(service: &str, namespace: &str, port: u16) -> String {
    format!("http://{}:{port}", cluster_dns_name(service, namespace))
}

/// Stable key for IP allocation: `namespace/service`.
pub fn alloc_key(namespace: &str, service: &str) -> String {
    format!("{namespace}/{service}")
}

/// Parse `127.53.X.Y` → (X, Y) for sequential allocation.
fn parse_hops_ip(ip: &str) -> Option<(u8, u8)> {
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() != 4 || parts[0] != "127" || parts[1] != "53" {
        return None;
    }
    Some((parts[2].parse().ok()?, parts[3].parse().ok()?))
}

fn format_hops_ip(mid: u8, low: u8) -> String {
    format!("{DNS_IP_PREFIX}.{mid}.{low}")
}

/// Next free IP after existing bindings (skips 127.53.0.0 and 127.53.0.1).
pub fn next_hops_ip(used: &[&str]) -> String {
    let mut max = (0u8, 1u8); // start after .0.1
    for ip in used {
        if let Some(p) = parse_hops_ip(ip) {
            if p > max {
                max = p;
            }
        }
    }
    let (mut mid, mut low) = max;
    if low == 255 {
        mid = mid.saturating_add(1);
        low = 2;
    } else {
        low += 1;
    }
    format_hops_ip(mid, low)
}

/// Ensure each service has an IP; returns service → IP (sorted insertion order).
pub fn allocate_service_ips(
    namespace: &str,
    services: &[super::net::ServiceEndpoint],
    existing: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut used: Vec<String> = existing.values().cloned().collect();
    for svc in services {
        let key = alloc_key(namespace, &svc.name);
        if let Some(ip) = existing.get(&key) {
            out.insert(svc.name.clone(), ip.clone());
            continue;
        }
        // Prefer reusing if this service name already mapped under same ns in existing values
        let ip = next_hops_ip(&used.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        used.push(ip.clone());
        out.insert(svc.name.clone(), ip);
    }
    out
}

/// Build hosts-file lines for one workspace (no markers).
///
/// Each line maps one loopback IP to:
/// - full k8s FQDN (`svc.ns.svc.cluster.local`) — primary, matches in-cluster config
/// - mDNS-safe twin without trailing `.local` (`svc.ns.svc.cluster`) — macOS reliability
/// - short `svc.ns`
pub fn hosts_lines_for_workspace(
    namespace: &str,
    service_ips: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (svc, ip) in service_ips {
        let fqdn = cluster_dns_name(svc, namespace);
        // Strip final ".local" for tools that send *.local to mDNS only.
        let no_mdns = fqdn.trim_end_matches(".local");
        let short = short_dns_name(svc, namespace);
        lines.push(format!("{ip} {fqdn} {no_mdns} {short}"));
    }
    lines
}

/// Merge workspace lines into a full hosts file body, replacing our managed block.
pub fn merge_hosts_file(existing: &str, all_workspace_lines: &[String]) -> String {
    let stripped = strip_managed_block(existing);
    let mut out = stripped.trim_end().to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(HOSTS_BEGIN);
    out.push('\n');
    if all_workspace_lines.is_empty() {
        out.push_str("# (no hops local workspaces)\n");
    } else {
        for line in all_workspace_lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(HOSTS_END);
    out.push('\n');
    out
}

/// Remove the hops-managed block from hosts content.
pub fn strip_managed_block(existing: &str) -> String {
    let mut out = String::new();
    let mut in_block = false;
    for line in existing.lines() {
        if line.trim() == HOSTS_BEGIN {
            in_block = true;
            continue;
        }
        if line.trim() == HOSTS_END {
            in_block = false;
            continue;
        }
        if !in_block {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Global IP allocation state under ~/.hops/local/runtime/dns-ip-alloc.json
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DnsIpAlloc {
    /// key `namespace/service` → `127.53.x.y`
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

fn alloc_path(state_dir: &Path) -> PathBuf {
    state_dir.join("runtime").join("dns-ip-alloc.json")
}

pub fn load_ip_alloc(state_dir: &Path) -> DnsIpAlloc {
    let path = alloc_path(state_dir);
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_ip_alloc(state_dir: &Path, alloc: &DnsIpAlloc) -> Result<(), Box<dyn Error>> {
    let path = alloc_path(state_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(alloc)?)?;
    Ok(())
}

/// Update alloc bindings for this namespace's services; drop stale services in ns.
pub fn sync_alloc_for_namespace(
    state_dir: &Path,
    namespace: &str,
    services: &[super::net::ServiceEndpoint],
) -> Result<BTreeMap<String, String>, Box<dyn Error>> {
    let mut alloc = load_ip_alloc(state_dir);
    // Drop bindings for services no longer present in this namespace
    let wanted: std::collections::BTreeSet<String> = services
        .iter()
        .map(|s| alloc_key(namespace, &s.name))
        .collect();
    alloc.bindings.retain(|k, _| {
        if let Some(ns) = k.split('/').next() {
            if ns == namespace {
                return wanted.contains(k);
            }
        }
        true
    });
    let service_ips = allocate_service_ips(namespace, services, &alloc.bindings);
    for (svc, ip) in &service_ips {
        alloc.bindings.insert(alloc_key(namespace, svc), ip.clone());
    }
    save_ip_alloc(state_dir, &alloc)?;
    Ok(service_ips)
}

/// Rebuild /etc/hosts managed block from **all** workspace runtimes that use dns mode.
/// `workspace_blocks`: list of (namespace, service→ip).
pub fn rebuild_hosts_from_blocks(
    workspace_blocks: &[(String, BTreeMap<String, String>)],
) -> Result<(), Box<dyn Error>> {
    let mut lines = Vec::new();
    for (ns, ips) in workspace_blocks {
        lines.extend(hosts_lines_for_workspace(ns, ips));
    }
    let current = fs::read_to_string("/etc/hosts").unwrap_or_default();
    let merged = merge_hosts_file(&current, &lines);
    // Combined with aliases in apply_privileged_dns_config when possible.
    write_etc_hosts(&merged)?;
    Ok(())
}

/// True when `/etc/hosts` managed block + lo0 aliases already match desired config.
/// Used to skip admin prompts on subsequent `up`/`status`.
pub fn dns_os_config_present(hosts_body: &str, loopback_ips: &[String]) -> bool {
    let current = fs::read_to_string("/etc/hosts").unwrap_or_default();
    let desired_block = extract_managed_block(hosts_body);
    let current_block = extract_managed_block(&current);
    if desired_block.is_empty() || desired_block != current_block {
        return false;
    }
    if !cfg!(target_os = "macos") {
        return true;
    }
    if !macos_resolver_present() {
        return false;
    }
    let lo0 = Command::new("ifconfig")
        .arg("lo0")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    loopback_ips
        .iter()
        .all(|ip| ip.is_empty() || ip == "127.0.0.1" || lo0.contains(ip.as_str()))
}

fn macos_resolver_present() -> bool {
    if !cfg!(target_os = "macos") {
        return true;
    }
    let Ok(text) = fs::read_to_string(MACOS_RESOLVER_PATH) else {
        return false;
    };
    text.contains("127.0.0.1") && text.contains(&MACOS_LOCAL_DNS_PORT.to_string())
}

fn extract_managed_block(hosts: &str) -> String {
    let mut out = String::new();
    let mut in_block = false;
    for line in hosts.lines() {
        let t = line.trim();
        if t == HOSTS_BEGIN {
            in_block = true;
            continue;
        }
        if t == HOSTS_END {
            break;
        }
        if in_block {
            out.push_str(t);
            out.push('\n');
        }
    }
    out
}

/// Apply hosts file **and** macOS lo0 aliases in **one** elevation when needed.
///
/// This is the main privileged entry for cluster DNS setup.
/// Skips elevation entirely when OS config already matches (no re-prompt).
pub fn apply_privileged_dns_config(
    hosts_body: &str,
    loopback_ips: &[String],
) -> Result<(), Box<dyn Error>> {
    if dns_os_config_present(hosts_body, loopback_ips) {
        log::debug!("cluster DNS OS config already present; skipping admin prompt");
        return Ok(());
    }

    let tmp = std::env::temp_dir().join(format!("hops-hosts-{}.tmp", std::process::id()));
    fs::write(&tmp, hosts_body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644));
    }
    let tmp_s = tmp.to_string_lossy().into_owned();

    let mut shell = String::new();
    shell.push_str(&format!("cp '{tmp_s}' /etc/hosts && chmod 644 /etc/hosts"));
    if cfg!(target_os = "macos") {
        for ip in loopback_ips {
            if ip == "127.0.0.1" || ip.is_empty() {
                continue;
            }
            // Idempotent: ignore failure if alias already exists.
            shell.push_str(&format!(
                " && (ifconfig lo0 | grep -q '{ip}' || ifconfig lo0 alias {ip} netmask 0xff000000)"
            ));
        }
        // Bypass mDNS 5s timeout for *.svc.cluster.local via a local stub DNS.
        let port = MACOS_LOCAL_DNS_PORT;
        shell.push_str(&format!(
            " && mkdir -p /etc/resolver && printf 'nameserver 127.0.0.1\\nport {port}\\n' > '{MACOS_RESOLVER_PATH}'"
        ));
        shell.push_str(
            " ; dscacheutil -flushcache 2>/dev/null; killall -HUP mDNSResponder 2>/dev/null; true",
        );
    }

    log::info!(
        "Configuring cluster DNS on this machine (admin required once): /etc/hosts + loopback aliases"
    );
    let result = run_privileged_shell(&shell, PrivilegedPrompt::InteractiveOnce);
    let _ = fs::remove_file(&tmp);
    result.map_err(|e| {
        format!(
            "cluster DNS needs admin privileges to write /etc/hosts (and lo0 aliases on macOS).\n\
             {e}\n\
             Re-run `hops local status` and approve the **single** prompt,\n\
             or grant passwordless sudo for hops on this machine."
        )
        .into()
    })
}

/// Write /etc/hosts only (used when aliases already present).
pub fn write_etc_hosts(content: &str) -> Result<(), Box<dyn Error>> {
    apply_privileged_dns_config(content, &[])
}

/// Whether elevation may prompt the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegedPrompt {
    /// `sudo -n` only — never open a password dialog (cleanup / best-effort).
    Never,
    /// At most one interactive elevation (sudo **or** macOS GUI, not both).
    InteractiveOnce,
}

/// Run a shell script with elevation.
///
/// **Never** cascades multiple password prompts: passwordless sudo first, then
/// exactly one interactive path (TTY → `sudo`, else macOS → `osascript`).
pub fn run_privileged_shell(script: &str, prompt: PrivilegedPrompt) -> Result<(), Box<dyn Error>> {
    // 1) passwordless sudo (cached ticket after a recent successful elevation)
    let status = Command::new("sudo")
        .args(["-n", "sh", "-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if status.as_ref().map(|s| s.success()).unwrap_or(false) {
        return Ok(());
    }

    if matches!(prompt, PrivilegedPrompt::Never) {
        return Err("admin elevation required (non-interactive; sudo -n failed)".into());
    }

    // 2) Exactly one interactive method — do not fall through to a second prompt.
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        let status = Command::new("sudo")
            .args(["sh", "-c", script])
            .status()
            .map_err(|e| format!("sudo failed to start: {e}"))?;
        if status.success() {
            return Ok(());
        }
        return Err("admin elevation denied or failed (sudo)".into());
    }

    if cfg!(target_os = "macos") {
        // GUI admin dialog (IDEs / agents without a TTY). Escape for AppleScript.
        let escaped = script
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "; ");
        let applescript = format!("do shell script \"{escaped}\" with administrator privileges");
        let status = Command::new("osascript")
            .args(["-e", &applescript])
            .status()
            .map_err(|e| format!("osascript failed to start: {e}"))?;
        if status.success() {
            return Ok(());
        }
        return Err("admin elevation denied or failed (macOS dialog)".into());
    }

    Err("admin elevation required but no TTY and no GUI helper available".into())
}

/// Ensure loopback aliases exist (macOS needs `ifconfig lo0 alias`; Linux /8 is fine).
pub fn ensure_loopback_aliases(ips: &[String]) -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let missing: Vec<String> = ips
        .iter()
        .filter(|ip| !ip.is_empty() && *ip != "127.0.0.1")
        .filter(|ip| {
            !Command::new("ifconfig")
                .arg("lo0")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains(ip.as_str()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut shell = String::from("true");
    for ip in &missing {
        shell.push_str(&format!(" && ifconfig lo0 alias {ip} netmask 0xff000000"));
    }
    run_privileged_shell(&shell, PrivilegedPrompt::InteractiveOnce)
        .map_err(|e| format!("could not create loopback aliases on lo0: {e}").into())
}

/// Best-effort remove loopback aliases (macOS). **Never prompts** — cleanup only.
pub fn remove_loopback_aliases(ips: &[String]) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let mut shell = String::from("true");
    let mut any = false;
    for ip in ips {
        if ip.is_empty() || ip == "127.0.0.1" {
            continue;
        }
        any = true;
        shell.push_str(&format!(" ; ifconfig lo0 -alias {ip} 2>/dev/null"));
    }
    if any {
        let _ = run_privileged_shell(&shell, PrivilegedPrompt::Never);
    }
}

/// Port-forward argv: bind cluster service port on a specific loopback IP.
pub fn build_dns_port_forward_args(
    namespace: &str,
    service: &str,
    bind_ip: &str,
    service_port: u16,
) -> Vec<String> {
    vec![
        "port-forward".into(),
        "-n".into(),
        namespace.into(),
        format!("svc/{service}"),
        format!("{service_port}:{service_port}"),
        "--address".into(),
        bind_ip.into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::local::workbench::net::ServiceEndpoint;

    #[test]
    fn cluster_names_look_like_k8s_dns() {
        assert_eq!(
            cluster_dns_name("e2e-ui-api", "dogfood"),
            "e2e-ui-api.dogfood.svc.cluster.local"
        );
        assert_eq!(
            format_dns_url("e2e-ui-ui", "dogfood", 5180),
            "http://e2e-ui-ui.dogfood.svc.cluster.local:5180"
        );
    }

    #[test]
    fn next_ip_increments() {
        let a = next_hops_ip(&[]);
        assert_eq!(a, "127.53.0.2");
        let b = next_hops_ip(&["127.53.0.2", "127.53.0.3"]);
        assert_eq!(b, "127.53.0.4");
    }

    #[test]
    fn allocate_stable_across_calls() {
        let services = vec![
            ServiceEndpoint {
                namespace: "x".into(),
                name: "api".into(),
                port: 8791,
                protocol: "TCP".into(),
            },
            ServiceEndpoint {
                namespace: "x".into(),
                name: "ui".into(),
                port: 5180,
                protocol: "TCP".into(),
            },
        ];
        let mut existing = BTreeMap::new();
        existing.insert("x/api".into(), "127.53.0.5".into());
        let ips = allocate_service_ips("x", &services, &existing);
        assert_eq!(ips.get("api").map(String::as_str), Some("127.53.0.5"));
        assert!(ips.get("ui").unwrap().starts_with("127.53."));
        assert_ne!(ips.get("ui"), ips.get("api"));
    }

    #[test]
    fn merge_hosts_replaces_block() {
        let existing = "127.0.0.1 localhost\n# BEGIN hops-local-dns (managed by hops local — do not edit)\nold\n# END hops-local-dns\n";
        let lines =
            hosts_lines_for_workspace("x", &BTreeMap::from([("foo".into(), "127.53.0.2".into())]));
        let merged = merge_hosts_file(existing, &lines);
        assert!(merged.contains("127.0.0.1 localhost"));
        assert!(merged.contains("foo.x.svc.cluster.local"));
        assert!(merged.contains("foo.x.svc.cluster")); // mDNS-safe twin
        assert!(!merged.contains("\nold\n"));
        assert_eq!(merged.matches(HOSTS_BEGIN).count(), 1);
    }

    #[test]
    fn dns_port_forward_binds_address() {
        let args = build_dns_port_forward_args("x", "api", "127.53.0.2", 8791);
        assert!(args.contains(&"--address".into()));
        assert!(args.contains(&"127.53.0.2".into()));
        assert!(args.contains(&"8791:8791".into()));
        assert!(args.contains(&"svc/api".into()));
    }
}
