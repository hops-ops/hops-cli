use super::gitops_write::{log_written, write_gitops_files, GitopsFile};
use super::{kubectl_apply_stdin, run_cmd_output};
use clap::Args;
use serde_json::json;
use std::error::Error;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

const DEFAULT_PROVIDER_PACKAGE: &str =
    "xpkg.crossplane.io/crossplane-contrib/provider-upjet-zitadel:v0.1.1";
const DEFAULT_PROVIDER_NAME: &str = "crossplane-contrib-provider-upjet-zitadel";
const PROVIDER_CONFIG_CRD: &str = "providerconfigs.zitadel.m.crossplane.io";

#[derive(Args, Debug)]
pub struct ZitadelArgs {
    /// Zitadel access token. Falls back to ZITADEL_ACCESS_TOKEN, then reads
    /// the AuthStack iam-admin PAT Secret from the source cluster.
    #[arg(long)]
    pub access_token: Option<String>,

    /// Zitadel API domain. Accepts either a host or issuer URL.
    #[arg(long, default_value = "auth.ops.com.ai")]
    pub domain: String,

    /// Zitadel API port stored in the provider credentials JSON.
    /// Defaults to an explicit --domain URL port when present, otherwise 443.
    #[arg(long)]
    pub port: Option<String>,

    /// Set Zitadel provider credentials.insecure=true
    #[arg(long)]
    pub insecure: bool,

    /// Kubernetes context containing the AuthStack iam-admin PAT Secret.
    /// Required — no platform-name default.
    #[arg(long)]
    pub source_context: String,

    /// Namespace containing the AuthStack iam-admin PAT Secret
    #[arg(long, default_value = "zitadel")]
    pub source_namespace: String,

    /// AuthStack Secret holding the iam-admin PAT
    #[arg(long, default_value = "iam-admin-pat")]
    pub source_secret_name: String,

    /// Key in the source Secret holding the iam-admin PAT
    #[arg(long, default_value = "pat")]
    pub source_secret_key: String,

    /// Namespace for the generated Secret and ProviderConfig
    #[arg(long, short = 'n', default_value = "default")]
    pub namespace: String,

    /// Secret name that stores generated Zitadel credentials JSON
    #[arg(long, default_value = "zitadel-credentials")]
    pub secret_name: String,

    /// ProviderConfig name to create/update
    #[arg(long, default_value = "default")]
    pub provider_config_name: String,

    /// Provider resource name for provider-upjet-zitadel
    #[arg(long, default_value = DEFAULT_PROVIDER_NAME)]
    pub provider_name: String,

    /// provider-upjet-zitadel package reference
    #[arg(long, default_value = DEFAULT_PROVIDER_PACKAGE)]
    pub provider_package: String,

    /// Refresh credentials in the secret only; skips Provider and ProviderConfig apply
    #[arg(long)]
    pub refresh: bool,

    /// Write non-secret Provider / ProviderConfig YAML under this directory
    /// (e.g. `./.gitops/cluster`). Credential Secrets are not written.
    #[arg(long)]
    pub gitops: Option<PathBuf>,
}

pub fn run(args: &ZitadelArgs) -> Result<(), Box<dyn Error>> {
    let access_token = resolve_access_token(args)?;
    let domain = normalize_domain(&args.domain)?;
    let port = effective_port(args.port.as_deref(), domain.port.as_deref());
    let credentials_json =
        build_credentials_json(&access_token, &domain.host, &port, args.insecure)?;

    if args.refresh {
        log::info!(
            "Refreshing secret '{}/{}' with generated Zitadel credentials...",
            args.namespace,
            args.secret_name
        );
        kubectl_apply_stdin(&build_secret_yaml(
            &args.namespace,
            &args.secret_name,
            &credentials_json,
        ))?;
        log::info!(
            "Zitadel credentials secret refreshed ({}/{})",
            args.namespace,
            args.secret_name
        );
        return Ok(());
    }

    let provider_yaml = build_provider_yaml(&args.provider_name, &args.provider_package);
    let provider_config_yaml = build_provider_config_yaml(
        &args.namespace,
        &args.provider_config_name,
        &args.secret_name,
    );

    if let Some(gitops) = &args.gitops {
        let written = write_gitops_files(
            gitops,
            &[
                GitopsFile {
                    rel_path: "providers/zitadel.yaml".into(),
                    yaml: provider_yaml.clone(),
                },
                GitopsFile {
                    rel_path: "providerconfigs/zitadel.yaml".into(),
                    yaml: provider_config_yaml.clone(),
                },
            ],
        )?;
        log_written(&written);
        log::info!(
            "Zitadel non-secret manifests written under {} (Secret still applied live only)",
            gitops.display()
        );
    }

    log::info!(
        "Applying provider-upjet-zitadel package '{}'...",
        args.provider_package
    );
    kubectl_apply_stdin(&provider_yaml)?;

    wait_for_crd(PROVIDER_CONFIG_CRD)?;

    log::info!(
        "Applying secret '{}/{}' with generated Zitadel credentials...",
        args.namespace,
        args.secret_name
    );
    kubectl_apply_stdin(&build_secret_yaml(
        &args.namespace,
        &args.secret_name,
        &credentials_json,
    ))?;

    log::info!(
        "Applying ProviderConfig '{}/{}'...",
        args.namespace,
        args.provider_config_name
    );
    kubectl_apply_stdin(&provider_config_yaml)?;

    log::info!(
        "Zitadel provider configured for '{}' (ProviderConfig: {}/{})",
        domain.host,
        args.namespace,
        args.provider_config_name
    );
    Ok(())
}

fn resolve_access_token(args: &ZitadelArgs) -> Result<String, Box<dyn Error>> {
    if let Some(token) = non_empty(args.access_token.as_deref()) {
        return Ok(token.to_string());
    }

    if let Ok(token) = std::env::var("ZITADEL_ACCESS_TOKEN") {
        if let Some(token) = non_empty(Some(&token)) {
            return Ok(token.to_string());
        }
    }

    read_source_secret_token(
        &args.source_context,
        &args.source_namespace,
        &args.source_secret_name,
        &args.source_secret_key,
    )
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn read_source_secret_token(
    context: &str,
    namespace: &str,
    secret_name: &str,
    secret_key: &str,
) -> Result<String, Box<dyn Error>> {
    log::info!(
        "Reading Zitadel PAT from {}/{} in context '{}'...",
        namespace,
        secret_name,
        context
    );

    let template = format!(
        "go-template={{{{ index .data {:?} | base64decode }}}}",
        secret_key
    );
    let output = Command::new("kubectl")
        .args([
            "--context",
            context,
            "get",
            "secret",
            secret_name,
            "-n",
            namespace,
            "-o",
            &template,
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("kubectl exited with {}: {}", output.status, stderr).into());
    }

    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(format!(
            "Secret {}/{} key '{}' in context '{}' is empty",
            namespace, secret_name, secret_key, context
        )
        .into());
    }
    Ok(token)
}

fn wait_for_crd(crd: &str) -> Result<(), Box<dyn Error>> {
    log::info!("Waiting for CRD {}...", crd);
    for _ in 0..60 {
        if run_cmd_output("kubectl", &["get", "crd", crd]).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(5));
    }

    Err(format!("Timed out waiting for CRD {}", crd).into())
}

#[derive(Debug, PartialEq, Eq)]
struct Domain {
    host: String,
    port: Option<String>,
}

fn normalize_domain(input: &str) -> Result<Domain, Box<dyn Error>> {
    let trimmed = input.trim().trim_end_matches('/');
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let host_port = without_scheme.split('/').next().unwrap_or("").trim();
    let (host, port) = split_host_port(host_port)?;

    if host.is_empty() {
        return Err("Zitadel domain is empty".into());
    }
    Ok(Domain {
        host: host.to_string(),
        port,
    })
}

fn split_host_port(host_port: &str) -> Result<(&str, Option<String>), Box<dyn Error>> {
    if let Some(rest) = host_port.strip_prefix('[') {
        let close_idx = rest
            .find(']')
            .ok_or("IPv6 domain literal is missing closing ']'")?
            + 1;
        let host = &host_port[..=close_idx];
        let suffix = &host_port[close_idx + 1..];
        if suffix.is_empty() {
            return Ok((host, None));
        }
        if let Some(port) = suffix.strip_prefix(':') {
            return Ok((host, non_empty(Some(port)).map(ToString::to_string)));
        }
        return Err("unexpected characters after IPv6 domain literal".into());
    }

    if host_port.matches(':').count() == 1 {
        let mut parts = host_port.splitn(2, ':');
        let host = parts.next().unwrap_or("").trim();
        let port = parts
            .next()
            .and_then(|port| non_empty(Some(port)))
            .map(ToString::to_string);
        return Ok((host, port));
    }

    Ok((host_port, None))
}

fn effective_port(cli_port: Option<&str>, domain_port: Option<&str>) -> String {
    non_empty(cli_port)
        .or_else(|| non_empty(domain_port))
        .unwrap_or("443")
        .to_string()
}

fn build_credentials_json(
    access_token: &str,
    domain: &str,
    port: &str,
    insecure: bool,
) -> Result<String, Box<dyn Error>> {
    serde_json::to_string(&json!({
        "access_token": access_token,
        "domain": domain,
        "port": port,
        "insecure": insecure,
    }))
    .map_err(|err| format!("failed to serialize Zitadel credentials JSON: {}", err).into())
}

fn build_provider_yaml(provider_name: &str, provider_package: &str) -> String {
    format!(
        "apiVersion: pkg.crossplane.io/v1\nkind: Provider\nmetadata:\n  name: {provider_name}\nspec:\n  package: {provider_package}\n"
    )
}

fn build_secret_yaml(namespace: &str, secret_name: &str, credentials_json: &str) -> String {
    let credentials_block = indent_block(credentials_json, 4);
    format!(
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: {secret_name}\n  namespace: {namespace}\ntype: Opaque\nstringData:\n  credentials: |\n{credentials_block}"
    )
}

fn build_provider_config_yaml(
    namespace: &str,
    provider_config_name: &str,
    secret_name: &str,
) -> String {
    format!(
        "apiVersion: zitadel.m.crossplane.io/v1beta1\nkind: ProviderConfig\nmetadata:\n  name: {provider_config_name}\n  namespace: {namespace}\nspec:\n  credentials:\n    source: Secret\n    secretRef:\n      namespace: {namespace}\n      name: {secret_name}\n      key: credentials\n"
    )
}

fn indent_block(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines()
        .map(|line| format!("{pad}{line}\n"))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_domain_accepts_host_or_url() {
        assert_eq!(
            normalize_domain("auth.ops.com.ai").unwrap(),
            Domain {
                host: "auth.ops.com.ai".to_string(),
                port: None
            }
        );
        assert_eq!(
            normalize_domain("https://auth.ops.com.ai").unwrap(),
            Domain {
                host: "auth.ops.com.ai".to_string(),
                port: None
            }
        );
        assert_eq!(
            normalize_domain("https://auth.ops.com.ai/ui/login").unwrap(),
            Domain {
                host: "auth.ops.com.ai".to_string(),
                port: None
            }
        );
        assert_eq!(
            normalize_domain("https://auth.ops.com.ai:443").unwrap(),
            Domain {
                host: "auth.ops.com.ai".to_string(),
                port: Some("443".to_string())
            }
        );
    }

    #[test]
    fn effective_port_prefers_cli_then_domain_then_default() {
        assert_eq!(effective_port(Some("9443"), Some("8443")), "9443");
        assert_eq!(effective_port(None, Some("8443")), "8443");
        assert_eq!(effective_port(None, None), "443");
    }

    #[test]
    fn credentials_json_matches_zitadel_provider_shape() {
        let json = build_credentials_json("pat", "auth.ops.com.ai", "443", false).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["access_token"], "pat");
        assert_eq!(value["domain"], "auth.ops.com.ai");
        assert_eq!(value["port"], "443");
        assert_eq!(value["insecure"], false);
    }

    #[test]
    fn provider_config_yaml_uses_secret_ref() {
        let yaml = build_provider_config_yaml("default", "default", "zitadel-credentials");
        assert!(yaml.contains("apiVersion: zitadel.m.crossplane.io/v1beta1"));
        assert!(yaml.contains("kind: ProviderConfig"));
        assert!(yaml.contains("name: zitadel-credentials"));
        assert!(yaml.contains("key: credentials"));
    }
}
