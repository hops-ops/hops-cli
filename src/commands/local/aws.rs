use super::gitops_write::{log_written, write_gitops_files, GitopsFile};
use super::workbench::controller::reject_imperative_owner;
use super::{kubectl_apply_stdin, run_cmd, run_cmd_output};
use clap::Args;
use serde::Deserialize;
use std::error::Error;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

const DEFAULT_PROVIDER_PACKAGE: &str =
    "xpkg.crossplane.io/crossplane-contrib/provider-family-aws:v2.4.0";
const DEFAULT_PROVIDER_NAME: &str = "crossplane-contrib-provider-family-aws";
const DEFAULT_AWS_REGION: &str = "us-east-2";
const DEFAULT_AWS_RUNTIME_CONFIG_NAME: &str = "aws";
const PROVIDER_CONFIG_CRD: &str = "providerconfigs.aws.m.upbound.io";

#[derive(Args, Debug)]
pub struct AwsArgs {
    /// AWS CLI profile to source credentials from
    /// (falls back to AWS_PROFILE/AWS_DEFAULT_PROFILE, then prompts)
    #[arg(long, short = 'p')]
    pub profile: Option<String>,

    /// AWS region to write into the ProviderConfig credentials file
    /// (falls back to AWS_REGION/AWS_DEFAULT_REGION, then us-east-2)
    #[arg(long, short = 'r')]
    pub region: Option<String>,

    /// Namespace for the generated Secret and ProviderConfig
    #[arg(long, short = 'n', default_value = "default")]
    pub namespace: String,

    /// Secret name that stores generated AWS credentials
    #[arg(long, default_value = "aws-creds")]
    pub secret_name: String,

    /// ProviderConfig name to create/update
    #[arg(long, default_value = "default")]
    pub provider_config_name: String,

    /// DeploymentRuntimeConfig name to use for AWS provider pods
    #[arg(long, default_value = DEFAULT_AWS_RUNTIME_CONFIG_NAME)]
    pub runtime_config_name: String,

    /// Provider resource name for provider-family-aws
    #[arg(long, default_value = DEFAULT_PROVIDER_NAME)]
    pub provider_name: String,

    /// provider-family-aws package reference
    #[arg(long, default_value = DEFAULT_PROVIDER_PACKAGE)]
    pub provider_package: String,

    /// Refresh credentials and AWS runtime region in the live, non-GitOps mode
    #[arg(long)]
    pub refresh: bool,

    /// Write non-secret Provider / DeploymentRuntimeConfig / ProviderConfig YAML
    /// under this directory (e.g. `./.gitops/local/cluster`). Credential Secrets are
    /// **not** written — applied live only. In GitOps mode this is the only
    /// Kubernetes object this command applies.
    #[arg(long)]
    pub gitops: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct AwsExportCredentials {
    #[serde(rename = "AccessKeyId")]
    access_key_id: String,
    #[serde(rename = "SecretAccessKey")]
    secret_access_key: String,
    #[serde(rename = "SessionToken")]
    session_token: Option<String>,
}

pub fn run(args: &AwsArgs) -> Result<(), Box<dyn Error>> {
    if args.gitops.is_none() {
        reject_imperative_owner(&super::backend::kind::active_cluster_name())?;
    }
    let profile = resolve_profile(args.profile.as_deref())?;
    let region = resolve_region(args.region.as_deref())?;

    log::info!("Exporting AWS credentials from profile '{}'...", profile);
    let creds = export_credentials(&profile)?;
    let credentials_ini = build_credentials_ini(&creds, &region);

    let runtime_yaml = build_runtime_config_yaml(&args.runtime_config_name, &region);
    let provider_yaml = build_provider_yaml(
        &args.provider_name,
        &args.provider_package,
        &args.runtime_config_name,
    );
    let provider_config_yaml = build_provider_config_yaml(
        &args.namespace,
        &args.provider_config_name,
        &args.secret_name,
    );

    // GitOps owns every non-secret Kubernetes object above the bare control
    // plane. Keep credentials live-only, but materialize the Provider,
    // ProviderConfig, and runtime declarations for the Cluster controller.
    // Returning here is important: applying these objects imperatively would
    // create a second state owner and makes `--gitops` unsafe to re-run.
    if let Some(gitops) = &args.gitops {
        let written = write_gitops_files(
            gitops,
            &[
                GitopsFile {
                    rel_path: "providers/aws-runtime.yaml".into(),
                    yaml: runtime_yaml.clone(),
                },
                GitopsFile {
                    rel_path: "providers/aws.yaml".into(),
                    yaml: provider_yaml.clone(),
                },
                GitopsFile {
                    rel_path: "providers/aws-provider-config.yaml".into(),
                    yaml: provider_config_yaml.clone(),
                },
            ],
        )?;
        log_written(&written);
        log::info!(
            "Applying credential secret '{}/{}'; non-secret AWS manifests are file-owned under {}",
            args.namespace,
            args.secret_name,
            gitops.display()
        );
        apply_gitops_secret(
            &args.namespace,
            &args.secret_name,
            &credentials_ini,
            kubectl_apply_stdin,
        )?;
        log::info!(
            "AWS credentials secret refreshed from profile '{}' for region '{}' ({}/{})",
            profile,
            region,
            args.namespace,
            args.secret_name
        );
        return Ok(());
    }

    if args.refresh {
        log::info!(
            "Applying AWS provider runtime '{}' for region '{}'...",
            args.runtime_config_name,
            region
        );
        kubectl_apply_stdin(&build_runtime_config_yaml(
            &args.runtime_config_name,
            &region,
        ))?;

        log::info!(
            "Refreshing secret '{}/{}' with generated credentials...",
            args.namespace,
            args.secret_name
        );
        kubectl_apply_stdin(&build_secret_yaml(
            &args.namespace,
            &args.secret_name,
            &credentials_ini,
        ))?;
        log::info!(
            "AWS credentials secret refreshed from profile '{}' for region '{}' ({}/{})",
            profile,
            region,
            args.namespace,
            args.secret_name
        );
        return Ok(());
    }

    log::info!(
        "Applying AWS provider runtime '{}' for region '{}'...",
        args.runtime_config_name,
        region
    );
    kubectl_apply_stdin(&runtime_yaml)?;

    log::info!(
        "Applying provider-family-aws package '{}'...",
        args.provider_package
    );
    kubectl_apply_stdin(&provider_yaml)?;

    wait_for_crd(PROVIDER_CONFIG_CRD)?;

    log::info!(
        "Applying secret '{}/{}' with generated credentials...",
        args.namespace,
        args.secret_name
    );
    kubectl_apply_stdin(&build_secret_yaml(
        &args.namespace,
        &args.secret_name,
        &credentials_ini,
    ))?;

    log::info!(
        "Applying ProviderConfig '{}/{}'...",
        args.namespace,
        args.provider_config_name
    );
    kubectl_apply_stdin(&provider_config_yaml)?;

    log::info!(
        "AWS provider configured from profile '{}' for region '{}' (ProviderConfig: {}/{})",
        profile,
        region,
        args.namespace,
        args.provider_config_name
    );
    Ok(())
}

/// Apply only the live credential prerequisite for a GitOps AWS setup. The
/// callback keeps the ownership boundary testable without invoking kubectl.
fn apply_gitops_secret<F>(
    namespace: &str,
    secret_name: &str,
    credentials_ini: &str,
    mut apply: F,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut(&str) -> Result<(), Box<dyn Error>>,
{
    apply(&build_secret_yaml(namespace, secret_name, credentials_ini))
}

fn resolve_profile(cli_profile: Option<&str>) -> Result<String, Box<dyn Error>> {
    let env_profile = std::env::var("AWS_PROFILE").ok();
    let env_default_profile = std::env::var("AWS_DEFAULT_PROFILE").ok();

    if let Some(profile) = select_profile(
        cli_profile,
        env_profile.as_deref(),
        env_default_profile.as_deref(),
    ) {
        return Ok(profile);
    }

    prompt_for_profile()
}

fn resolve_region(cli_region: Option<&str>) -> Result<String, Box<dyn Error>> {
    let env_region = std::env::var("AWS_REGION").ok();
    let env_default_region = std::env::var("AWS_DEFAULT_REGION").ok();

    let region = select_region(
        cli_region,
        env_region.as_deref(),
        env_default_region.as_deref(),
    );

    if !region
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(format!("Invalid AWS region '{}'.", region).into());
    }

    Ok(region.to_string())
}

fn select_region<'a>(
    cli_region: Option<&'a str>,
    env_region: Option<&'a str>,
    env_default_region: Option<&'a str>,
) -> &'a str {
    [cli_region, env_region, env_default_region]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|region| !region.is_empty())
        .unwrap_or(DEFAULT_AWS_REGION)
}

fn select_profile(
    cli_profile: Option<&str>,
    env_profile: Option<&str>,
    env_default_profile: Option<&str>,
) -> Option<String> {
    [cli_profile, env_profile, env_default_profile]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|profile| !profile.is_empty())
        .map(str::to_string)
}

fn prompt_for_profile() -> Result<String, Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "AWS profile is not set. Pass `--profile <name>` or set AWS_PROFILE/AWS_DEFAULT_PROFILE."
                .into(),
        );
    }

    print!("AWS profile is not set. Enter AWS profile name: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let profile = input.trim();

    if profile.is_empty() {
        return Err("No AWS profile provided. Pass `--profile <name>`.".into());
    }

    Ok(profile.to_string())
}

fn export_credentials(profile: &str) -> Result<AwsExportCredentials, Box<dyn Error>> {
    let output = match run_aws_export_credentials(profile) {
        Ok(output) => output,
        Err(initial_err) => {
            if sso_login_required(&initial_err) {
                if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                    return Err(format!(
                        "failed to export credentials for profile '{}': {}\nSSO login is required, but no interactive terminal was detected. Run `aws sso login --profile {}` first.",
                        profile, initial_err, profile
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
                        "failed to export credentials for profile '{}': {}\nAttempted `aws sso login --profile {}`, but login failed: {}",
                        profile, initial_err, profile, login_err
                    )
                })?;

                run_aws_export_credentials(profile).map_err(|retry_err| {
                    format!(
                        "failed to export credentials for profile '{}': {}\nAttempted `aws sso login --profile {}` and retried export, but it still failed: {}",
                        profile, initial_err, profile, retry_err
                    )
                })?
            } else {
                return Err(format!(
                    "failed to export credentials for profile '{}': {}\nIf this is an SSO profile, run `aws sso login --profile {}` first.",
                    profile, initial_err, profile
                )
                .into());
            }
        }
    };

    let creds: AwsExportCredentials = serde_json::from_str(&output).map_err(|err| {
        format!(
            "failed to parse credential JSON for profile '{}': {}",
            profile, err
        )
    })?;

    if creds.access_key_id.trim().is_empty() || creds.secret_access_key.trim().is_empty() {
        return Err(format!(
            "AWS profile '{}' returned empty access key or secret key",
            profile
        )
        .into());
    }

    Ok(creds)
}

fn run_aws_export_credentials(profile: &str) -> Result<String, String> {
    run_cmd_output(
        "aws",
        &[
            "configure",
            "export-credentials",
            "--profile",
            profile,
            "--format",
            "process",
        ],
    )
    .map_err(|err| err.to_string())
}

fn sso_login_required(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("error loading sso token")
        || lower.contains("token for") && lower.contains("does not exist")
        || lower.contains("sso session associated with this profile has expired")
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

fn build_credentials_ini(creds: &AwsExportCredentials, region: &str) -> String {
    let mut ini = format!(
        "[default]\naws_access_key_id = {}\naws_secret_access_key = {}\nregion = {}\n",
        creds.access_key_id, creds.secret_access_key, region
    );

    if let Some(session_token) = creds.session_token.as_deref() {
        if !session_token.trim().is_empty() {
            ini.push_str(&format!("aws_session_token = {}\n", session_token));
        }
    }

    ini
}

fn build_runtime_config_yaml(runtime_config_name: &str, region: &str) -> String {
    format!(
        "apiVersion: pkg.crossplane.io/v1beta1\nkind: DeploymentRuntimeConfig\nmetadata:\n  name: {runtime_config_name}\nspec:\n  deploymentTemplate:\n    spec:\n      selector: {{}}\n      template:\n        spec:\n          containers:\n            - name: package-runtime\n              env:\n                - name: AWS_REGION\n                  value: {region}\n                - name: AWS_DEFAULT_REGION\n                  value: {region}\n"
    )
}

fn build_provider_yaml(
    provider_name: &str,
    provider_package: &str,
    runtime_config_name: &str,
) -> String {
    format!(
        "apiVersion: pkg.crossplane.io/v1\nkind: Provider\nmetadata:\n  name: {provider_name}\nspec:\n  package: {provider_package}\n  runtimeConfigRef:\n    name: {runtime_config_name}\n"
    )
}

fn build_secret_yaml(namespace: &str, secret_name: &str, credentials_ini: &str) -> String {
    let credentials_block = indent_block(credentials_ini, 4);
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
        "apiVersion: aws.m.upbound.io/v1beta1\nkind: ProviderConfig\nmetadata:\n  name: {provider_config_name}\n  namespace: {namespace}\nspec:\n  credentials:\n    source: Secret\n    secretRef:\n      namespace: {namespace}\n      name: {secret_name}\n      key: credentials\n"
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
    fn select_profile_prefers_cli_then_envs() {
        assert_eq!(
            select_profile(Some("cli"), Some("env"), Some("default-env")),
            Some("cli".to_string())
        );
        assert_eq!(
            select_profile(None, Some("env"), Some("default-env")),
            Some("env".to_string())
        );
        assert_eq!(
            select_profile(None, None, Some("default-env")),
            Some("default-env".to_string())
        );
    }

    #[test]
    fn select_profile_ignores_blank_values() {
        assert_eq!(
            select_profile(Some("   "), Some(""), Some("  default  ")),
            Some("default".to_string())
        );
        assert_eq!(select_profile(Some(""), Some(" "), Some("")), None);
    }

    #[test]
    fn sso_login_required_detects_missing_or_expired_token_errors() {
        assert!(sso_login_required(
            "aws exited with exit status: 255: Error loading SSO Token: Token for hops does not exist"
        ));
        assert!(sso_login_required(
            "The SSO session associated with this profile has expired or is otherwise invalid."
        ));
        assert!(!sso_login_required(
            "Unable to retrieve credentials: no credentials found"
        ));
    }

    #[test]
    fn credentials_ini_includes_session_token_when_present() {
        let creds = AwsExportCredentials {
            access_key_id: "AKIA...".to_string(),
            secret_access_key: "secret".to_string(),
            session_token: Some("token".to_string()),
        };

        let ini = build_credentials_ini(&creds, "us-east-2");
        assert!(ini.contains("aws_access_key_id = AKIA..."));
        assert!(ini.contains("aws_secret_access_key = secret"));
        assert!(ini.contains("region = us-east-2"));
        assert!(ini.contains("aws_session_token = token"));
    }

    #[test]
    fn resolve_region_prefers_cli_then_envs_then_default() {
        assert_eq!(
            select_region(Some("us-west-2"), Some("eu-west-1"), Some("ap-south-1")),
            "us-west-2"
        );
        assert_eq!(
            select_region(None, Some("eu-west-1"), Some("ap-south-1")),
            "eu-west-1"
        );
        assert_eq!(select_region(None, None, Some("ap-south-1")), "ap-south-1");
        assert_eq!(select_region(None, None, None), DEFAULT_AWS_REGION);
    }

    #[test]
    fn resolve_region_rejects_unexpected_characters() {
        assert!(resolve_region(Some("us-east-2;rm")).is_err());
    }

    #[test]
    fn provider_config_yaml_uses_secret_ref() {
        let yaml = build_provider_config_yaml("default", "default", "aws-creds");
        assert!(yaml.contains("apiVersion: aws.m.upbound.io/v1beta1"));
        assert!(yaml.contains("kind: ProviderConfig"));
        assert!(yaml.contains("name: aws-creds"));
        assert!(yaml.contains("key: credentials"));
    }

    #[test]
    fn runtime_config_yaml_sets_aws_region_env() {
        let yaml = build_runtime_config_yaml("aws", "us-east-2");
        assert!(yaml.contains("kind: DeploymentRuntimeConfig"));
        assert!(yaml.contains("name: aws"));
        assert!(yaml.contains("name: AWS_REGION"));
        assert!(yaml.contains("value: us-east-2"));
        assert!(yaml.contains("name: AWS_DEFAULT_REGION"));
    }

    #[test]
    fn provider_yaml_uses_aws_runtime_config() {
        let yaml = build_provider_yaml("aws-provider", "xpkg.example/provider:v1", "aws");
        assert!(yaml.contains("kind: Provider"));
        assert!(yaml.contains("name: aws-provider"));
        assert!(yaml.contains("package: xpkg.example/provider:v1"));
        assert!(yaml.contains("runtimeConfigRef:"));
        assert!(yaml.contains("name: aws"));
    }

    #[test]
    fn gitops_applies_only_the_credential_secret() {
        let mut applied = Vec::new();
        apply_gitops_secret(
            "default",
            "aws-creds",
            "[default]\nregion = us-east-2\n",
            |yaml| {
                applied.push(yaml.to_string());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(applied.len(), 1);
        assert!(applied[0].contains("kind: Secret"));
        assert!(!applied[0].contains("kind: Provider\n"));
        assert!(!applied[0].contains("kind: ProviderConfig\n"));
        assert!(!applied[0].contains("kind: DeploymentRuntimeConfig\n"));
    }
}
