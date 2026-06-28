use super::{command_exists, kubectl_apply_stdin, run_cmd, run_cmd_output};
use clap::Args;
use serde_json::json;
use std::error::Error;
use std::io::{self, IsTerminal};
use std::thread;
use std::time::Duration;

const DEFAULT_DNS_PROVIDER_PACKAGE: &str =
    "xpkg.upbound.io/wildbitca/provider-cloudflare-dns:v0.2.5";
const DEFAULT_DNS_PROVIDER_NAME: &str = "wildbitca-provider-cloudflare-dns";
const PROVIDER_CONFIG_CRD: &str = "providerconfigs.upjet-cloudflare.m.upbound.io";
const DNS_RECORD_CRD: &str = "records.dns.upjet-cloudflare.m.upbound.io";

#[derive(Args, Debug)]
pub struct CloudflareArgs {
    /// Cloudflare API token. Falls back to CLOUDFLARE_API_TOKEN, then AWS Secrets Manager.
    #[arg(long)]
    pub api_token: Option<String>,

    /// AWS Secrets Manager secret ID used when no token is passed or exported.
    #[arg(long, default_value = "cloudflare/dns-edit")]
    pub aws_secret_id: String,

    /// JSON property inside the AWS Secrets Manager SecretString.
    #[arg(long, default_value = "token")]
    pub aws_secret_property: String,

    /// AWS CLI profile for reading the Cloudflare token from Secrets Manager.
    #[arg(long, short = 'p')]
    pub profile: Option<String>,

    /// Namespace for the generated Secret and ProviderConfig.
    #[arg(long, short = 'n', default_value = "default")]
    pub namespace: String,

    /// Secret name that stores generated Cloudflare credentials JSON.
    #[arg(long, default_value = "cloudflare-credentials")]
    pub secret_name: String,

    /// ProviderConfig name to create/update.
    #[arg(long, default_value = "default")]
    pub provider_config_name: String,

    /// Provider resource name for provider-cloudflare-dns.
    #[arg(long, default_value = DEFAULT_DNS_PROVIDER_NAME)]
    pub provider_name: String,

    /// provider-cloudflare-dns package reference.
    #[arg(long, default_value = DEFAULT_DNS_PROVIDER_PACKAGE)]
    pub provider_package: String,

    /// Refresh credentials in the Secret only; skips Provider and ProviderConfig apply.
    #[arg(long)]
    pub refresh: bool,
}

pub fn run(args: &CloudflareArgs) -> Result<(), Box<dyn Error>> {
    let token = resolve_api_token(args)?;
    let credentials_json = build_credentials_json(&token)?;

    if args.refresh {
        log::info!(
            "Refreshing secret '{}/{}' with generated Cloudflare credentials...",
            args.namespace,
            args.secret_name
        );
        kubectl_apply_stdin(&build_secret_yaml(
            &args.namespace,
            &args.secret_name,
            &credentials_json,
        ))?;
        log::info!(
            "Cloudflare credentials secret refreshed ({}/{})",
            args.namespace,
            args.secret_name
        );
        return Ok(());
    }

    log::info!(
        "Applying Cloudflare DNS provider package '{}'...",
        args.provider_package
    );
    kubectl_apply_stdin(&build_provider_yaml(
        &args.provider_name,
        &args.provider_package,
    ))?;

    wait_for_crd(PROVIDER_CONFIG_CRD)?;
    wait_for_crd(DNS_RECORD_CRD)?;

    log::info!(
        "Applying secret '{}/{}' with generated Cloudflare credentials...",
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
    kubectl_apply_stdin(&build_provider_config_yaml(
        &args.namespace,
        &args.provider_config_name,
        &args.secret_name,
    ))?;

    log::info!(
        "Cloudflare DNS provider configured (ProviderConfig: {}/{})",
        args.namespace,
        args.provider_config_name
    );
    Ok(())
}

fn resolve_api_token(args: &CloudflareArgs) -> Result<String, Box<dyn Error>> {
    if let Some(token) = non_empty(args.api_token.as_deref()) {
        return Ok(token.to_string());
    }

    if let Ok(token) = std::env::var("CLOUDFLARE_API_TOKEN") {
        if let Some(token) = non_empty(Some(&token)) {
            return Ok(token.to_string());
        }
    }

    let profile = resolve_profile(args.profile.as_deref());
    read_aws_secret_token(
        &args.aws_secret_id,
        &args.aws_secret_property,
        profile.as_deref(),
    )
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn resolve_profile(cli_profile: Option<&str>) -> Option<String> {
    let env_profile = std::env::var("AWS_PROFILE").ok();
    let env_default_profile = std::env::var("AWS_DEFAULT_PROFILE").ok();

    let profile = [
        cli_profile,
        env_profile.as_deref(),
        env_default_profile.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|profile| !profile.is_empty())
    .map(str::to_string);

    profile
}

fn read_aws_secret_token(
    secret_id: &str,
    property: &str,
    profile: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    if !command_exists("aws") {
        return Err(
            "Cloudflare API token is not set and AWS CLI (`aws`) is not in PATH. Pass `--api-token`, set CLOUDFLARE_API_TOKEN, or install AWS CLI."
                .into(),
        );
    }

    log::info!(
        "Reading Cloudflare API token from AWS Secrets Manager secret '{}'...",
        secret_id
    );
    let output = match run_aws_get_secret_value(secret_id, profile) {
        Ok(output) => output,
        Err(initial_err) => {
            if sso_login_required(&initial_err) {
                let profile = profile.ok_or_else(|| {
                    format!(
                        "failed to read AWS secret '{}': {}\nSSO login is required, but no AWS profile was selected.",
                        secret_id, initial_err
                    )
                })?;
                if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                    return Err(format!(
                        "failed to read AWS secret '{}': {}\nSSO login is required, but no interactive terminal was detected. Run `aws sso login --profile {}` first.",
                        secret_id, initial_err, profile
                    )
                    .into());
                }

                log::info!(
                    "AWS SSO token missing/expired for profile '{}'. Running `aws sso login --profile {}`...",
                    profile,
                    profile
                );
                run_cmd("aws", &["sso", "login", "--profile", profile]).map_err(|login_err| {
                    format!(
                        "failed to read AWS secret '{}': {}\nAttempted `aws sso login --profile {}`, but login failed: {}",
                        secret_id, initial_err, profile, login_err
                    )
                })?;
                run_aws_get_secret_value(secret_id, Some(profile)).map_err(|retry_err| {
                    format!(
                        "failed to read AWS secret '{}': {}\nAttempted `aws sso login --profile {}` and retried, but it still failed: {}",
                        secret_id, initial_err, profile, retry_err
                    )
                })?
            } else {
                return Err(format!(
                    "failed to read AWS secret '{}': {}\nPass `--api-token`, set CLOUDFLARE_API_TOKEN, or verify AWS credentials.",
                    secret_id, initial_err
                )
                .into());
            }
        }
    };

    extract_secret_property(output.trim(), property)
        .map_err(|err| format!("failed to extract Cloudflare token: {}", err).into())
}

fn run_aws_get_secret_value(secret_id: &str, profile: Option<&str>) -> Result<String, String> {
    let mut args = vec![
        "secretsmanager".to_string(),
        "get-secret-value".to_string(),
        "--secret-id".to_string(),
        secret_id.to_string(),
        "--query".to_string(),
        "SecretString".to_string(),
        "--output".to_string(),
        "text".to_string(),
    ];
    if let Some(profile) = profile {
        args.push("--profile".to_string());
        args.push(profile.to_string());
    }
    let refs: Vec<&str> = args.iter().map(|arg| arg.as_str()).collect();
    run_cmd_output("aws", &refs).map_err(|err| err.to_string())
}

fn sso_login_required(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("error loading sso token")
        || lower.contains("token for") && lower.contains("does not exist")
        || lower.contains("sso session associated with this profile has expired")
}

fn extract_secret_property(secret_string: &str, property: &str) -> Result<String, String> {
    let trimmed = secret_string.trim();
    if trimmed.is_empty() {
        return Err("AWS Secrets Manager returned an empty SecretString".to_string());
    }

    if property.trim().is_empty() {
        return Ok(trimmed.to_string());
    }

    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(value) => value
            .get(property)
            .and_then(|value| value.as_str())
            .and_then(|value| non_empty(Some(value)))
            .map(str::to_string)
            .ok_or_else(|| {
                format!(
                    "SecretString JSON does not contain non-empty property '{}'",
                    property
                )
            }),
        Err(_) if property == "token" => Ok(trimmed.to_string()),
        Err(err) => Err(format!(
            "SecretString is not JSON and property '{}' was requested: {}",
            property, err
        )),
    }
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

fn build_credentials_json(api_token: &str) -> Result<String, Box<dyn Error>> {
    serde_json::to_string(&json!({
        "api_token": api_token,
    }))
    .map_err(|err| format!("failed to serialize Cloudflare credentials JSON: {}", err).into())
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
        "apiVersion: upjet-cloudflare.m.upbound.io/v1beta1\nkind: ProviderConfig\nmetadata:\n  name: {provider_config_name}\n  namespace: {namespace}\nspec:\n  credentials:\n    source: Secret\n    secretRef:\n      namespace: {namespace}\n      name: {secret_name}\n      key: credentials\n"
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
    fn credentials_json_matches_cloudflare_provider_shape() {
        let json = build_credentials_json("cf-token").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["api_token"], "cf-token");
    }

    #[test]
    fn extracts_token_property_from_json_secret() {
        let token =
            extract_secret_property(r#"{"token":"cf-token","other":"ignored"}"#, "token").unwrap();
        assert_eq!(token, "cf-token");
    }

    #[test]
    fn default_token_property_accepts_plain_secret_string() {
        let token = extract_secret_property("cf-token", "token").unwrap();
        assert_eq!(token, "cf-token");
    }

    #[test]
    fn non_default_property_requires_json_secret_string() {
        let err = extract_secret_property("cf-token", "api_token").unwrap_err();
        assert!(err.contains("SecretString is not JSON"));
    }

    #[test]
    fn provider_config_yaml_uses_namespaced_wildbit_cloudflare_api_group() {
        let yaml = build_provider_config_yaml("default", "default", "cloudflare-credentials");
        assert!(yaml.contains("apiVersion: upjet-cloudflare.m.upbound.io/v1beta1"));
        assert!(yaml.contains("kind: ProviderConfig"));
        assert!(yaml.contains("namespace: default"));
        assert!(yaml.contains("name: cloudflare-credentials"));
        assert!(yaml.contains("key: credentials"));
    }

    #[test]
    fn provider_yaml_uses_wildbit_dns_provider_package() {
        let yaml = build_provider_yaml(DEFAULT_DNS_PROVIDER_NAME, DEFAULT_DNS_PROVIDER_PACKAGE);
        assert!(yaml.contains("name: wildbitca-provider-cloudflare-dns"));
        assert!(yaml.contains("xpkg.upbound.io/wildbitca/provider-cloudflare-dns:v0.2.5"));
    }
}
