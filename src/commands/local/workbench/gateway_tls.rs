//! HTTPS for the shared local Gateway on providers that publish it directly
//! on the host (OrbStack LoadBalancer on :80/:443).
//!
//! Hops keeps one local CA under `$HOME/.hops/local/tls` and issues the
//! Gateway a leaf certificate naming every `.localhost` HTTPRoute hostname
//! in the cluster. Browsers reject second-level wildcards like
//! `*.localhost`, so hostnames are listed exactly; the leaf is reissued when
//! the set changes. The result is the `platform-tls` Secret the Gateway's
//! `https` listener references. Trust the CA once with:
//!
//!     security add-trusted-cert -r trustRoot \
//!       -k ~/Library/Keychains/login.keychain-db ~/.hops/local/tls/ca.crt
//!
//! The CA carries name constraints (`localhost` and its subdomains,
//! 127.0.0.0/8), so even a leaked `ca.key` cannot mint a certificate for a
//! real site.

use super::ingress::ingress_routes_from_value;
use crate::commands::local::{kubectl_apply_stdin, kubectl_command, run_cmd};
use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};

pub const GATEWAY_TLS_SECRET: &str = "platform-tls";
pub const GATEWAY_NAMESPACE: &str = "istio-ingress";
const TLS_SUBDIR: &str = "tls";
/// Apple caps TLS server certificates at 825 days, user-trusted roots included.
const LEAF_DAYS: &str = "825";
const RENEW_WITHIN_SECS: &str = "2592000";
/// Names always on the leaf, whatever routes exist yet.
const BASE_NAMES: &[&str] = &["localhost", "*.hops.localhost"];
/// The IstioStack creates the namespace; give it this long on a fresh cluster.
const NAMESPACE_WAIT: Duration = Duration::from_secs(180);

/// Reissue the Gateway certificate when routes change and make sure the
/// cluster Secret holds it. Returns true when the Secret was (re)applied.
pub fn ensure_gateway_tls(state_dir: &Path) -> Result<bool, Box<dyn Error>> {
    let dir = state_dir.join(TLS_SUBDIR);
    fs::create_dir_all(&dir)?;
    // Keys are only ever written inside this directory.
    restrict(&dir, 0o700)?;
    ensure_ca(&dir)?;
    let names = desired_names(discover_route_hostnames()?);
    let issued = if leaf_is_current(&dir, &names) {
        false
    } else {
        issue_leaf(&dir, &names)?;
        true
    };
    if !issued && secret_exists()? {
        return Ok(false);
    }
    wait_for_namespace()?;
    apply_secret(&dir)?;
    log::info!(
        "Gateway TLS {GATEWAY_NAMESPACE}/{GATEWAY_TLS_SECRET} covers: {}",
        names.iter().cloned().collect::<Vec<_>>().join(", ")
    );
    Ok(true)
}

fn desired_names(hostnames: BTreeSet<String>) -> BTreeSet<String> {
    hostnames
        .into_iter()
        .filter(|hostname| hostname.ends_with(".localhost"))
        .chain(BASE_NAMES.iter().map(|name| name.to_string()))
        .collect()
}

fn discover_route_hostnames() -> Result<BTreeSet<String>, Box<dyn Error>> {
    let output = kubectl_command(&["get", "httproute", "-A", "-o", "json"])
        .output()
        .map_err(|error| format!("kubectl get httproute failed: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("the server doesn't have a resource type") {
            return Ok(BTreeSet::new());
        }
        return Err(format!("kubectl get httproute -A failed: {stderr}").into());
    }
    Ok(
        ingress_routes_from_value("", &serde_json::from_slice(&output.stdout)?)
            .into_iter()
            .map(|route| route.hostname)
            .collect(),
    )
}

fn path_str(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", path.display()).into())
}

fn ensure_ca(dir: &Path) -> Result<(), Box<dyn Error>> {
    let key = dir.join("ca.key");
    let crt = dir.join("ca.crt");
    if key.is_file() && crt.is_file() {
        warn_if_unconstrained(&crt);
        return Ok(());
    }
    log::info!("Creating the hops local CA in {}", dir.display());
    let config = dir.join("ca.cnf");
    fs::write(
        &config,
        "[req]\n\
         distinguished_name = dn\n\
         x509_extensions = ext\n\
         prompt = no\n\
         [dn]\n\
         CN = Hops Local CA\n\
         O = Hops\n\
         [ext]\n\
         basicConstraints = critical, CA:TRUE, pathlen:0\n\
         keyUsage = critical, keyCertSign, cRLSign\n\
         subjectKeyIdentifier = hash\n\
         nameConstraints = critical, permitted;DNS:localhost, permitted;IP:127.0.0.0/255.0.0.0\n",
    )?;
    run_cmd(
        "openssl",
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            path_str(&key)?,
            "-out",
            path_str(&crt)?,
            "-days",
            "3650",
            "-config",
            path_str(&config)?,
        ],
    )?;
    restrict(&key, 0o600)?;
    // A leaf from a previous CA would no longer chain to the trusted one.
    for stale in ["gateway.crt", "gateway.key", "gateway.names"] {
        let _ = fs::remove_file(dir.join(stale));
    }
    log::info!(
        "Trust it for browsers once: security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db {}",
        crt.display()
    );
    Ok(())
}

/// CAs from before name constraints keep working; say how to replace one.
fn warn_if_unconstrained(crt: &Path) {
    static WARNED: Once = Once::new();
    let Some(path) = crt.to_str() else {
        return;
    };
    let constrained = Command::new("openssl")
        .args(["x509", "-noout", "-text", "-in", path])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).contains("Name Constraints"))
        .unwrap_or(true);
    if !constrained {
        WARNED.call_once(|| {
            log::warn!(
                "The hops local CA {path} has no name constraints; delete {} and re-trust the new CA to limit it to localhost names",
                crt.parent().unwrap_or(crt).display()
            )
        });
    }
}

fn names_file_body(names: &BTreeSet<String>) -> String {
    names.iter().map(|name| format!("{name}\n")).collect()
}

fn leaf_is_current(dir: &Path, names: &BTreeSet<String>) -> bool {
    let crt = dir.join("gateway.crt");
    if !dir.join("gateway.key").is_file() || !crt.is_file() {
        return false;
    }
    if fs::read_to_string(dir.join("gateway.names")).ok() != Some(names_file_body(names)) {
        return false;
    }
    let (Some(crt), Some(ca)) = (crt.to_str(), dir.join("ca.crt").to_str().map(String::from))
    else {
        return false;
    };
    let succeeds = |args: &[&str]| {
        Command::new("openssl")
            .args(args)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    };
    // Chains to the current CA (catches a recreated CA) and is not expiring.
    succeeds(&["verify", "-CAfile", &ca, crt])
        && succeeds(&["x509", "-checkend", RENEW_WITHIN_SECS, "-noout", "-in", crt])
}

fn leaf_config(names: &BTreeSet<String>) -> String {
    let alt = names
        .iter()
        .enumerate()
        .map(|(index, name)| format!("DNS.{} = {name}\n", index + 1))
        .collect::<String>();
    format!(
        "[req]\n\
         distinguished_name = dn\n\
         prompt = no\n\
         [dn]\n\
         CN = hops local gateway\n\
         [ext]\n\
         subjectAltName = @alt\n\
         basicConstraints = CA:FALSE\n\
         keyUsage = critical, digitalSignature, keyEncipherment\n\
         extendedKeyUsage = serverAuth\n\
         authorityKeyIdentifier = keyid\n\
         [alt]\n\
         {alt}IP.1 = 127.0.0.1\n"
    )
}

fn issue_leaf(dir: &Path, names: &BTreeSet<String>) -> Result<(), Box<dyn Error>> {
    let config = dir.join("gateway.cnf");
    let key = dir.join("gateway.key");
    let csr = dir.join("gateway.csr");
    let crt = dir.join("gateway.crt");
    // Drop the old pair first, so a failed issuance never leaves a new key
    // beside the old certificate and names.
    for stale in [&crt, &dir.join("gateway.names")] {
        let _ = fs::remove_file(stale);
    }
    fs::write(&config, leaf_config(names))?;
    run_cmd(
        "openssl",
        &[
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            path_str(&key)?,
            "-out",
            path_str(&csr)?,
            "-config",
            path_str(&config)?,
        ],
    )?;
    restrict(&key, 0o600)?;
    run_cmd(
        "openssl",
        &[
            "x509",
            "-req",
            "-in",
            path_str(&csr)?,
            "-CA",
            path_str(&dir.join("ca.crt"))?,
            "-CAkey",
            path_str(&dir.join("ca.key"))?,
            "-CAcreateserial",
            "-out",
            path_str(&crt)?,
            "-days",
            LEAF_DAYS,
            "-sha256",
            "-extfile",
            path_str(&config)?,
            "-extensions",
            "ext",
        ],
    )?;
    let _ = fs::remove_file(&csr);
    fs::write(dir.join("gateway.names"), names_file_body(names))?;
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<(), Box<dyn Error>> {
    Ok(())
}

fn wait_for_namespace() -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    loop {
        let found = kubectl_command(&["get", "namespace", GATEWAY_NAMESPACE, "-o", "name"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if found {
            return Ok(());
        }
        if started.elapsed() >= NAMESPACE_WAIT {
            return Err(format!(
                "namespace {GATEWAY_NAMESPACE} did not appear within {}s; is the IstioStack ready?",
                NAMESPACE_WAIT.as_secs()
            )
            .into());
        }
        thread::sleep(Duration::from_secs(3));
    }
}

fn secret_exists() -> Result<bool, Box<dyn Error>> {
    let output = kubectl_command(&[
        "get",
        "secret",
        GATEWAY_TLS_SECRET,
        "-n",
        GATEWAY_NAMESPACE,
        "-o",
        "name",
    ])
    .output()?;
    Ok(output.status.success())
}

fn apply_secret(dir: &Path) -> Result<(), Box<dyn Error>> {
    let cert = format!("--cert={}", path_str(&dir.join("gateway.crt"))?);
    let key = format!("--key={}", path_str(&dir.join("gateway.key"))?);
    let output = kubectl_command(&[
        "create",
        "secret",
        "tls",
        GATEWAY_TLS_SECRET,
        "-n",
        GATEWAY_NAMESPACE,
        &cert,
        &key,
        "--dry-run=client",
        "-o",
        "yaml",
    ])
    .output()?;
    if !output.status.success() {
        return Err(format!(
            "render Secret {GATEWAY_NAMESPACE}/{GATEWAY_TLS_SECRET}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    kubectl_apply_stdin(&String::from_utf8(output.stdout)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_names_keep_local_hosts_and_base_names() {
        let names = desired_names(
            ["mobpit.localhost", "auth.hops.localhost", "example.com"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            vec![
                "*.hops.localhost",
                "auth.hops.localhost",
                "localhost",
                "mobpit.localhost"
            ]
        );
    }

    #[test]
    fn leaf_follows_the_current_ca() {
        let dir = std::env::temp_dir().join(format!("hops-gateway-tls-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let names: BTreeSet<String> = ["localhost", "mobpit.localhost"]
            .into_iter()
            .map(String::from)
            .collect();
        ensure_ca(&dir).unwrap();
        let ca_text = Command::new("openssl")
            .args(["x509", "-noout", "-text", "-in"])
            .arg(dir.join("ca.crt"))
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&ca_text.stdout).contains("Name Constraints"));
        assert!(!leaf_is_current(&dir, &names));
        issue_leaf(&dir, &names).unwrap();
        assert!(leaf_is_current(&dir, &names));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join("gateway.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // A recreated CA invalidates the old leaf.
        fs::remove_file(dir.join("ca.key")).unwrap();
        ensure_ca(&dir).unwrap();
        assert!(!leaf_is_current(&dir, &names));
        issue_leaf(&dir, &names).unwrap();
        assert!(leaf_is_current(&dir, &names));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaf_config_lists_every_name() {
        let names: BTreeSet<String> = ["localhost", "mobpit.localhost"]
            .into_iter()
            .map(String::from)
            .collect();
        let config = leaf_config(&names);
        assert!(config.contains("DNS.1 = localhost\n"));
        assert!(config.contains("DNS.2 = mobpit.localhost\n"));
        assert!(config.contains("IP.1 = 127.0.0.1\n"));
    }
}
