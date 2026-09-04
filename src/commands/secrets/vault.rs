//! Value-safe HashiCorp Vault KV transport for `hops secrets sync vault`.

use super::VaultSecretsRuntimeConfig;
use serde_json::{json, Map, Value as JsonValue};
use std::error::Error;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct VaultSession {
    address: String,
    token: String,
    mount: String,
    version: String,
    _port_forward: Option<PortForwardGuard>,
}

struct PortForwardGuard {
    child: Child,
}

impl Drop for PortForwardGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) fn validate_settings(
    settings: &VaultSecretsRuntimeConfig,
) -> Result<(), Box<dyn Error>> {
    normalize_vault_address(&settings.address)?;
    validate_vault_path(&settings.mount, "secrets.vault.mount", false)?;
    validate_vault_path(&settings.path_prefix, "secrets.vault.path_prefix", true)?;
    validate_env_name(&settings.token_env)?;
    if settings.kube_local_port == 0 {
        return Err("secrets.vault.kube.local_port must be greater than zero".into());
    }
    for (label, value) in [
        (
            "secrets.vault.kube.namespace",
            settings.kube_namespace.as_str(),
        ),
        ("secrets.vault.kube.service", settings.kube_service.as_str()),
    ] {
        if value.is_empty()
            || !value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.')
        {
            return Err(format!("{label} contains unsupported characters").into());
        }
    }
    Ok(())
}

pub(crate) fn normalize_vault_address(value: &str) -> Result<String, Box<dyn Error>> {
    let normalized = value.trim().trim_end_matches('/').to_string();
    if !normalized.starts_with("http://") && !normalized.starts_with("https://") {
        return Err("secrets.vault.address must use http:// or https://".into());
    }
    let remainder = normalized
        .split_once("://")
        .map(|(_, remainder)| remainder)
        .unwrap_or_default();
    if remainder.contains('/') || remainder.contains('?') || remainder.contains('#') {
        return Err("secrets.vault.address must not contain a path, query, or fragment".into());
    }
    if parse_host_port(&normalized).is_none() {
        return Err("secrets.vault.address must contain a valid host and port".into());
    }
    Ok(normalized)
}

pub(crate) fn validate_vault_path(
    value: &str,
    label: &str,
    allow_empty: bool,
) -> Result<String, Box<dyn Error>> {
    let normalized = value.trim().trim_matches('/');
    if normalized.is_empty() {
        if allow_empty {
            return Ok(String::new());
        }
        return Err(format!("{label} cannot be empty").into());
    }
    if normalized.len() > 512 {
        return Err(format!("{label} is too long").into());
    }
    for component in normalized.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || !component
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
        {
            return Err(format!("{label} contains an unsupported path component").into());
        }
    }
    Ok(normalized.to_string())
}

fn validate_env_name(value: &str) -> Result<(), Box<dyn Error>> {
    let mut chars = value.chars();
    let valid_start = chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');
    if !valid_start || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err("secrets.vault.token_env must be a valid environment variable name".into());
    }
    Ok(())
}

pub(crate) fn open_session(
    settings: &VaultSecretsRuntimeConfig,
    address_override: Option<&str>,
    port_forward: Option<bool>,
) -> Result<VaultSession, Box<dyn Error>> {
    let mut effective = settings.clone();
    if let Some(address) = address_override {
        effective.address = address.trim().trim_end_matches('/').to_string();
    }
    validate_settings(&effective)?;

    let token = std::env::var(&effective.token_env).map_err(|_| {
        format!(
            "Vault token not found; export the environment variable named by secrets.vault.token_env ({})",
            effective.token_env
        )
    })?;
    if token.trim().is_empty() {
        return Err(format!(
            "Vault token environment variable {} is empty",
            effective.token_env
        )
        .into());
    }

    let mut address = effective.address.trim_end_matches('/').to_string();
    let want_port_forward = should_port_forward(&address, effective.kube_enabled, port_forward);
    let mut port_forward_guard = None;
    if !address_reachable(&address) {
        if !want_port_forward {
            return Err(format!(
                "Vault at {address} is unreachable; start Vault, correct secrets.vault.address, or pass --port-forward explicitly"
            )
            .into());
        }
        log::info!(
            "Vault at {} is unreachable; opening kubectl port-forward to {}/{}",
            address,
            effective.kube_namespace,
            effective.kube_service
        );
        port_forward_guard = Some(start_port_forward(&effective)?);
        address = format!("http://127.0.0.1:{}", effective.kube_local_port);
        wait_for_address(&address, Duration::from_secs(20))?;
    }

    let session = VaultSession {
        address,
        token,
        mount: validate_vault_path(&effective.mount, "secrets.vault.mount", false)?,
        version: effective.version,
        _port_forward: port_forward_guard,
    };
    probe_vault(&session, &effective.token_env)?;
    Ok(session)
}

fn probe_vault(session: &VaultSession, token_env: &str) -> Result<(), Box<dyn Error>> {
    let health_url = format!("{}/v1/sys/health", session.address);
    match ureq::get(&health_url)
        .timeout(Duration::from_secs(5))
        .call()
    {
        Ok(_)
        | Err(ureq::Error::Status(429, _))
        | Err(ureq::Error::Status(472, _))
        | Err(ureq::Error::Status(473, _))
        | Err(ureq::Error::Status(501, _))
        | Err(ureq::Error::Status(503, _)) => {}
        Err(ureq::Error::Status(code, _)) => {
            return Err(format!("Vault health check failed with HTTP {code}").into());
        }
        Err(_) => return Err("Vault health check failed before receiving a response".into()),
    }

    let lookup_url = format!("{}/v1/auth/token/lookup-self", session.address);
    match ureq::get(&lookup_url)
        .set("X-Vault-Token", &session.token)
        .timeout(Duration::from_secs(5))
        .call()
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, _)) => {
            Err(format!("Vault rejected the token from {token_env} with HTTP {code}").into())
        }
        Err(_) => Err("Vault token validation failed before receiving a response".into()),
    }
}

fn address_reachable(address: &str) -> bool {
    let Some((host, port)) = parse_host_port(address) else {
        return false;
    };
    let Ok(addresses) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    any_address_reachable(addresses)
}

fn any_address_reachable(addresses: impl IntoIterator<Item = SocketAddr>) -> bool {
    addresses
        .into_iter()
        .any(|address| TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_ok())
}

fn should_port_forward(address: &str, kube_enabled: bool, requested: Option<bool>) -> bool {
    requested.unwrap_or_else(|| kube_enabled && is_loopback_address(address))
}

fn is_loopback_address(address: &str) -> bool {
    let Some((host, _)) = parse_host_port(address) else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn parse_host_port(address: &str) -> Option<(String, u16)> {
    let trimmed = address.trim();
    let (scheme, remainder) = trimmed.split_once("://")?;
    let default_port = match scheme {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = remainder.split('/').next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']')?;
        let port = if suffix.is_empty() {
            default_port
        } else {
            suffix.strip_prefix(':')?.parse().ok()?
        };
        return Some((host.to_string(), port));
    }
    if let Some((host, port)) = authority.rsplit_once(':') {
        if host.is_empty() || host.contains(':') {
            return None;
        }
        return Some((host.to_string(), port.parse().ok()?));
    }
    Some((authority.to_string(), default_port))
}

fn start_port_forward(
    settings: &VaultSecretsRuntimeConfig,
) -> Result<PortForwardGuard, Box<dyn Error>> {
    let mut command = Command::new("kubectl");
    if let Some(context) = &settings.kube_context {
        command.arg("--context").arg(context);
    }
    let child = command
        .arg("--namespace")
        .arg(&settings.kube_namespace)
        .arg("port-forward")
        .arg(format!("service/{}", settings.kube_service))
        .arg(format!("{}:8200", settings.kube_local_port))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("failed to start kubectl port-forward: {error}"))?;
    Ok(PortForwardGuard { child })
}

fn wait_for_address(address: &str, timeout: Duration) -> Result<(), Box<dyn Error>> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if address_reachable(address) {
            thread::sleep(Duration::from_millis(200));
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err(format!("timed out waiting for Vault port-forward at {address}").into())
}

impl VaultSession {
    fn data_url(&self, secret_path: &str) -> String {
        if self.version == "v1" {
            format!("{}/v1/{}/{}", self.address, self.mount, secret_path)
        } else {
            format!("{}/v1/{}/data/{}", self.address, self.mount, secret_path)
        }
    }

    pub(crate) fn read_data(
        &self,
        secret_path: &str,
    ) -> Result<Option<Map<String, JsonValue>>, Box<dyn Error>> {
        let url = self.data_url(secret_path);
        match ureq::get(&url)
            .set("X-Vault-Token", &self.token)
            .timeout(Duration::from_secs(15))
            .call()
        {
            Ok(response) => {
                let body: JsonValue = response.into_json().map_err(|_| {
                    format!("Vault returned invalid JSON while reading {secret_path:?}")
                })?;
                let data = if self.version == "v1" {
                    body.get("data")
                } else {
                    body.pointer("/data/data")
                };
                match data {
                    Some(JsonValue::Object(map)) => Ok(Some(map.clone())),
                    Some(_) => Err(format!(
                        "Vault path {secret_path:?} returned non-object secret data"
                    )
                    .into()),
                    None => Err(format!(
                        "Vault path {secret_path:?} response did not contain secret data"
                    )
                    .into()),
                }
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(ureq::Error::Status(code, _)) => {
                Err(format!("Vault read failed for {secret_path:?} with HTTP {code}").into())
            }
            Err(_) => Err(format!(
                "Vault read failed for {secret_path:?} before receiving a response"
            )
            .into()),
        }
    }

    pub(crate) fn write_data(
        &self,
        secret_path: &str,
        data: &Map<String, JsonValue>,
    ) -> Result<(), Box<dyn Error>> {
        let url = self.data_url(secret_path);
        let body = if self.version == "v1" {
            JsonValue::Object(data.clone())
        } else {
            json!({ "data": data })
        };
        match ureq::post(&url)
            .set("X-Vault-Token", &self.token)
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(15))
            .send_json(body)
        {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, _)) => {
                Err(format!("Vault write failed for {secret_path:?} with HTTP {code}").into())
            }
            Err(_) => Err(format!(
                "Vault write failed for {secret_path:?} before receiving a response"
            )
            .into()),
        }
    }
}

pub(crate) fn json_maps_equal(
    left: &Map<String, JsonValue>,
    right: &Map<String, JsonValue>,
) -> bool {
    left == right
}

pub(crate) fn object_to_vault_map(
    value: &JsonValue,
    path_label: &str,
) -> Result<Map<String, JsonValue>, Box<dyn Error>> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("Vault secret JSON must be an object: {path_label}"))?;
    let mut map = Map::new();
    for (key, value) in object {
        let value = match value {
            JsonValue::String(_) | JsonValue::Number(_) | JsonValue::Bool(_) | JsonValue::Null => {
                value.clone()
            }
            nested => JsonValue::String(nested.to_string()),
        };
        map.insert(key.clone(), value);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn validates_and_normalizes_vault_paths() {
        assert_eq!(
            validate_vault_path("/harmony/stripe/", "path", false).unwrap(),
            "harmony/stripe"
        );
        assert!(validate_vault_path("harmony/../stripe", "path", false).is_err());
        assert!(validate_vault_path("harmony//stripe", "path", false).is_err());
    }

    #[test]
    fn parses_vault_addresses_without_credentials() {
        assert_eq!(
            parse_host_port("http://127.0.0.1:8200"),
            Some(("127.0.0.1".to_string(), 8200))
        );
        assert_eq!(
            parse_host_port("https://vault.example.com"),
            Some(("vault.example.com".to_string(), 443))
        );
        assert!(parse_host_port("http://token@vault.example.com").is_none());
    }

    #[test]
    fn implicit_port_forwarding_is_limited_to_loopback_addresses() {
        assert!(should_port_forward("http://127.0.0.1:8200", true, None));
        assert!(should_port_forward("http://[::1]:8200", true, None));
        assert!(!should_port_forward(
            "https://vault.example.com",
            true,
            None
        ));
        assert!(should_port_forward(
            "https://vault.example.com",
            false,
            Some(true)
        ));
        assert!(!should_port_forward(
            "http://127.0.0.1:8200",
            true,
            Some(false)
        ));
    }

    #[test]
    fn reachability_checks_every_resolved_address() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let reachable = listener.local_addr().unwrap();
        let unreachable: SocketAddr = "127.0.0.1:0".parse().unwrap();

        assert!(any_address_reachable([unreachable, reachable]));
    }

    #[test]
    fn vault_converts_nested_json_without_exposing_or_dropping_values() {
        let map = object_to_vault_map(
            &json!({"plain": "value", "nested": {"enabled": true}}),
            "fixture.json",
        )
        .unwrap();
        assert_eq!(map["plain"], json!("value"));
        assert_eq!(map["nested"], json!("{\"enabled\":true}"));
        assert!(json_maps_equal(&map, &map.clone()));
    }
}
