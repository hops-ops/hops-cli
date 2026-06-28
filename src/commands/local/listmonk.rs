use super::{kubectl_apply_stdin, run_cmd_output};
use clap::Args;
use serde_json::json;
use std::error::Error;
use std::process::Command;
use std::thread;
use std::time::Duration;

const DEFAULT_PROVIDER_PACKAGE: &str = "ghcr.io/hops-ops/provider-listmonk:v0.0.3";
const DEFAULT_PROVIDER_NAME: &str = "hops-ops-provider-listmonk";
const PROVIDER_CONFIG_CRD: &str = "providerconfigs.listmonk.crossplane.io";

#[derive(Args, Debug)]
pub struct ListmonkArgs {
    /// Listmonk API endpoint (e.g. http://marketing-listmonk.marketing.svc.cluster.local:9000).
    /// Falls back to LISTMONK_ENDPOINT, then derives from the source service.
    #[arg(long)]
    pub endpoint: Option<String>,

    /// Listmonk api-typed user username. Falls back to LISTMONK_USERNAME,
    /// then reads the `username` key from the source Secret.
    #[arg(long)]
    pub username: Option<String>,

    /// Listmonk api-typed user token (the user's password, since for
    /// type=api users the password IS the Basic-Auth token).
    /// Falls back to LISTMONK_TOKEN, then reads the `token` key from
    /// the source Secret.
    #[arg(long)]
    pub token: Option<String>,

    /// Kubernetes context containing the chart-bootstrapped
    /// provider-creds Secret. Required — no platform-name default.
    #[arg(long)]
    pub source_context: String,

    /// Namespace of the chart-bootstrapped provider-creds Secret.
    /// Default `marketing` (matches EmailMarketingStack v3 install ns).
    #[arg(long, default_value = "marketing")]
    pub source_namespace: String,

    /// Source Secret name. Default `marketing-listmonk-provider-creds`
    /// (the chart's `<release>-provider-creds` for a release named
    /// `marketing-listmonk`).
    #[arg(long, default_value = "marketing-listmonk-provider-creds")]
    pub source_secret_name: String,

    /// Key in the source Secret holding the api username. Default
    /// `username` (matches the chart's api-user-bootstrap Job output).
    #[arg(long, default_value = "username")]
    pub source_username_key: String,

    /// Key in the source Secret holding the api token. Default `token`.
    #[arg(long, default_value = "token")]
    pub source_token_key: String,

    /// Namespace where the credentials Secret + ProviderConfig get
    /// applied. ProviderConfig is cluster-scoped so its `namespace`
    /// metadata doesn't matter, but the credential Secret lives here.
    #[arg(long, short = 'n', default_value = "crossplane-system")]
    pub namespace: String,

    /// Secret name that stores generated Listmonk credentials JSON.
    #[arg(long, default_value = "listmonk-credentials")]
    pub secret_name: String,

    /// ProviderConfig name to create/update.
    #[arg(long, default_value = "default")]
    pub provider_config_name: String,

    /// Provider resource name for hops-ops/provider-listmonk.
    #[arg(long, default_value = DEFAULT_PROVIDER_NAME)]
    pub provider_name: String,

    /// provider-listmonk package reference.
    #[arg(long, default_value = DEFAULT_PROVIDER_PACKAGE)]
    pub provider_package: String,

    /// Refresh credentials in the Secret only; skips Provider and
    /// ProviderConfig apply.
    #[arg(long)]
    pub refresh: bool,
}

pub fn run(args: &ListmonkArgs) -> Result<(), Box<dyn Error>> {
    let username = resolve_username(args)?;
    let token = resolve_token(args)?;
    let endpoint = resolve_endpoint(args)?;
    let credentials_json = build_credentials_json(&endpoint, &username, &token)?;

    if args.refresh {
        log::info!(
            "Refreshing secret '{}/{}' with generated Listmonk credentials...",
            args.namespace,
            args.secret_name
        );
        kubectl_apply_stdin(&build_secret_yaml(
            &args.namespace,
            &args.secret_name,
            &credentials_json,
        ))?;
        log::info!(
            "Listmonk credentials secret refreshed ({}/{})",
            args.namespace,
            args.secret_name
        );
        return Ok(());
    }

    log::info!(
        "Applying provider-listmonk package '{}'...",
        args.provider_package
    );
    kubectl_apply_stdin(&build_provider_yaml(
        &args.provider_name,
        &args.provider_package,
    ))?;

    wait_for_crd(PROVIDER_CONFIG_CRD)?;

    log::info!(
        "Applying secret '{}/{}' with generated Listmonk credentials...",
        args.namespace,
        args.secret_name
    );
    kubectl_apply_stdin(&build_secret_yaml(
        &args.namespace,
        &args.secret_name,
        &credentials_json,
    ))?;

    log::info!("Applying ProviderConfig '{}'...", args.provider_config_name);
    kubectl_apply_stdin(&build_provider_config_yaml(
        &args.provider_config_name,
        &args.namespace,
        &args.secret_name,
    ))?;

    log::info!(
        "Listmonk provider configured for '{}' (ProviderConfig: {})",
        endpoint,
        args.provider_config_name
    );
    Ok(())
}

fn resolve_username(args: &ListmonkArgs) -> Result<String, Box<dyn Error>> {
    if let Some(value) = non_empty(args.username.as_deref()) {
        return Ok(value.to_string());
    }
    if let Ok(value) = std::env::var("LISTMONK_USERNAME") {
        if let Some(value) = non_empty(Some(&value)) {
            return Ok(value.to_string());
        }
    }
    read_source_secret_value(
        &args.source_context,
        &args.source_namespace,
        &args.source_secret_name,
        &args.source_username_key,
        "username",
    )
}

fn resolve_token(args: &ListmonkArgs) -> Result<String, Box<dyn Error>> {
    if let Some(value) = non_empty(args.token.as_deref()) {
        return Ok(value.to_string());
    }
    if let Ok(value) = std::env::var("LISTMONK_TOKEN") {
        if let Some(value) = non_empty(Some(&value)) {
            return Ok(value.to_string());
        }
    }
    read_source_secret_value(
        &args.source_context,
        &args.source_namespace,
        &args.source_secret_name,
        &args.source_token_key,
        "token",
    )
}

fn resolve_endpoint(args: &ListmonkArgs) -> Result<String, Box<dyn Error>> {
    if let Some(value) = non_empty(args.endpoint.as_deref()) {
        return Ok(value.to_string());
    }
    if let Ok(value) = std::env::var("LISTMONK_ENDPOINT") {
        if let Some(value) = non_empty(Some(&value)) {
            return Ok(value.to_string());
        }
    }
    // Derive from the source secret's namespace — assume the chart's
    // standard service naming `<release>-listmonk.<ns>.svc.cluster.local:9000`.
    // The convention is that the chart-bootstrapped Secret is named
    // `<release>-provider-creds`, so the release name is the secret
    // name minus the `-provider-creds` suffix.
    let release = args
        .source_secret_name
        .strip_suffix("-provider-creds")
        .ok_or(
            "--endpoint required when --source-secret-name doesn't end with `-provider-creds`",
        )?;
    Ok(format!(
        "http://{}.{}.svc.cluster.local:9000",
        release, args.source_namespace
    ))
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn read_source_secret_value(
    context: &str,
    namespace: &str,
    secret_name: &str,
    secret_key: &str,
    label: &str,
) -> Result<String, Box<dyn Error>> {
    log::info!(
        "Reading Listmonk {} from {}/{} in context '{}'...",
        label,
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

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return Err(format!(
            "Secret {}/{} key '{}' in context '{}' is empty",
            namespace, secret_name, secret_key, context
        )
        .into());
    }
    Ok(value)
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

fn build_credentials_json(
    endpoint: &str,
    username: &str,
    token: &str,
) -> Result<String, Box<dyn Error>> {
    serde_json::to_string(&json!({
        "endpoint": endpoint,
        "username": username,
        "token": token,
    }))
    .map_err(|err| format!("failed to serialize Listmonk credentials JSON: {}", err).into())
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
    provider_config_name: &str,
    secret_namespace: &str,
    secret_name: &str,
) -> String {
    format!(
        "apiVersion: listmonk.crossplane.io/v1beta1\nkind: ProviderConfig\nmetadata:\n  name: {provider_config_name}\nspec:\n  credentials:\n    source: Secret\n    secretRef:\n      namespace: {secret_namespace}\n      name: {secret_name}\n      key: credentials\n"
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
    fn credentials_json_matches_listmonk_provider_shape() {
        let json = build_credentials_json(
            "http://marketing-listmonk.marketing.svc.cluster.local:9000",
            "crossplane-provider",
            "secret-token-value",
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["endpoint"],
            "http://marketing-listmonk.marketing.svc.cluster.local:9000"
        );
        assert_eq!(value["username"], "crossplane-provider");
        assert_eq!(value["token"], "secret-token-value");
    }

    #[test]
    fn provider_config_yaml_uses_cluster_scoped_api_group() {
        let yaml =
            build_provider_config_yaml("default", "crossplane-system", "listmonk-credentials");
        assert!(yaml.contains("apiVersion: listmonk.crossplane.io/v1beta1"));
        assert!(yaml.contains("kind: ProviderConfig"));
        assert!(yaml.contains("name: default"));
        assert!(yaml.contains("namespace: crossplane-system"));
        assert!(yaml.contains("name: listmonk-credentials"));
        assert!(yaml.contains("key: credentials"));
    }

    #[test]
    fn provider_yaml_uses_v0_0_3_default() {
        let yaml = build_provider_yaml(DEFAULT_PROVIDER_NAME, DEFAULT_PROVIDER_PACKAGE);
        assert!(yaml.contains("name: hops-ops-provider-listmonk"));
        assert!(yaml.contains("ghcr.io/hops-ops/provider-listmonk"));
    }
}
