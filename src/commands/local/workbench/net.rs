//! Host access planning: kubefwd-style URLs preferred, map-mode unique ports fallback.

use super::registry::WorkspaceRecord;
use std::collections::BTreeMap;

/// Default starting port for map-mode allocation (avoids privileged + common dev ports).
pub const MAP_PORT_BASE_START: u16 = 18000;
/// Ports reserved per workspace in map mode (stride).
pub const MAP_PORT_STRIDE: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAccessMode {
    /// svc.<ns>.svc.cluster.local style (kubefwd).
    Kubefwd,
    /// Unique localhost ports via port-forward map.
    Map,
}

impl HostAccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            HostAccessMode::Kubefwd => "kubefwd",
            HostAccessMode::Map => "map",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "kubefwd" => Some(HostAccessMode::Kubefwd),
            "map" => Some(HostAccessMode::Map),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEndpoint {
    pub name: String,
    pub port: u16,
    pub protocol: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAccessPlan {
    pub mode: HostAccessMode,
    pub namespace: String,
    /// Service name → URL for host browser/curl.
    pub urls: BTreeMap<String, String>,
    /// Map-mode only: service → host port.
    pub port_map: BTreeMap<String, u16>,
    pub port_base: Option<u16>,
}

/// Format kubefwd-style URL for a service in a namespace.
pub fn format_kubefwd_url(service: &str, namespace: &str, port: u16) -> String {
    // kubefwd resolves <svc>.<ns> on loopback; cluster FQDN form is also accepted by some tools.
    format!("http://{service}.{namespace}.svc.cluster.local:{port}")
}

/// Format map-mode localhost URL.
pub fn format_map_url(host_port: u16) -> String {
    format!("http://127.0.0.1:{host_port}")
}

/// Allocate a non-overlapping port base for a new workspace given existing records.
pub fn allocate_port_base(existing: &[WorkspaceRecord]) -> u16 {
    let mut used: Vec<u16> = existing.iter().filter_map(|r| r.port_base).collect();
    used.sort_unstable();
    let mut candidate = MAP_PORT_BASE_START;
    for base in used {
        if candidate == base {
            candidate = base.saturating_add(MAP_PORT_STRIDE);
        } else if candidate < base {
            break;
        }
    }
    candidate
}

/// Plan host access for services in a workspace namespace.
///
/// `kubefwd_available`: whether kubefwd (or equivalent) is on PATH / preferred.
/// When false, map mode allocates unique ports from `port_base`.
pub fn plan_host_access(
    namespace: &str,
    services: &[ServiceEndpoint],
    kubefwd_available: bool,
    port_base: u16,
) -> HostAccessPlan {
    if kubefwd_available {
        let mut urls = BTreeMap::new();
        for svc in services {
            urls.insert(
                svc.name.clone(),
                format_kubefwd_url(&svc.name, namespace, svc.port),
            );
        }
        return HostAccessPlan {
            mode: HostAccessMode::Kubefwd,
            namespace: namespace.to_string(),
            urls,
            port_map: BTreeMap::new(),
            port_base: None,
        };
    }

    let mut urls = BTreeMap::new();
    let mut port_map = BTreeMap::new();
    for (i, svc) in services.iter().enumerate() {
        let host_port = port_base.saturating_add(i as u16);
        port_map.insert(svc.name.clone(), host_port);
        urls.insert(svc.name.clone(), format_map_url(host_port));
    }
    HostAccessPlan {
        mode: HostAccessMode::Map,
        namespace: namespace.to_string(),
        urls,
        port_map,
        port_base: Some(port_base),
    }
}

/// Render a short status card for humans (no kubectl literacy).
pub fn format_status_card(workspace: &str, plan: &HostAccessPlan) -> String {
    let mut lines = Vec::new();
    lines.push(format!("workspace: {workspace}"));
    lines.push(format!("namespace: {}", plan.namespace));
    lines.push(format!("access:   {}", plan.mode.as_str()));
    if plan.urls.is_empty() {
        lines.push("urls:     (no services discovered yet)".into());
    } else {
        lines.push("urls:".into());
        for (name, url) in &plan.urls {
            lines.push(format!("  - {name}: {url}"));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubefwd_urls_include_namespace() {
        let url = format_kubefwd_url("e2e-ui-ui", "hops-wt-alice", 5180);
        assert_eq!(
            url,
            "http://e2e-ui-ui.hops-wt-alice.svc.cluster.local:5180"
        );
        let url2 = format_kubefwd_url("e2e-ui-ui", "hops-wt-bob", 5180);
        assert_ne!(url, url2);
    }

    #[test]
    fn two_workspaces_get_distinct_map_ports() {
        let existing = vec![WorkspaceRecord {
            name: "alice".into(),
            namespace: "hops-wt-alice".into(),
            env_path: "/x".into(),
            project_root: None,
            host_access_mode: Some("map".into()),
            port_base: Some(18000),
            delivery_mode: None,
            updated_at: None,
        }];
        let bob_base = allocate_port_base(&existing);
        assert_ne!(bob_base, 18000);
        assert_eq!(bob_base, 18100);

        let services = vec![
            ServiceEndpoint {
                name: "ui".into(),
                port: 5180,
                protocol: "TCP".into(),
            },
            ServiceEndpoint {
                name: "api".into(),
                port: 8791,
                protocol: "TCP".into(),
            },
        ];
        let alice = plan_host_access("hops-wt-alice", &services, false, 18000);
        let bob = plan_host_access("hops-wt-bob", &services, false, bob_base);
        assert_eq!(alice.mode, HostAccessMode::Map);
        assert_eq!(bob.mode, HostAccessMode::Map);
        assert_ne!(alice.urls.get("ui"), bob.urls.get("ui"));
        assert_ne!(alice.port_map.get("ui"), bob.port_map.get("ui"));
        // No manual port planning: allocation is automatic and non-overlapping.
        assert!(alice.port_map.get("ui").unwrap() < bob.port_map.get("ui").unwrap());
    }

    #[test]
    fn kubefwd_mode_when_available() {
        let services = vec![ServiceEndpoint {
            name: "ui".into(),
            port: 5180,
            protocol: "TCP".into(),
        }];
        let plan = plan_host_access("hops-wt-x", &services, true, 18000);
        assert_eq!(plan.mode, HostAccessMode::Kubefwd);
        assert!(plan.urls["ui"].contains("hops-wt-x"));
        assert!(plan.port_map.is_empty());
    }

    #[test]
    fn status_card_lists_urls_without_kubectl() {
        let services = vec![ServiceEndpoint {
            name: "ui".into(),
            port: 5180,
            protocol: "TCP".into(),
        }];
        let plan = plan_host_access("hops-wt-alice", &services, true, 0);
        let card = format_status_card("alice", &plan);
        assert!(card.contains("workspace: alice"));
        assert!(card.contains("ui:"));
        assert!(!card.to_lowercase().contains("kubectl"));
    }
}
