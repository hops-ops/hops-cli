use super::{command_exists, kubectl_apply_stdin, run_cmd, run_cmd_output};
use clap::Args;
use serde_json::json;
use std::error::Error;
use std::io::{self, IsTerminal, Write};
use std::thread;
use std::time::Duration;

const DEFAULT_PROVIDER_PACKAGE: &str =
    "xpkg.crossplane.io/crossplane-contrib/provider-upjet-github:v0.19.0";
const DEFAULT_PROVIDER_NAME: &str = "crossplane-contrib-provider-upjet-github";
const PROVIDER_CONFIG_CRD: &str = "providerconfigs.github.m.upbound.io";
const GH_HOST: &str = "github.com";
const GH_LOGIN_SCOPES: &str = "repo,delete_repo,read:org,admin:org";

#[derive(Args, Debug)]
pub struct GithubArgs {
    /// GitHub owner (organization or user) to configure for the ProviderConfig
    /// (falls back to GH_OWNER/GITHUB_OWNER, then prompts with your gh login)
    #[arg(long, short = 'o')]
    pub owner: Option<String>,

    /// Namespace for the generated Secret and ProviderConfig
    #[arg(long, short = 'n', default_value = "default")]
    pub namespace: String,

    /// Secret name that stores generated GitHub credentials JSON
    #[arg(long, default_value = "github-creds")]
    pub secret_name: String,

    /// ProviderConfig name to create/update
    #[arg(long, default_value = "default")]
    pub provider_config_name: String,

    /// Provider resource name for provider-upjet-github
    #[arg(long, default_value = DEFAULT_PROVIDER_NAME)]
    pub provider_name: String,

    /// provider-upjet-github package reference
    #[arg(long, default_value = DEFAULT_PROVIDER_PACKAGE)]
    pub provider_package: String,

    /// Refresh credentials in the secret only; skips Provider and ProviderConfig apply
    #[arg(long)]
    pub refresh: bool,
}

pub fn run(args: &GithubArgs) -> Result<(), Box<dyn Error>> {
    if !command_exists("gh") {
        return Err(
            "GitHub CLI (`gh`) is not installed or not in PATH. Install it first, then rerun `hops local github`."
                .into(),
        );
    }

    log::info!("Exporting GitHub token from `gh auth token`...");
    let token = export_token()?;
    let inferred_owner = authenticated_login().ok();
    let owner = resolve_owner(args.owner.as_deref(), inferred_owner.as_deref())?;
    let credentials_json = build_credentials_json(&owner, &token)?;

    if args.refresh {
        log::info!(
            "Refreshing secret '{}/{}' with generated credentials...",
            args.namespace,
            args.secret_name
        );
        kubectl_apply_stdin(&build_secret_yaml(
            &args.namespace,
            &args.secret_name,
            &credentials_json,
        ))?;
        log::info!(
            "GitHub credentials secret refreshed for owner '{}' ({}/{})",
            owner,
            args.namespace,
            args.secret_name
        );
        return Ok(());
    }

    log::info!(
        "Applying provider-upjet-github package '{}'...",
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
        "GitHub provider configured for owner '{}' (ProviderConfig: {}/{})",
        owner,
        args.namespace,
        args.provider_config_name
    );
    Ok(())
}

fn resolve_owner(
    cli_owner: Option<&str>,
    inferred_owner: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let env_owner = std::env::var("GH_OWNER").ok();
    let env_github_owner = std::env::var("GITHUB_OWNER").ok();

    if let Some(owner) = select_owner(
        cli_owner,
        env_owner.as_deref(),
        env_github_owner.as_deref(),
    ) {
        return Ok(owner);
    }

    prompt_for_owner(inferred_owner)
}

fn select_owner(
    cli_owner: Option<&str>,
    env_owner: Option<&str>,
    env_github_owner: Option<&str>,
) -> Option<String> {
    [cli_owner, env_owner, env_github_owner]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|owner| !owner.is_empty())
        .map(str::to_string)
}

fn prompt_for_owner(default_owner: Option<&str>) -> Result<String, Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        if let Some(owner) = default_owner
            .map(str::trim)
            .filter(|owner| !owner.is_empty())
        {
            return Ok(owner.to_string());
        }

        return Err(
            "GitHub owner is not set. Pass `--owner <org-or-user>` or set GH_OWNER/GITHUB_OWNER."
                .into(),
        );
    }

    let prompt = match default_owner.map(str::trim).filter(|owner| !owner.is_empty()) {
        Some(default) => format!("GitHub owner is not set. Enter GitHub owner [{default}]: "),
        None => "GitHub owner is not set. Enter GitHub owner: ".to_string(),
    };

    print!("{prompt}");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let owner = input.trim();

    if !owner.is_empty() {
        return Ok(owner.to_string());
    }

    if let Some(default) = default_owner.map(str::trim).filter(|owner| !owner.is_empty()) {
        return Ok(default.to_string());
    }

    Err("No GitHub owner provided. Pass `--owner <org-or-user>`.".into())
}

fn export_token() -> Result<String, Box<dyn Error>> {
    let output = match run_gh_auth_token() {
        Ok(output) => output,
        Err(initial_err) => {
            if gh_login_required(&initial_err) {
                if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                    return Err(format!(
                        "failed to export GitHub token: {}\nGitHub CLI login is required, but no interactive terminal was detected. Run `{}` first.",
                        initial_err,
                        recommended_gh_login_command()
                    )
                    .into());
                }

                log::info!(
                    "GitHub CLI is not authenticated. If you need to re-auth explicitly, run: `{}`",
                    recommended_gh_login_command()
                );
                log::info!(
                    "Running `{}` and retrying...",
                    recommended_gh_login_command()
                );
                run_cmd(
                    "gh",
                    &["auth", "login", "-h", GH_HOST, "-w", "-s", GH_LOGIN_SCOPES],
                )
                .map_err(|login_err| {
                    format!(
                        "failed to export GitHub token: {}\nAttempted `{}`, but login failed: {}",
                        initial_err,
                        recommended_gh_login_command(),
                        login_err
                    )
                })?;

                run_gh_auth_token().map_err(|retry_err| {
                    format!(
                        "failed to export GitHub token: {}\nAttempted `{}` and retried export, but it still failed: {}",
                        initial_err,
                        recommended_gh_login_command(),
                        retry_err
                    )
                })?
            } else {
                return Err(format!(
                    "failed to export GitHub token: {}\nRun `{}` first and verify `gh auth token` works.",
                    initial_err,
                    recommended_gh_login_command()
                )
                .into());
            }
        }
    };

    let token = output.trim();
    if token.is_empty() {
        return Err("`gh auth token` returned an empty token.".into());
    }

    Ok(token.to_string())
}

fn run_gh_auth_token() -> Result<String, String> {
    run_cmd_output("gh", &["auth", "token"]).map_err(|err| err.to_string())
}

fn recommended_gh_login_command() -> String {
    format!("gh auth login -h {} -w -s {}", GH_HOST, GH_LOGIN_SCOPES)
}

fn gh_login_required(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("gh auth login")
        || lower.contains("not logged into any hosts")
        || lower.contains("authentication failed")
}

fn authenticated_login() -> Result<String, Box<dyn Error>> {
    let output = run_cmd_output("gh", &["api", "user", "--jq", ".login"])?;
    let login = output.trim();
    if login.is_empty() {
        return Err("`gh api user --jq .login` returned an empty login.".into());
    }
    Ok(login.to_string())
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

fn build_credentials_json(owner: &str, token: &str) -> Result<String, Box<dyn Error>> {
    serde_json::to_string(&json!({
        "owner": owner,
        "token": token,
    }))
    .map_err(|err| format!("failed to serialize GitHub credentials JSON: {}", err).into())
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
        "apiVersion: github.m.upbound.io/v1beta1\nkind: ProviderConfig\nmetadata:\n  name: {provider_config_name}\n  namespace: {namespace}\nspec:\n  credentials:\n    source: Secret\n    secretRef:\n      namespace: {namespace}\n      name: {secret_name}\n      key: credentials\n"
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
    fn select_owner_prefers_cli_then_envs() {
        assert_eq!(
            select_owner(Some("cli"), Some("env"), Some("github-env")),
            Some("cli".to_string())
        );
        assert_eq!(
            select_owner(None, Some("env"), Some("github-env")),
            Some("env".to_string())
        );
        assert_eq!(
            select_owner(None, None, Some("github-env")),
            Some("github-env".to_string())
        );
    }

    #[test]
    fn select_owner_ignores_blank_values() {
        assert_eq!(
            select_owner(Some("   "), Some(""), Some("  hops-ops  ")),
            Some("hops-ops".to_string())
        );
        assert_eq!(select_owner(Some(""), Some(" "), Some("")), None);
    }

    #[test]
    fn gh_login_required_detects_missing_login_errors() {
        assert!(gh_login_required(
            "gh exited with exit status: 4: You are not logged into any GitHub hosts. Run gh auth login."
        ));
        assert!(gh_login_required(
            "authentication failed; please run gh auth login"
        ));
        assert!(!gh_login_required("gh exited with exit status: 1: unknown api endpoint"));
    }

    #[test]
    fn credentials_json_contains_owner_and_token() {
        let json = build_credentials_json("hops-ops", "gho_123").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["owner"], "hops-ops");
        assert_eq!(value["token"], "gho_123");
    }

    #[test]
    fn provider_config_yaml_uses_secret_ref() {
        let yaml = build_provider_config_yaml("default", "default", "github-creds");
        assert!(yaml.contains("apiVersion: github.m.upbound.io/v1beta1"));
        assert!(yaml.contains("kind: ProviderConfig"));
        assert!(yaml.contains("name: github-creds"));
        assert!(yaml.contains("key: credentials"));
    }
}
