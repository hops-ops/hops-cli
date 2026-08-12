//! Host access — one path.
//!
//! Services this workspace cares about:
//! - Services in the workspace namespace
//! - In-cluster `*.svc.cluster.local` endpoints referenced by those pods
//!   (e.g. OIDC issuer in `auth`)
//!
//! For each:
//! 1. allocate loopback IPs in `127.53.0.0/16`
//! 2. write `/etc/hosts` + lo0 aliases (one admin elevation)
//! 3. run a supervisor that keeps `kubectl port-forward` alive
//!
//! URLs are real k8s FQDNs, e.g.
//! `http://e2e-ui-ui.dogfood.svc.cluster.local:5180`

use super::cluster_dns::{
    self, format_dns_url, remove_loopback_aliases, sync_alloc_for_namespace, MACOS_LOCAL_DNS_PORT,
};
use crate::commands::local::{kubectl_command, HOPS_KUBE_CONTEXT_ENV};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub const RUNTIME_SUBDIR: &str = "runtime";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEndpoint {
    pub namespace: String,
    pub name: String,
    pub port: u16,
    pub protocol: String,
}

impl ServiceEndpoint {
    pub fn key(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostAccessPlan {
    /// Primary workspace namespace (status / cards).
    pub namespace: String,
    pub urls: BTreeMap<String, String>,
    /// service key `ns/name` → loopback IP
    pub ip_map: BTreeMap<String, String>,
    /// service key → port
    pub service_ports: BTreeMap<String, u16>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HostAccessRuntime {
    pub namespace: String,
    pub pids: Vec<u32>,
    #[serde(default)]
    pub log_path: Option<String>,
    /// service key `ns/name` → port (preferred). Also accepts bare name for older files.
    #[serde(default)]
    pub service_ports: BTreeMap<String, u16>,
    /// service key `ns/name` → loopback IP
    #[serde(default)]
    pub ip_map: BTreeMap<String, String>,
}

pub fn format_dns_service_url(service: &str, namespace: &str, port: u16) -> String {
    format_dns_url(service, namespace, port)
}

pub fn plan_host_access(namespace: &str, services: &[ServiceEndpoint]) -> HostAccessPlan {
    plan_host_access_with_ips(namespace, services, &BTreeMap::new())
}

pub fn plan_host_access_with_ips(
    namespace: &str,
    services: &[ServiceEndpoint],
    ip_map: &BTreeMap<String, String>,
) -> HostAccessPlan {
    let mut urls = BTreeMap::new();
    let mut service_ports = BTreeMap::new();
    for svc in services {
        let key = svc.key();
        urls.insert(
            key.clone(),
            format_dns_url(&svc.name, &svc.namespace, svc.port),
        );
        service_ports.insert(key, svc.port);
    }
    HostAccessPlan {
        namespace: namespace.to_string(),
        urls,
        ip_map: ip_map.clone(),
        service_ports,
    }
}

pub fn format_status_card(workspace: &str, plan: &HostAccessPlan) -> String {
    format_status_card_with_listen(workspace, plan, &BTreeMap::new())
}

pub fn format_status_card_with_listen(
    workspace: &str,
    plan: &HostAccessPlan,
    listen: &BTreeMap<String, bool>,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!("workspace: {workspace}"));
    lines.push(format!("namespace: {}", plan.namespace));
    lines.push("access:   cluster DNS (Service FQDNs)".into());
    if plan.urls.is_empty() {
        lines.push("urls:     (no services discovered yet)".into());
    } else {
        lines.push("urls:".into());
        for (key, url) in &plan.urls {
            let mark = match listen.get(key) {
                Some(true) => " [up]",
                Some(false) => " [down]",
                None => "",
            };
            lines.push(format!("  - {key}: {url}{mark}"));
        }
    }
    lines.join("\n")
}

pub fn host_access_status_line(rt: &HostAccessRuntime) -> String {
    let alive = rt.pids.iter().filter(|p| pid_is_alive(**p)).count();
    let listen = rt
        .ip_map
        .iter()
        .filter(|(key, ip)| {
            let port = rt.service_ports.get(*key).copied().unwrap_or(80);
            ip_port_listening(ip, port)
        })
        .count();
    format!(
        "access: dns supervisor {}/{} alive; {}/{} endpoints listening",
        alive,
        rt.pids.len().max(1),
        listen,
        rt.ip_map.len()
    )
}

fn runtime_path(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join(RUNTIME_SUBDIR)
        .join(format!("{workspace}.host-access.json"))
}

pub fn save_host_access_runtime(
    state_dir: &Path,
    workspace: &str,
    rt: &HostAccessRuntime,
) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(state_dir.join(RUNTIME_SUBDIR))?;
    fs::write(
        runtime_path(state_dir, workspace),
        serde_json::to_string_pretty(rt)?,
    )?;
    Ok(())
}

pub fn load_host_access_runtime(
    state_dir: &Path,
    workspace: &str,
) -> Result<Option<HostAccessRuntime>, Box<dyn Error>> {
    let path = runtime_path(state_dir, workspace);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&fs::read_to_string(path)?)?))
}

pub fn clear_host_access_runtime(state_dir: &Path, workspace: &str) {
    let _ = fs::remove_file(runtime_path(state_dir, workspace));
}

/// Services in `namespace` (first TCP port each).
pub fn discover_services(namespace: &str) -> Result<Vec<ServiceEndpoint>, Box<dyn Error>> {
    discover_services_in_namespace(namespace)
}

fn discover_services_in_namespace(namespace: &str) -> Result<Vec<ServiceEndpoint>, Box<dyn Error>> {
    let output = kubectl_command(&["get", "svc", "-n", namespace, "-o", "json"])
        .output()
        .map_err(|e| format!("kubectl get svc failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "kubectl get svc -n {namespace} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let v: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let mut out = Vec::new();
    for item in v["items"].as_array().cloned().unwrap_or_default() {
        let name = item["metadata"]["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() || name == "kubernetes" {
            continue;
        }
        for p in item["spec"]["ports"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let port = p["port"].as_u64().unwrap_or(0) as u16;
            let protocol = p["protocol"].as_str().unwrap_or("TCP");
            if port == 0 || (protocol != "TCP" && protocol != "tcp") {
                continue;
            }
            // Skip postgres-ish ports for host browser access.
            if port == 5432 {
                continue;
            }
            out.push(ServiceEndpoint {
                namespace: namespace.to_string(),
                name: name.clone(),
                port,
                protocol: "TCP".into(),
            });
            break;
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Workspace Services **plus** in-cluster FQDNs referenced by pod env
/// (OIDC issuer, etc.).
pub fn discover_workspace_endpoints(
    namespace: &str,
) -> Result<Vec<ServiceEndpoint>, Box<dyn Error>> {
    let mut by_key: BTreeMap<String, ServiceEndpoint> = BTreeMap::new();
    for svc in discover_services_in_namespace(namespace)? {
        by_key.insert(svc.key(), svc);
    }

    for (ref_ns, ref_name, port_hint) in scan_pod_cluster_dns_refs(namespace)? {
        if ref_ns == namespace && by_key.contains_key(&format!("{ref_ns}/{ref_name}")) {
            continue;
        }
        let key = format!("{ref_ns}/{ref_name}");
        if by_key.contains_key(&key) {
            continue;
        }
        // Resolve live service port when possible.
        let port = match service_port(&ref_ns, &ref_name) {
            Ok(p) => p,
            Err(_) => port_hint.unwrap_or(80),
        };
        if port == 5432 {
            continue;
        }
        by_key.insert(
            key,
            ServiceEndpoint {
                namespace: ref_ns,
                name: ref_name,
                port,
                protocol: "TCP".into(),
            },
        );
    }

    Ok(by_key.into_values().collect())
}

fn service_port(namespace: &str, name: &str) -> Result<u16, Box<dyn Error>> {
    let output = kubectl_command(&[
        "get",
        "svc",
        name,
        "-n",
        namespace,
        "-o",
        "jsonpath={.spec.ports[0].port}",
    ])
    .output()
    .map_err(|e| format!("kubectl get svc {name}: {e}"))?;
    if !output.status.success() {
        return Err("service not found".into());
    }
    let s = String::from_utf8_lossy(&output.stdout);
    s.trim()
        .parse()
        .map_err(|_| format!("bad port for {namespace}/{name}").into())
}

/// Parse pod env for `http(s)://svc.ns.svc.cluster.local:port` references.
fn scan_pod_cluster_dns_refs(
    namespace: &str,
) -> Result<Vec<(String, String, Option<u16>)>, Box<dyn Error>> {
    let output = kubectl_command(&["get", "pods", "-n", namespace, "-o", "json"])
        .output()
        .map_err(|e| format!("kubectl get pods failed: {e}"))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let v: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let mut found: BTreeSet<(String, String, Option<u16>)> = BTreeSet::new();

    for item in v["items"].as_array().cloned().unwrap_or_default() {
        let containers = item["spec"]["containers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for c in containers {
            for env in c["env"].as_array().cloned().unwrap_or_default() {
                if let Some(val) = env["value"].as_str() {
                    for (ns, name, port) in regex_lite_cluster_dns(val) {
                        found.insert((ns, name, port));
                    }
                }
            }
        }
    }
    Ok(found.into_iter().collect())
}

/// Minimal parser: find `name.namespace.svc.cluster.local` and optional `:port`.
fn regex_lite_cluster_dns(s: &str) -> Vec<(String, String, Option<u16>)> {
    let mut out = Vec::new();
    let marker = ".svc.cluster.local";
    let mut search_from = 0;
    while let Some(rel) = s[search_from..].find(marker) {
        let end = search_from + rel;
        // walk back for hostname labels
        let head = &s[..end];
        let start = head
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))
            .map(|i| i + 1)
            .unwrap_or(0);
        let host = &s[start..end];
        // expect name.namespace
        let mut parts: Vec<&str> = host.split('.').collect();
        if parts.len() >= 2 {
            let ns = parts.pop().unwrap().to_string();
            let name = parts.join(".");
            if !name.is_empty() && !ns.is_empty() {
                let after = end + marker.len();
                let port = if s[after..].starts_with(':') {
                    let digits: String = s[after + 1..]
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    digits.parse().ok()
                } else {
                    None
                };
                out.push((ns, name, port));
            }
        }
        search_from = end + marker.len();
    }
    out
}

/// Start host access for workspace endpoints (FQDNs + supervisor).
pub fn start_host_access(
    namespace: &str,
    services: &[ServiceEndpoint],
    state_dir: &Path,
    workspace: &str,
) -> Result<(HostAccessPlan, HostAccessRuntime), Box<dyn Error>> {
    if services.is_empty() {
        return Ok((
            plan_host_access(namespace, services),
            HostAccessRuntime {
                namespace: namespace.into(),
                ..Default::default()
            },
        ));
    }

    // Allocate IPs per namespace group.
    let mut by_ns: BTreeMap<String, Vec<ServiceEndpoint>> = BTreeMap::new();
    for svc in services {
        by_ns
            .entry(svc.namespace.clone())
            .or_default()
            .push(svc.clone());
    }

    let mut ip_map: BTreeMap<String, String> = BTreeMap::new();
    let mut ns_blocks: Vec<(String, BTreeMap<String, String>)> = Vec::new();
    for (ns, svcs) in &by_ns {
        // sync_alloc maps bare service name → ip; rekey to ns/name
        let bare = sync_alloc_for_namespace(state_dir, ns, svcs)?;
        let mut bare_for_hosts = BTreeMap::new();
        for (name, ip) in bare {
            bare_for_hosts.insert(name.clone(), ip.clone());
            ip_map.insert(format!("{ns}/{name}"), ip);
        }
        ns_blocks.push((ns.clone(), bare_for_hosts));
    }

    let ips: Vec<String> = ip_map.values().cloned().collect();

    let mut blocks = collect_dns_blocks_from_runtimes(state_dir, Some(workspace))?;
    blocks.extend(ns_blocks);
    let mut merged_by_ns: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (ns, m) in blocks {
        merged_by_ns.entry(ns).or_default().extend(m);
    }
    let mut host_lines = Vec::new();
    for (ns, m) in &merged_by_ns {
        host_lines.extend(cluster_dns::hosts_lines_for_workspace(ns, m));
    }
    let current = fs::read_to_string("/etc/hosts").unwrap_or_default();
    let hosts_body = cluster_dns::merge_hosts_file(&current, &host_lines);

    cluster_dns::apply_privileged_dns_config(&hosts_body, &ips)?;
    verify_loopback_aliases_ready(&ips)?;

    let plan = plan_host_access_with_ips(namespace, services, &ip_map);
    // Zone file for macOS stub DNS (instant *.svc.cluster.local; avoid mDNS).
    write_zone_file(state_dir, &merged_by_ns)?;
    ensure_macos_stub_dns(state_dir)?;
    let rt = start_dns_supervisor(&plan, services, state_dir, workspace)?;
    std::thread::sleep(Duration::from_millis(700));
    if !rt.pids.iter().any(|p| pid_is_alive(*p)) {
        let tail = rt
            .log_path
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok())
            .unwrap_or_default();
        let tail: String = tail
            .lines()
            .rev()
            .take(30)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("dns supervisor exited immediately\n{tail}").into());
    }
    log::info!(
        "host access: cluster DNS for {} endpoint(s)",
        services.len()
    );
    Ok((plan, rt))
}

/// Write zone: fqdn → ip for the macOS stub DNS (and reloads on every start).
fn write_zone_file(
    state_dir: &Path,
    by_ns: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<(), Box<dyn Error>> {
    let path = state_dir.join(RUNTIME_SUBDIR).join("dns-zone.tsv");
    fs::create_dir_all(path.parent().unwrap())?;
    let mut body = String::new();
    for (ns, m) in by_ns {
        for (svc, ip) in m {
            let fqdn = format!("{svc}.{ns}.svc.cluster.local");
            let twin = format!("{svc}.{ns}.svc.cluster");
            let short = format!("{svc}.{ns}");
            body.push_str(&format!("{fqdn}\t{ip}\n{twin}\t{ip}\n{short}\t{ip}\n"));
        }
    }
    fs::write(path, body)?;
    Ok(())
}

/// macOS: run a tiny UDP DNS on 127.0.0.1:53535 answering A records from dns-zone.tsv.
/// Paired with `/etc/resolver/svc.cluster.local` so getaddrinfo skips mDNS.
fn ensure_macos_stub_dns(state_dir: &Path) -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let log_dir = state_dir.join(RUNTIME_SUBDIR);
    fs::create_dir_all(&log_dir)?;
    let zone = log_dir.join("dns-zone.tsv");
    let script = log_dir.join("macos-stub-dns.py");
    let pid_path = log_dir.join("macos-stub-dns.pid");
    let log_path = log_dir.join("macos-stub-dns.log");

    // Already healthy?
    if let Ok(raw) = fs::read_to_string(&pid_path) {
        if let Ok(pid) = raw.trim().parse::<u32>() {
            if pid_is_alive(pid) && stub_dns_responds() {
                return Ok(());
            }
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }

    let py = format!(
        r#"#!/usr/bin/env python3
import socket, struct, sys, time, os
ZONE = '{zone}'
PORT = {port}
LOG = '{log}'

def load_zone():
    m = {{}}
    try:
        with open(ZONE) as f:
            for line in f:
                line = line.strip()
                if not line or line.startswith('#'):
                    continue
                parts = line.split()
                if len(parts) >= 2:
                    m[parts[0].lower().rstrip('.')] = parts[1]
    except FileNotFoundError:
        pass
    return m

def parse_qname(data, off):
    labels = []
    while True:
        if off >= len(data):
            raise ValueError('bad qname')
        l = data[off]
        if l == 0:
            return '.'.join(labels).lower(), off + 1
        if (l & 0xC0) == 0xC0:
            # pointer — not expected in questions we generate answers for
            ptr = struct.unpack('!H', data[off:off+2])[0] & 0x3FFF
            name, _ = parse_qname(data, ptr)
            return name, off + 2
        off += 1
        labels.append(data[off:off+l].decode('ascii', 'ignore'))
        off += l

def encode_name(name):
    out = b''
    for lab in name.split('.'):
        if not lab:
            continue
        b = lab.encode('ascii')
        out += bytes([len(b)]) + b
    return out + b'\x00'

def build_response(req, zone):
    if len(req) < 12:
        return None
    tid = req[:2]
    flags_qr = b'\x81\x80'  # standard response, recursion available
    # parse question
    try:
        qname, qend = parse_qname(req, 12)
    except Exception:
        return None
    if qend + 4 > len(req):
        return None
    qtype, qclass = struct.unpack('!HH', req[qend:qend+4])
    question = req[12:qend+4]
    ip = zone.get(qname)
    if qtype != 1 or qclass != 1 or not ip:  # A IN
        # NXDOMAIN-ish empty answer so clients fall through quickly
        return tid + flags_qr + struct.pack('!HHHH', 1, 0, 0, 0) + question
    try:
        addr = socket.inet_aton(ip)
    except OSError:
        return tid + flags_qr + struct.pack('!HHHH', 1, 0, 0, 0) + question
    answer = encode_name(qname) + struct.pack('!HHIH', 1, 1, 30, 4) + addr
    return tid + flags_qr + struct.pack('!HHHH', 1, 1, 0, 0) + question + answer

def main():
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(('127.0.0.1', PORT))
    with open(LOG, 'a') as lf:
        lf.write('start port=%s zone=%s\\n' % (PORT, ZONE))
        lf.flush()
        zone = load_zone()
        last = time.time()
        while True:
            sock.settimeout(2.0)
            try:
                data, addr = sock.recvfrom(512)
            except socket.timeout:
                if time.time() - last > 2:
                    zone = load_zone()
                    last = time.time()
                continue
            if time.time() - last > 2:
                zone = load_zone()
                last = time.time()
            resp = build_response(data, zone)
            if resp:
                sock.sendto(resp, addr)

if __name__ == '__main__':
    main()
"#,
        zone = zone.display().to_string(),
        port = MACOS_LOCAL_DNS_PORT,
        log = log_path.display().to_string(),
    );
    fs::write(&script, py)?;
    import_unix_chmod(&script);

    let log_out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log_out.try_clone()?;
    let child = Command::new("python3")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|e| format!("failed to spawn macOS stub DNS: {e}"))?;
    let pid = child.id();
    std::mem::forget(child);
    fs::write(&pid_path, format!("{pid}\n"))?;
    std::thread::sleep(Duration::from_millis(200));
    if !pid_is_alive(pid) {
        return Err("macOS stub DNS exited immediately".into());
    }
    log::info!(
        "macOS stub DNS for *.svc.cluster.local on 127.0.0.1:{MACOS_LOCAL_DNS_PORT} pid={pid}"
    );
    Ok(())
}

fn stub_dns_responds() -> bool {
    // best-effort: UDP not easy to probe without a real query; process alive is enough
    true
}

fn verify_loopback_aliases_ready(ips: &[String]) -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let lo0 = Command::new("ifconfig")
        .arg("lo0")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let missing: Vec<&str> = ips
        .iter()
        .filter(|ip| !ip.is_empty() && *ip != "127.0.0.1" && !lo0.contains(ip.as_str()))
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "lo0 missing aliases ({}); approve the single admin prompt so aliases apply",
        missing.join(", ")
    )
    .into())
}

fn start_dns_supervisor(
    plan: &HostAccessPlan,
    services: &[ServiceEndpoint],
    state_dir: &Path,
    workspace: &str,
) -> Result<HostAccessRuntime, Box<dyn Error>> {
    let _ = stop_host_access_processes_only(state_dir, workspace);

    let log_dir = state_dir.join(RUNTIME_SUBDIR);
    fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join(format!("{workspace}.host-access.log"));
    let config_path = log_dir.join(format!("{workspace}.dns-forwards.tsv"));
    let script_path = log_dir.join(format!("{workspace}.dns-sup.sh"));
    let piddir = log_dir.join(format!("{workspace}.dns-pf-pids"));

    // TSV: NS \t SVC \t IP \t PORT \t KEY
    let mut tsv = String::new();
    for svc in services {
        let key = svc.key();
        let ip = plan
            .ip_map
            .get(&key)
            .cloned()
            .unwrap_or_else(|| "127.0.0.1".into());
        tsv.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            svc.namespace, svc.name, ip, svc.port, key
        ));
    }
    fs::write(&config_path, &tsv)?;

    let q = |s: &str| s.replace('\'', r"'\''");
    let script = format!(
        r#"#!/usr/bin/env bash
set -u
CONFIG='{config}'
LOG='{log}'
PIDDIR='{piddir}'
export KUBECONFIG="${{KUBECONFIG:-}}"
KCTX="${{HOPS_KUBE_CONTEXT:-}}"
k() {{
  if [ -n "$KCTX" ]; then
    command kubectl --context "$KCTX" "$@"
  else
    command kubectl "$@"
  fi
}}
mkdir -p "$PIDDIR"
echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) supervisor start" >>"$LOG"
cleanup() {{
  for f in "$PIDDIR"/*.pid; do
    [ -f "$f" ] || continue
    kill "$(cat "$f")" 2>/dev/null || true
  done
  exit 0
}}
trap cleanup TERM INT HUP
while true; do
  while IFS=$'\t' read -r NS SVC IP PORT KEY; do
    [ -z "${{NS:-}}" ] && continue
    safe=$(echo "$KEY" | tr '/:' '__')
    pf="$PIDDIR/$safe.pid"
    pid=""
    if [ -f "$pf" ]; then pid=$(cat "$pf" 2>/dev/null || true); fi
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then continue; fi
    if [ -n "$pid" ]; then
      kill "$pid" 2>/dev/null || true
      echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) restart $KEY" >>"$LOG"
    else
      echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) start $KEY $IP:$PORT" >>"$LOG"
    fi
    k port-forward -n "$NS" --address "$IP" "svc/$SVC" "${{PORT}}:${{PORT}}" >>"$LOG" 2>&1 &
    echo $! >"$pf"
  done < "$CONFIG"
  sleep 2
done
"#,
        config = q(&config_path.to_string_lossy()),
        log = q(&log_path.to_string_lossy()),
        piddir = q(&piddir.to_string_lossy()),
    );
    fs::write(&script_path, script)?;
    import_unix_chmod(&script_path);

    let log_out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log_out.try_clone()?;
    let mut cmd = Command::new("bash");
    cmd.arg(&script_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err));
    if let Ok(kc) = std::env::var("KUBECONFIG") {
        cmd.env("KUBECONFIG", kc);
    }
    if let Ok(ctx) = std::env::var(HOPS_KUBE_CONTEXT_ENV) {
        cmd.env(HOPS_KUBE_CONTEXT_ENV, ctx);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn dns supervisor: {e}"))?;
    let pid = child.id();
    std::mem::forget(child);

    let service_ports: BTreeMap<String, u16> = services.iter().map(|s| (s.key(), s.port)).collect();
    let runtime = HostAccessRuntime {
        namespace: plan.namespace.clone(),
        pids: vec![pid],
        log_path: Some(log_path.display().to_string()),
        service_ports,
        ip_map: plan.ip_map.clone(),
    };
    save_host_access_runtime(state_dir, workspace, &runtime)?;
    log::info!("host access started (dns supervisor) pid={pid}");
    Ok(runtime)
}

#[cfg(unix)]
fn import_unix_chmod(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
}
#[cfg(not(unix))]
fn import_unix_chmod(_path: &Path) {}

/// Build `kubectl port-forward` argv (package registry host publish).
pub fn build_port_forward_args(
    namespace: &str,
    service: &str,
    host_port: u16,
    service_port: u16,
) -> Vec<String> {
    vec![
        "port-forward".into(),
        "-n".into(),
        namespace.into(),
        format!("svc/{service}"),
        format!("{host_port}:{service_port}"),
    ]
}

pub fn localhost_port_listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(200),
    )
    .is_ok()
}

pub fn ip_port_listening(ip: &str, port: u16) -> bool {
    let Ok(addr) = format!("{ip}:{port}").parse::<SocketAddr>() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

pub fn host_access_needs_heal(rt: &HostAccessRuntime) -> bool {
    if rt.ip_map.is_empty() {
        return true;
    }
    if !rt.pids.iter().any(|p| pid_is_alive(*p)) {
        return true;
    }
    for (key, ip) in &rt.ip_map {
        let port = rt.service_ports.get(key).copied().unwrap_or(80);
        if !ip_port_listening(ip, port) {
            return true;
        }
    }
    false
}

pub fn ensure_host_access(
    namespace: &str,
    services: &[ServiceEndpoint],
    state_dir: &Path,
    workspace: &str,
) -> Result<(HostAccessPlan, HostAccessRuntime, bool), Box<dyn Error>> {
    let prior = load_host_access_runtime(state_dir, workspace)?;
    if let Some(rt) = &prior {
        if !host_access_needs_heal(rt) {
            return Ok((plan_from_runtime(rt), rt.clone(), false));
        }
        log::info!("host access unhealthy; restarting dns supervisor");
    }
    let services = if services.is_empty() {
        prior
            .as_ref()
            .map(services_from_runtime)
            .unwrap_or_else(|| services.to_vec())
    } else {
        services.to_vec()
    };
    let (plan, rt) = start_host_access(namespace, &services, state_dir, workspace)?;
    std::thread::sleep(Duration::from_millis(400));
    Ok((plan, rt, true))
}

fn plan_from_runtime(rt: &HostAccessRuntime) -> HostAccessPlan {
    let mut urls = BTreeMap::new();
    for (key, port) in &rt.service_ports {
        let (ns, name) = split_service_key(key, &rt.namespace);
        urls.insert(key.clone(), format_dns_url(&name, &ns, *port));
    }
    if urls.is_empty() {
        for key in rt.ip_map.keys() {
            let (ns, name) = split_service_key(key, &rt.namespace);
            urls.insert(key.clone(), format_dns_url(&name, &ns, 80));
        }
    }
    HostAccessPlan {
        namespace: rt.namespace.clone(),
        urls,
        ip_map: rt.ip_map.clone(),
        service_ports: rt.service_ports.clone(),
    }
}

fn split_service_key(key: &str, default_ns: &str) -> (String, String) {
    if let Some((ns, name)) = key.split_once('/') {
        (ns.to_string(), name.to_string())
    } else {
        (default_ns.to_string(), key.to_string())
    }
}

fn services_from_runtime(rt: &HostAccessRuntime) -> Vec<ServiceEndpoint> {
    let keys: Vec<String> = if !rt.service_ports.is_empty() {
        rt.service_ports.keys().cloned().collect()
    } else {
        rt.ip_map.keys().cloned().collect()
    };
    keys.into_iter()
        .map(|key| {
            let (ns, name) = split_service_key(&key, &rt.namespace);
            ServiceEndpoint {
                namespace: ns,
                name,
                port: rt.service_ports.get(&key).copied().unwrap_or(80),
                protocol: "TCP".into(),
            }
        })
        .collect()
}

pub fn url_listen_status(plan: &HostAccessPlan) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    for (key, ip) in &plan.ip_map {
        let port = plan.service_ports.get(key).copied().unwrap_or(80);
        out.insert(key.clone(), ip_port_listening(ip, port));
    }
    out
}

fn stop_host_access_processes_only(
    state_dir: &Path,
    workspace: &str,
) -> Result<(), Box<dyn Error>> {
    if let Some(rt) = load_host_access_runtime(state_dir, workspace)? {
        for pid in &rt.pids {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
        std::thread::sleep(Duration::from_millis(200));
        for pid in &rt.pids {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        let piddir = state_dir
            .join(RUNTIME_SUBDIR)
            .join(format!("{workspace}.dns-pf-pids"));
        if let Ok(entries) = fs::read_dir(piddir) {
            for ent in entries.flatten() {
                if let Ok(s) = fs::read_to_string(ent.path()) {
                    if let Ok(pid) = s.trim().parse::<u32>() {
                        let _ = Command::new("kill")
                            .args(["-TERM", &pid.to_string()])
                            .status();
                    }
                }
            }
        }
    }
    clear_host_access_runtime(state_dir, workspace);
    Ok(())
}

pub fn stop_host_access(state_dir: &Path, workspace: &str) -> Result<(), Box<dyn Error>> {
    let rt = load_host_access_runtime(state_dir, workspace)?;
    stop_host_access_processes_only(state_dir, workspace)?;
    if let Some(rt) = rt {
        if !rt.ip_map.is_empty() {
            remove_loopback_aliases(&rt.ip_map.values().cloned().collect::<Vec<_>>());
            if let Ok(blocks) = collect_dns_blocks_from_runtimes(state_dir, Some(workspace)) {
                let _ = rebuild_hosts_from_blocks_noprompt(&blocks);
            }
        }
    }
    Ok(())
}

fn rebuild_hosts_from_blocks_noprompt(
    blocks: &[(String, BTreeMap<String, String>)],
) -> Result<(), Box<dyn Error>> {
    use super::cluster_dns::{
        dns_os_config_present, hosts_lines_for_workspace, merge_hosts_file, run_privileged_shell,
        PrivilegedPrompt,
    };
    let mut lines = Vec::new();
    for (ns, ips) in blocks {
        lines.extend(hosts_lines_for_workspace(ns, ips));
    }
    let current = fs::read_to_string("/etc/hosts").unwrap_or_default();
    let merged = merge_hosts_file(&current, &lines);
    if dns_os_config_present(&merged, &[]) {
        return Ok(());
    }
    let tmp = std::env::temp_dir().join(format!("hops-hosts-down-{}.tmp", std::process::id()));
    fs::write(&tmp, &merged)?;
    let script = format!("cp '{}' /etc/hosts && chmod 644 /etc/hosts", tmp.display());
    let res = run_privileged_shell(&script, PrivilegedPrompt::Never);
    let _ = fs::remove_file(&tmp);
    res
}

fn collect_dns_blocks_from_runtimes(
    state_dir: &Path,
    skip_workspace: Option<&str>,
) -> Result<Vec<(String, BTreeMap<String, String>)>, Box<dyn Error>> {
    let dir = state_dir.join(RUNTIME_SUBDIR);
    let mut by_ns: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    for ent in entries.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".host-access.json") {
            continue;
        }
        let ws = name.trim_end_matches(".host-access.json");
        if skip_workspace == Some(ws) {
            continue;
        }
        let Ok(text) = fs::read_to_string(ent.path()) else {
            continue;
        };
        let Ok(rt) = serde_json::from_str::<HostAccessRuntime>(&text) else {
            continue;
        };
        for (key, ip) in rt.ip_map {
            let (ns, svc) = split_service_key(&key, &rt.namespace);
            by_ns.entry(ns).or_default().insert(svc, ip);
        }
    }
    Ok(by_ns.into_iter().collect())
}

pub fn pid_is_alive(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "state="])
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let state = String::from_utf8_lossy(&o.stdout);
            let state = state.trim();
            !state.is_empty() && !state.starts_with('Z') && !state.starts_with('z')
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_urls_are_cluster_fqdns() {
        let svcs = vec![ServiceEndpoint {
            namespace: "dogfood".into(),
            name: "e2e-ui-ui".into(),
            port: 5180,
            protocol: "TCP".into(),
        }];
        let plan = plan_host_access("dogfood", &svcs);
        assert_eq!(
            plan.urls.get("dogfood/e2e-ui-ui").map(String::as_str),
            Some("http://e2e-ui-ui.dogfood.svc.cluster.local:5180")
        );
    }

    #[test]
    fn parse_cluster_dns_from_env_value() {
        let refs = regex_lite_cluster_dns(
            "http://zitadel-zitadel.auth.svc.cluster.local:8080/oauth/v2/keys",
        );
        assert_eq!(
            refs,
            vec![("auth".into(), "zitadel-zitadel".into(), Some(8080))]
        );
    }
}
