//! Gateway API browser ingress through Dory's local HTTPS router.
//!
//! Kubernetes remains authoritative: Environment HTTPRoutes declare public
//! hostnames and select one cluster Gateway. Hops gives that Gateway a stable
//! Kind NodePort at cluster creation and reconciles Dory custom domains to the
//! cluster's published host port. No app-specific forwarding process or host
//! file entry is required.

use super::net::RUNTIME_SUBDIR;
use super::registry::slugify_name;
use crate::commands::local::backend::kind::{self, INGRESS_NODE_PORT};
use crate::commands::local::kubectl_command;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const DORY_ADAPTER: &str = "dory";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayKey {
    pub namespace: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressRoute {
    pub hostname: String,
    pub gateway: GatewayKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayService {
    pub gateway: GatewayKey,
    pub service_name: String,
    pub service_port: u16,
    pub node_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IngressAccessPlan {
    pub namespace: String,
    pub routes: BTreeMap<String, GatewayKey>,
    pub urls: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngressAccessRuntime {
    pub namespace: String,
    #[serde(default)]
    pub adapter: String,
    /// Public hostname to Dory-published host port.
    #[serde(default)]
    pub aliases: BTreeMap<String, u16>,
    #[serde(default)]
    pub routes: BTreeMap<String, GatewayKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct DoryCustomDomain {
    hostname: String,
    port: u16,
}

fn runtime_path(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join(RUNTIME_SUBDIR)
        .join(format!("{}.ingress-access.json", slugify_name(workspace)))
}

pub fn load_ingress_access_runtime(
    state_dir: &Path,
    workspace: &str,
) -> Result<Option<IngressAccessRuntime>, Box<dyn Error>> {
    let path = runtime_path(state_dir, workspace);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

fn save_ingress_access_runtime(
    state_dir: &Path,
    workspace: &str,
    runtime: &IngressAccessRuntime,
) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(state_dir.join(RUNTIME_SUBDIR))?;
    fs::write(
        runtime_path(state_dir, workspace),
        serde_json::to_vec_pretty(runtime)?,
    )?;
    Ok(())
}

pub fn ingress_routes_from_value(namespace: &str, value: &Value) -> Vec<IngressRoute> {
    let mut routes = BTreeSet::new();
    for item in value["items"].as_array().cloned().unwrap_or_default() {
        let route_namespace = item["metadata"]["namespace"].as_str().unwrap_or(namespace);
        let gateways = item["spec"]["parentRefs"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|parent| {
                let group = parent["group"]
                    .as_str()
                    .unwrap_or("gateway.networking.k8s.io");
                let kind = parent["kind"].as_str().unwrap_or("Gateway");
                let name = parent["name"].as_str().unwrap_or("");
                if group != "gateway.networking.k8s.io" || kind != "Gateway" || name.is_empty() {
                    return None;
                }
                Some(GatewayKey {
                    namespace: parent["namespace"]
                        .as_str()
                        .unwrap_or(route_namespace)
                        .to_string(),
                    name: name.to_string(),
                })
            })
            .collect::<Vec<_>>();
        for hostname in item["spec"]["hostnames"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|hostname| hostname.as_str().map(str::to_string))
            .filter(|hostname| !hostname.trim().is_empty())
        {
            for gateway in &gateways {
                routes.insert((hostname.clone(), gateway.clone()));
            }
        }
    }
    routes
        .into_iter()
        .map(|(hostname, gateway)| IngressRoute { hostname, gateway })
        .collect()
}

pub fn gateway_service_from_value(
    gateway: &GatewayKey,
    value: &Value,
) -> Result<GatewayService, Box<dyn Error>> {
    let mut candidates = Vec::new();
    for item in value["items"].as_array().cloned().unwrap_or_default() {
        let name = item["metadata"]["name"].as_str().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let ports = item["spec"]["ports"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let selected = ports
            .iter()
            .find(|port| port["name"].as_str() == Some("http"))
            .or_else(|| ports.iter().find(|port| port["port"].as_u64() == Some(80)));
        let Some(port) = selected else {
            continue;
        };
        let Some(service_port) = port["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
        else {
            continue;
        };
        let Some(node_port) = port["nodePort"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
        else {
            return Err(format!(
                "Gateway {}/{} Service {name} HTTP port has no NodePort; local ingress requires nodePort {INGRESS_NODE_PORT}",
                gateway.namespace, gateway.name
            )
            .into());
        };
        candidates.push(GatewayService {
            gateway: gateway.clone(),
            service_name: name.to_string(),
            service_port,
            node_port,
        });
    }
    match candidates.as_slice() {
        [service] => Ok(service.clone()),
        [] => Err(format!(
            "Gateway {}/{} has no labeled Service with an HTTP port yet",
            gateway.namespace, gateway.name
        )
        .into()),
        _ => Err(format!(
            "Gateway {}/{} has multiple labeled HTTP Services; refusing to guess",
            gateway.namespace, gateway.name
        )
        .into()),
    }
}

pub fn discover_ingress_routes(namespace: &str) -> Result<Vec<IngressRoute>, Box<dyn Error>> {
    let output = kubectl_command(&["get", "httproute", "-n", namespace, "-o", "json"])
        .output()
        .map_err(|error| format!("kubectl get httproute failed: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("the server doesn't have a resource type")
            || stderr.contains("could not find the requested resource")
        {
            return Ok(Vec::new());
        }
        return Err(format!("kubectl get httproute -n {namespace} failed: {stderr}").into());
    }
    Ok(ingress_routes_from_value(
        namespace,
        &serde_json::from_slice(&output.stdout)?,
    ))
}

fn discover_gateway_service(gateway: &GatewayKey) -> Result<GatewayService, Box<dyn Error>> {
    let selector = format!("gateway.networking.k8s.io/gateway-name={}", gateway.name);
    let output = kubectl_command(&[
        "get",
        "svc",
        "-n",
        &gateway.namespace,
        "-l",
        &selector,
        "-o",
        "json",
    ])
    .output()?;
    if !output.status.success() {
        return Err(format!(
            "kubectl could not discover Service for Gateway {}/{}: {}",
            gateway.namespace,
            gateway.name,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    gateway_service_from_value(gateway, &serde_json::from_slice(&output.stdout)?)
}

fn dory_output(args: &[&str]) -> Result<std::process::Output, Box<dyn Error>> {
    Command::new("dory").args(args).output().map_err(|error| {
        format!(
            "Dory is required for local Gateway API ingress ({error}); install/start Dory and retry"
        )
        .into()
    })
}

fn dory_custom_domains() -> Result<BTreeMap<String, u16>, Box<dyn Error>> {
    let output = dory_output(&["network", "custom-domains"])?;
    if !output.status.success() {
        return Err(format!(
            "dory network custom-domains failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let domains: Vec<DoryCustomDomain> = serde_json::from_slice(&output.stdout)?;
    Ok(domains
        .into_iter()
        .map(|domain| (domain.hostname, domain.port))
        .collect())
}

fn set_dory_domain(hostname: &str, host_port: u16) -> Result<(), Box<dyn Error>> {
    let port = host_port.to_string();
    let output = dory_output(&[
        "network",
        "set-custom-domain",
        hostname,
        "--published-port",
        &port,
    ])?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "Dory could not register {hostname}: {}",
        String::from_utf8_lossy(&output.stderr)
    )
    .into())
}

fn remove_dory_domain_if_owned(hostname: &str, host_port: u16) {
    let Ok(domains) = dory_custom_domains() else {
        return;
    };
    if domains.get(hostname) != Some(&host_port) {
        return;
    }
    let _ = dory_output(&["network", "remove-custom-domain", hostname]);
}

pub fn plan_from_runtime(runtime: &IngressAccessRuntime) -> IngressAccessPlan {
    IngressAccessPlan {
        namespace: runtime.namespace.clone(),
        routes: runtime.routes.clone(),
        urls: runtime
            .aliases
            .keys()
            .map(|hostname| (hostname.clone(), format!("https://{hostname}")))
            .collect(),
    }
}

pub fn plan_from_routes(
    namespace: &str,
    routes: &[IngressRoute],
) -> Result<IngressAccessPlan, Box<dyn Error>> {
    let routes = route_map(routes)?;
    let urls = routes
        .keys()
        .map(|hostname| (hostname.clone(), format!("https://{hostname}")))
        .collect();
    Ok(IngressAccessPlan {
        namespace: namespace.to_string(),
        routes,
        urls,
    })
}

fn ensure_alias_ownership(
    state_dir: &Path,
    workspace: &str,
    hostnames: &BTreeSet<String>,
) -> Result<(), Box<dyn Error>> {
    let runtime_dir = state_dir.join(RUNTIME_SUBDIR);
    if !runtime_dir.is_dir() {
        return Ok(());
    }
    let own_path = runtime_path(state_dir, workspace);
    for entry in fs::read_dir(runtime_dir)?.flatten() {
        let path = entry.path();
        if path == own_path
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".ingress-access.json"))
        {
            continue;
        }
        let Some(runtime) = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<IngressAccessRuntime>(&bytes).ok())
        else {
            continue;
        };
        if let Some(hostname) = runtime
            .aliases
            .keys()
            .find(|hostname| hostnames.contains(*hostname))
        {
            return Err(format!(
                "hostname {hostname:?} is already owned by Environment runtime {}",
                path.display()
            )
            .into());
        }
    }
    Ok(())
}

fn validate_local_hostnames(hostnames: &BTreeSet<String>) -> Result<(), Box<dyn Error>> {
    if let Some(hostname) = hostnames
        .iter()
        .find(|hostname| !hostname.ends_with(".localhost"))
    {
        return Err(format!(
            "refusing to register non-local HTTPRoute hostname {hostname:?}; local browser ingress hostnames must end in .localhost"
        )
        .into());
    }
    Ok(())
}

fn validate_dory_domain_ownership(
    desired: &BTreeSet<String>,
    current: &BTreeMap<String, u16>,
    prior: Option<&IngressAccessRuntime>,
) -> Result<(), Box<dyn Error>> {
    for hostname in desired {
        let Some(current_port) = current.get(hostname) else {
            continue;
        };
        let owned = prior.is_some_and(|runtime| {
            runtime.adapter == DORY_ADAPTER && runtime.aliases.get(hostname) == Some(current_port)
        });
        if !owned {
            return Err(format!(
                "Dory custom domain {hostname:?} already exists on published port {current_port}; refusing to replace a route Hops does not own"
            )
            .into());
        }
    }
    Ok(())
}

pub fn ingress_access_needs_heal(runtime: &IngressAccessRuntime) -> bool {
    if runtime.adapter != DORY_ADAPTER || runtime.aliases.is_empty() || runtime.routes.is_empty() {
        return true;
    }
    let Ok(domains) = dory_custom_domains() else {
        return true;
    };
    runtime
        .aliases
        .iter()
        .any(|(hostname, port)| domains.get(hostname) != Some(port))
}

fn route_map(routes: &[IngressRoute]) -> Result<BTreeMap<String, GatewayKey>, Box<dyn Error>> {
    let mut result: BTreeMap<String, GatewayKey> = BTreeMap::new();
    for route in routes {
        if let Some(existing) = result.get(&route.hostname) {
            if existing != &route.gateway {
                return Err(format!(
                    "HTTPRoute hostname {:?} selects multiple Gateways ({}/{} and {}/{}); refusing to guess",
                    route.hostname,
                    existing.namespace,
                    existing.name,
                    route.gateway.namespace,
                    route.gateway.name
                )
                .into());
            }
        }
        result.insert(route.hostname.clone(), route.gateway.clone());
    }
    Ok(result)
}

pub fn ensure_ingress_access(
    namespace: &str,
    state_dir: &Path,
    workspace: &str,
) -> Result<(IngressAccessPlan, IngressAccessRuntime, bool), Box<dyn Error>> {
    let routes = discover_ingress_routes(namespace)?;
    let prior = load_ingress_access_runtime(state_dir, workspace)?;
    if routes.is_empty() {
        if prior.is_some() {
            stop_ingress_access(state_dir, workspace)?;
        }
        return Ok((
            IngressAccessPlan {
                namespace: namespace.to_string(),
                ..Default::default()
            },
            IngressAccessRuntime {
                namespace: namespace.to_string(),
                ..Default::default()
            },
            prior.is_some(),
        ));
    }

    let hostnames = routes
        .iter()
        .map(|route| route.hostname.clone())
        .collect::<BTreeSet<_>>();
    validate_local_hostnames(&hostnames)?;
    ensure_alias_ownership(state_dir, workspace, &hostnames)?;
    validate_dory_domain_ownership(&hostnames, &dory_custom_domains()?, prior.as_ref())?;
    let routes_by_hostname = route_map(&routes)?;
    let gateways = routes
        .iter()
        .map(|route| route.gateway.clone())
        .collect::<BTreeSet<_>>();
    if gateways.len() != 1 {
        return Err(format!(
            "local ingress expects one shared cluster Gateway, but Environment {namespace} selects {}",
            gateways.len()
        )
        .into());
    }

    let gateway = gateways.iter().next().expect("one Gateway");
    let service = discover_gateway_service(gateway)?;
    if service.node_port != INGRESS_NODE_PORT {
        return Err(format!(
            "Gateway {}/{} Service {} uses HTTP nodePort {}, but Hops local ingress reserves {INGRESS_NODE_PORT}; configure the Istio GatewayClass service defaults in cluster GitOps",
            gateway.namespace, gateway.name, service.service_name, service.node_port
        )
        .into());
    }
    let host_port = kind::ingress_host_port()?;
    let desired_aliases = hostnames
        .iter()
        .map(|hostname| (hostname.clone(), host_port))
        .collect::<BTreeMap<_, _>>();
    if let Some(runtime) = &prior {
        if runtime.adapter == DORY_ADAPTER
            && runtime.routes == routes_by_hostname
            && runtime.aliases == desired_aliases
            && !ingress_access_needs_heal(runtime)
        {
            return Ok((plan_from_runtime(runtime), runtime.clone(), false));
        }
    }

    stop_ingress_access(state_dir, workspace)?;
    let mut registered = Vec::new();
    for hostname in &hostnames {
        if let Err(error) = set_dory_domain(hostname, host_port) {
            for registered_hostname in registered {
                remove_dory_domain_if_owned(registered_hostname, host_port);
            }
            return Err(error);
        }
        registered.push(hostname);
    }
    let runtime = IngressAccessRuntime {
        namespace: namespace.to_string(),
        adapter: DORY_ADAPTER.to_string(),
        aliases: desired_aliases,
        routes: routes_by_hostname,
    };
    if let Err(error) = save_ingress_access_runtime(state_dir, workspace, &runtime) {
        for hostname in runtime.aliases.keys() {
            remove_dory_domain_if_owned(hostname, host_port);
        }
        return Err(error);
    }
    Ok((plan_from_runtime(&runtime), runtime, true))
}

pub fn stop_ingress_access(state_dir: &Path, workspace: &str) -> Result<(), Box<dyn Error>> {
    let Some(runtime) = load_ingress_access_runtime(state_dir, workspace)? else {
        return Ok(());
    };
    if runtime.adapter == DORY_ADAPTER {
        for (hostname, host_port) in &runtime.aliases {
            remove_dory_domain_if_owned(hostname, *host_port);
        }
    }
    let _ = fs::remove_file(runtime_path(state_dir, workspace));
    Ok(())
}

pub fn format_ingress_status(plan: &IngressAccessPlan, runtime: &IngressAccessRuntime) -> String {
    if plan.urls.is_empty() {
        return "ingress:  (no HTTPRoute hostnames)".into();
    }
    let up = !ingress_access_needs_heal(runtime);
    let mut lines = vec![format!(
        "ingress:  Gateway API via Dory [{}]",
        if up { "up" } else { "down" }
    )];
    lines.extend(plan.urls.values().map(|url| format!("  - {url}")));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_gateway_parent_defaults_and_cross_namespace_parent() {
        let value = json!({"items": [
            {"metadata": {"namespace": "dev"}, "spec": {
                "hostnames": ["app.dev.localhost"],
                "parentRefs": [{"name": "local", "namespace": "ingress"}]
            }},
            {"metadata": {"namespace": "dev"}, "spec": {
                "hostnames": ["internal.dev.localhost"],
                "parentRefs": [{"name": "same-namespace"}]
            }},
            {"metadata": {"namespace": "dev"}, "spec": {
                "hostnames": ["ignored.dev.localhost"],
                "parentRefs": [{"group": "example.test", "kind": "Other", "name": "no"}]
            }}
        ]});
        assert_eq!(
            ingress_routes_from_value("dev", &value),
            vec![
                IngressRoute {
                    hostname: "app.dev.localhost".into(),
                    gateway: GatewayKey {
                        namespace: "ingress".into(),
                        name: "local".into()
                    },
                },
                IngressRoute {
                    hostname: "internal.dev.localhost".into(),
                    gateway: GatewayKey {
                        namespace: "dev".into(),
                        name: "same-namespace".into()
                    },
                },
            ]
        );
    }

    #[test]
    fn plans_https_urls_from_declared_routes_without_mutation() {
        let routes = vec![IngressRoute {
            hostname: "app.feature.localhost".into(),
            gateway: GatewayKey {
                namespace: "ingress".into(),
                name: "local".into(),
            },
        }];
        let plan = plan_from_routes("feature", &routes).unwrap();
        assert_eq!(
            plan.urls.get("app.feature.localhost").map(String::as_str),
            Some("https://app.feature.localhost")
        );
        assert_eq!(plan.namespace, "feature");
    }

    #[test]
    fn selects_single_labeled_service_http_nodeport() {
        let gateway = GatewayKey {
            namespace: "ingress".into(),
            name: "local".into(),
        };
        let value = json!({"items": [{
            "metadata": {"name": "local-istio"},
            "spec": {"ports": [
                {"name": "status", "port": 15021, "nodePort": 30121},
                {"name": "http", "port": 80, "nodePort": 30080}
            ]}
        }]});
        assert_eq!(
            gateway_service_from_value(&gateway, &value).unwrap(),
            GatewayService {
                gateway,
                service_name: "local-istio".into(),
                service_port: 80,
                node_port: 30080,
            }
        );
    }

    #[test]
    fn rejects_http_service_without_nodeport() {
        let gateway = GatewayKey {
            namespace: "ingress".into(),
            name: "local".into(),
        };
        let value = json!({"items": [{
            "metadata": {"name": "local-istio"},
            "spec": {"ports": [{"name": "http", "port": 80}]}
        }]});
        assert!(gateway_service_from_value(&gateway, &value)
            .unwrap_err()
            .to_string()
            .contains("has no NodePort"));
    }

    #[test]
    fn rejects_ambiguous_gateway_services() {
        let gateway = GatewayKey {
            namespace: "ingress".into(),
            name: "local".into(),
        };
        let value = json!({"items": [
            {"metadata": {"name": "one"}, "spec": {"ports": [{"port": 80, "nodePort": 30080}]}},
            {"metadata": {"name": "two"}, "spec": {"ports": [{"name": "http", "port": 80, "nodePort": 30081}]}}
        ]});
        assert!(gateway_service_from_value(&gateway, &value)
            .unwrap_err()
            .to_string()
            .contains("multiple labeled HTTP Services"));
    }

    #[test]
    fn runtime_produces_clean_https_urls() {
        let gateway = GatewayKey {
            namespace: "ingress".into(),
            name: "local".into(),
        };
        let runtime = IngressAccessRuntime {
            namespace: "dev".into(),
            adapter: DORY_ADAPTER.into(),
            aliases: BTreeMap::from([("app.dev.localhost".into(), 30600)]),
            routes: BTreeMap::from([("app.dev.localhost".into(), gateway)]),
        };
        assert_eq!(
            plan_from_runtime(&runtime).urls["app.dev.localhost"],
            "https://app.dev.localhost"
        );
    }

    #[test]
    fn rejects_hostname_owned_by_another_environment() {
        let state_dir = std::env::temp_dir().join(format!("hops-ingress-{}", uuid::Uuid::new_v4()));
        let runtime = IngressAccessRuntime {
            namespace: "one".into(),
            adapter: DORY_ADAPTER.into(),
            aliases: BTreeMap::from([("app.localhost".into(), 30600)]),
            ..Default::default()
        };
        save_ingress_access_runtime(&state_dir, "one", &runtime).unwrap();

        let error =
            ensure_alias_ownership(&state_dir, "two", &BTreeSet::from(["app.localhost".into()]))
                .unwrap_err();
        assert!(error.to_string().contains("already owned"));
        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn rejects_non_local_http_route_hostname() {
        let error =
            validate_local_hostnames(&BTreeSet::from(["app.example.com".into()])).unwrap_err();
        assert!(error.to_string().contains("must end in .localhost"));
    }

    #[test]
    fn does_not_replace_unowned_dory_domain() {
        let hostnames = BTreeSet::from(["app.localhost".into()]);
        let domains = BTreeMap::from([("app.localhost".into(), 30600)]);
        assert!(validate_dory_domain_ownership(&hostnames, &domains, None)
            .unwrap_err()
            .to_string()
            .contains("does not own"));

        let prior = IngressAccessRuntime {
            namespace: "dev".into(),
            adapter: DORY_ADAPTER.into(),
            aliases: domains.clone(),
            ..Default::default()
        };
        validate_dory_domain_ownership(&hostnames, &domains, Some(&prior)).unwrap();
    }

    #[test]
    fn rejects_one_hostname_attached_to_multiple_gateways() {
        let routes = vec![
            IngressRoute {
                hostname: "app.localhost".into(),
                gateway: GatewayKey {
                    namespace: "ingress".into(),
                    name: "one".into(),
                },
            },
            IngressRoute {
                hostname: "app.localhost".into(),
                gateway: GatewayKey {
                    namespace: "ingress".into(),
                    name: "two".into(),
                },
            },
        ];
        assert!(route_map(&routes)
            .unwrap_err()
            .to_string()
            .contains("multiple Gateways"));
    }
}
