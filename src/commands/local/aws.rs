use super::{kubectl_apply_stdin, run_cmd, run_cmd_output};
use clap::Args;
use serde::Deserialize;
use std::error::Error;
use std::io::{self, IsTerminal, Write};
use std::thread;
use std::time::Duration;

const DEFAULT_PROVIDER_PACKAGE: &str =
    "xpkg.crossplane.io/crossplane-contrib/provider-family-aws:v2.4.0";
const DEFAULT_PROVIDER_NAME: &str = "crossplane-contrib-provider-family-aws";
const PROVIDER_CONFIG_CRD: &str = "providerconfigs.aws.m.upbound.io";

#[derive(Args, Debug)]
pub struct AwsArgs {
    /// AWS CLI profile to source credentials from
    /// (falls back to AWS_PROFILE/AWS_DEFAULT_PROFILE, then prompts)
    #[arg(long, short = 'p')]
    pub profile: Option<String>,

    /// Namespace for the generated Secret and ProviderConfig
    #[arg(long, short = 'n', default_value = "default")]
    pub namespace: String,

    /// Secret name that stores generated AWS credentials
    #[arg(long, default_value = "aws-creds")]
    pub secret_name: String,

    /// ProviderConfig name to create/update
    #[arg(long, default_value = "default")]
    pub provider_config_name: String,

    /// Provider resource name for provider-family-aws
    #[arg(long, default_value = DEFAULT_PROVIDER_NAME)]
    pub provider_name: String,

    /// provider-family-aws package reference
    #[arg(long, default_value = DEFAULT_PROVIDER_PACKAGE)]
    pub provider_package: String,
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
    let profile = resolve_profile(args.profile.as_deref())?;

    log::info!(
        "Exporting AWS credentials from profile '{}'...",
        profile
    );
    let creds = export_credentials(&profile)?;

    log::info!(
        "Applying provider-family-aws package '{}'...",
        args.provider_package
    );
    kubectl_apply_stdin(&build_provider_yaml(
        &args.provider_name,
        &args.provider_package,
    ))?;

    wait_for_crd(PROVIDER_CONFIG_CRD)?;

    log::info!(
        "Applying secret '{}/{}' with generated credentials...",
        args.namespace,
        args.secret_name
    );
    let credentials_ini = build_credentials_ini(&creds);
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
    kubectl_apply_stdin(&build_provider_config_yaml(
        &args.namespace,
        &args.provider_config_name,
        &args.secret_name,
    ))?;

    log::info!(
        "AWS provider configured from profile '{}' (ProviderConfig: {}/{})",
        profile,
        args.namespace,
        args.provider_config_name
    );
    Ok(())
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

fn build_credentials_ini(creds: &AwsExportCredentials) -> String {
    let mut ini = format!(
        "[default]\naws_access_key_id = {}\naws_secret_access_key = {}\n",
        creds.access_key_id, creds.secret_access_key
    );

    if let Some(session_token) = creds.session_token.as_deref() {
        if !session_token.trim().is_empty() {
            ini.push_str(&format!("aws_session_token = {}\n", session_token));
        }
    }

    ini
}

fn build_provider_yaml(provider_name: &str, provider_package: &str) -> String {
    format!(
        "apiVersion: pkg.crossplane.io/v1\nkind: Provider\nmetadata:\n  name: {provider_name}\nspec:\n  package: {provider_package}\n"
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

        let ini = build_credentials_ini(&creds);
        assert!(ini.contains("aws_access_key_id = AKIA..."));
        assert!(ini.contains("aws_secret_access_key = secret"));
        assert!(ini.contains("aws_session_token = token"));
    }

    #[test]
    fn provider_config_yaml_uses_secret_ref() {
        let yaml = build_provider_config_yaml("default", "default", "aws-creds");
        assert!(yaml.contains("apiVersion: aws.m.upbound.io/v1beta1"));
        assert!(yaml.contains("kind: ProviderConfig"));
        assert!(yaml.contains("name: aws-creds"));
        assert!(yaml.contains("key: credentials"));
    }
}
