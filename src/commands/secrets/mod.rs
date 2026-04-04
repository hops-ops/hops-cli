mod decrypt;
mod encrypt;
mod init;
mod list;
mod sync;

use clap::{Args, Subcommand};
use rusoto_core::{HttpClient, Region};
use rusoto_credential::StaticProvider;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CONFIG_FILE: &str = ".hops.yaml";
const SOPS_FILE: &str = ".sops.yaml";
const SECRET_DIR: &str = "secrets";
const ENCRYPTED_SECRET_DIR: &str = "secrets-encrypted";
const DEFAULT_AWS_REGION: &str = "us-east-1";
const DEFAULT_AWS_SECRET_SUBDIR: &str = "aws";
const DEFAULT_GITHUB_SECRET_SUBDIR: &str = "github";
const DEFAULT_GITHUB_SHARED_SUBDIR: &str = "_shared";

#[derive(Args, Debug)]
pub struct SecretsArgs {
    #[command(subcommand)]
    pub command: SecretsCommands,
}

#[derive(Subcommand, Debug)]
pub enum SecretsCommands {
    /// Initialize repo secrets configuration (gitignore, tags, SOPS KMS key)
    Init(init::InitArgs),
    /// Encrypt files from secrets/ into secrets-encrypted/ using sops
    Encrypt(encrypt::EncryptArgs),
    /// Decrypt files from secrets-encrypted/ into secrets/ using sops
    Decrypt(decrypt::DecryptArgs),
    /// List local and remote secrets
    List,
    /// Sync secrets to AWS Secrets Manager
    Sync(sync::SyncArgs),
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RepoConfig {
    #[serde(default)]
    secrets: SecretsConfig,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct SecretsConfig {
    #[serde(default)]
    aws: AwsSecretsConfig,
    encrypted_dir: Option<String>,
    #[serde(default)]
    github: GithubSecretsConfig,
    plaintext_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct AwsSecretsConfig {
    path: Option<String>,
    region: Option<String>,
    tags: Option<HashMap<String, String>>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct GithubSecretsConfig {
    owner: Option<String>,
    path: Option<String>,
    #[serde(default)]
    shared_secrets: GithubSharedSecretsConfig,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct GithubSharedSecretsConfig {
    path: Option<String>,
    repos: Option<Vec<String>>,
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

pub fn run(args: &SecretsArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        SecretsCommands::Init(init_args) => init::run(init_args),
        SecretsCommands::Encrypt(encrypt_args) => encrypt::run(encrypt_args),
        SecretsCommands::Decrypt(decrypt_args) => decrypt::run(decrypt_args),
        SecretsCommands::List => list::run(),
        SecretsCommands::Sync(sync_args) => sync::run(sync_args),
    }
}

fn run_command_output(program: &str, args: &[&str]) -> Result<Vec<u8>, Box<dyn Error>> {
    log::debug!("Running: {} {}", program, args.join(" "));
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{} exited with {}: {}", program, output.status, stderr).into());
    }
    Ok(output.stdout)
}

fn run_command_output_string(program: &str, args: &[&str]) -> Result<String, Box<dyn Error>> {
    Ok(String::from_utf8(run_command_output(program, args)?)?)
}

fn require_command(program: &str) -> Result<(), Box<dyn Error>> {
    let status = Command::new("sh")
        .args(["-c", &format!("command -v {} >/dev/null 2>&1", program)])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Required command not found in PATH: {}", program).into())
    }
}

fn load_config() -> Result<RepoConfig, Box<dyn Error>> {
    let path = Path::new(CONFIG_FILE);
    if !path.exists() {
        return Ok(RepoConfig::default());
    }
    let content = fs::read_to_string(path)?;
    Ok(serde_yaml::from_str(&content)?)
}

fn save_config(config: &RepoConfig) -> Result<(), Box<dyn Error>> {
    let mut value = serde_yaml::to_value(config)?;
    sort_value(&mut value);
    fs::write(CONFIG_FILE, serde_yaml::to_string(&value)?)?;
    Ok(())
}

fn configured_aws_settings() -> Result<AwsSecretsRuntimeConfig, Box<dyn Error>> {
    let config = load_config()?;
    let tags = config
        .secrets
        .aws
        .tags
        .unwrap_or_default()
        .into_iter()
        .collect::<HashMap<_, _>>();
    let path = config
        .secrets
        .aws
        .path
        .unwrap_or_else(|| DEFAULT_AWS_SECRET_SUBDIR.to_string());
    let region = config
        .secrets
        .aws
        .region
        .unwrap_or_else(|| DEFAULT_AWS_REGION.to_string());
    Ok(AwsSecretsRuntimeConfig { path, region, tags })
}

fn configured_github_settings() -> Result<GithubSecretsRuntimeConfig, Box<dyn Error>> {
    let config = load_config()?;
    Ok(GithubSecretsRuntimeConfig {
        owner: config.secrets.github.owner,
        path: config
            .secrets
            .github
            .path
            .unwrap_or_else(|| DEFAULT_GITHUB_SECRET_SUBDIR.to_string()),
        shared_path: config
            .secrets
            .github
            .shared_secrets
            .path
            .unwrap_or_else(|| DEFAULT_GITHUB_SHARED_SUBDIR.to_string()),
        shared_repos: config
            .secrets
            .github
            .shared_secrets
            .repos
            .unwrap_or_default(),
    })
}

fn configured_secret_paths() -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let config = load_config()?;
    let plaintext = config
        .secrets
        .plaintext_dir
        .unwrap_or_else(|| SECRET_DIR.to_string());
    let encrypted = config
        .secrets
        .encrypted_dir
        .unwrap_or_else(|| ENCRYPTED_SECRET_DIR.to_string());
    Ok((PathBuf::from(plaintext), PathBuf::from(encrypted)))
}

#[derive(Debug, Clone)]
struct AwsSecretsRuntimeConfig {
    path: String,
    region: String,
    tags: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct GithubSecretsRuntimeConfig {
    owner: Option<String>,
    path: String,
    shared_path: String,
    shared_repos: Vec<String>,
}

fn selected_aws_profile() -> Option<String> {
    [
        std::env::var("AWS_PROFILE").ok(),
        std::env::var("AWS_DEFAULT_PROFILE").ok(),
    ]
    .into_iter()
    .flatten()
    .map(|value| value.trim().to_string())
    .find(|value| !value.is_empty())
}

fn run_aws_export_credentials(profile: &str) -> Result<String, String> {
    run_command_output_string(
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

fn export_aws_credentials(profile: &str) -> Result<AwsExportCredentials, Box<dyn Error>> {
    let output = run_aws_export_credentials(profile).map_err(|initial_err| {
        format!(
            "failed to export AWS credentials for profile '{}': {}\nIf this is an SSO profile, run `aws sso login --profile {}` first.",
            profile, initial_err, profile
        )
    })?;

    let credentials: AwsExportCredentials = serde_json::from_str(&output).map_err(|err| {
        format!(
            "failed to parse credential JSON for profile '{}': {}",
            profile, err
        )
    })?;

    if credentials.access_key_id.trim().is_empty()
        || credentials.secret_access_key.trim().is_empty()
    {
        return Err(format!(
            "AWS profile '{}' returned empty access key or secret key",
            profile
        )
        .into());
    }

    Ok(credentials)
}

fn aws_clients(
    region: &str,
) -> Result<
    (
        rusoto_secretsmanager::SecretsManagerClient,
        rusoto_sts::StsClient,
    ),
    Box<dyn Error>,
> {
    let region = region
        .parse::<Region>()
        .map_err(|_| format!("unsupported AWS region '{}'", region))?;
    if let Some(profile) = selected_aws_profile() {
        require_command("aws")?;
        let credentials = export_aws_credentials(&profile)?;
        let provider = StaticProvider::new(
            credentials.access_key_id,
            credentials.secret_access_key,
            credentials.session_token,
            None,
        );

        let secrets_client = rusoto_secretsmanager::SecretsManagerClient::new_with(
            HttpClient::new()?,
            provider.clone(),
            region.clone(),
        );
        let sts_client = rusoto_sts::StsClient::new_with(HttpClient::new()?, provider, region);
        return Ok((secrets_client, sts_client));
    }

    Ok((
        rusoto_secretsmanager::SecretsManagerClient::new(region.clone()),
        rusoto_sts::StsClient::new(region),
    ))
}

fn collect_local_secret_names(root: &Path) -> Vec<String> {
    if !root.exists() {
        return Vec::new();
    }

    let mut results = Vec::new();
    walk_local_secret_names(root, root, &mut results);
    results.sort();
    results
}

fn walk_local_secret_names(root: &Path, current: &Path, results: &mut Vec<String>) {
    let metadata = match fs::metadata(current) {
        Ok(metadata) => metadata,
        Err(err) => {
            log::warn!("Failed to inspect '{}': {}", current.display(), err);
            return;
        }
    };

    if metadata.is_dir() {
        let entries = match fs::read_dir(current) {
            Ok(entries) => entries,
            Err(err) => {
                log::warn!("Failed to read directory '{}': {}", current.display(), err);
                return;
            }
        };

        let mut has_env_files = false;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk_local_secret_names(root, &p, results);
            } else if p.is_file() {
                if p.extension().and_then(|e| e.to_str()) == Some("json") {
                    if let Some(name) = derive_secret_name(root, &p) {
                        results.push(name);
                    }
                } else {
                    has_env_files = true;
                }
            }
        }

        if has_env_files {
            if let Some(name) = derive_secret_name(root, current) {
                results.push(name);
            }
        }
        return;
    }

    if let Some(name) = derive_secret_name(root, current) {
        results.push(name);
    }
}

fn derive_secret_name(root: &Path, file_path: &Path) -> Option<String> {
    let relative = file_path.strip_prefix(root).ok()?;
    let mut components: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();

    let last = components.last_mut()?;
    if let Some(stripped) = last.strip_suffix(".json") {
        *last = stripped.to_string();
    }
    Some(components.join("/"))
}

fn mirror_tree_with_sops(
    source_root: &Path,
    dest_root: &Path,
    sops_mode: &str,
    force: bool,
) -> Result<(), Box<dyn Error>> {
    require_command("sops")?;

    if !source_root.exists() {
        return Err(format!("Source path does not exist: {}", source_root.display()).into());
    }

    fs::create_dir_all(dest_root)?;
    process_tree(source_root, source_root, dest_root, sops_mode, force)
}

fn process_tree(
    source_root: &Path,
    current: &Path,
    dest_root: &Path,
    sops_mode: &str,
    force: bool,
) -> Result<(), Box<dyn Error>> {
    if current.is_dir() {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            process_tree(source_root, &entry.path(), dest_root, sops_mode, force)?;
        }
        return Ok(());
    }

    if !current.is_file() {
        return Ok(());
    }

    let relative = current.strip_prefix(source_root)?;
    let destination = dest_root.join(relative);
    if destination.exists() && !force {
        return Err(format!(
            "Destination file already exists: {} (rerun with --force to overwrite)",
            destination.display()
        )
        .into());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let source = current
        .to_str()
        .ok_or_else(|| format!("Non-UTF8 path not supported: {}", current.display()))?;
    let output = match sops_mode {
        "encrypt" => run_command_output(
            "sops",
            &["--encrypt", "--input-type=raw", "--output-type=raw", source],
        )?,
        "decrypt" => run_command_output(
            "sops",
            &["--decrypt", "--input-type=raw", "--output-type=raw", source],
        )?,
        _ => return Err(format!("Unsupported sops mode: {}", sops_mode).into()),
    };

    fs::write(&destination, output)?;
    log::info!("Wrote {}", destination.display());
    Ok(())
}

fn sort_value(value: &mut Value) {
    if let Some(mapping) = value.as_mapping_mut() {
        let mut entries: Vec<_> = mapping
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        entries.sort_by(|(left, _), (right, _)| {
            left.as_str()
                .unwrap_or("")
                .cmp(right.as_str().unwrap_or(""))
        });
        mapping.clear();
        for (key, mut value) in entries {
            sort_value(&mut value);
            mapping.insert(key, value);
        }
        return;
    }

    if let Some(sequence) = value.as_sequence_mut() {
        for entry in sequence {
            sort_value(entry);
        }
    }
}

fn default_secret_paths() -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    configured_secret_paths()
}

#[cfg(test)]
mod tests {
    use super::{derive_secret_name, sort_value};
    use serde_yaml::Value;
    use std::path::Path;

    #[test]
    fn derive_secret_name_trims_root_and_json() {
        let name = derive_secret_name(
            Path::new("secrets"),
            Path::new("secrets/examples/example.json"),
        );
        assert_eq!(name.as_deref(), Some("examples/example"));
    }

    #[test]
    fn derive_secret_name_env_dir() {
        let name = derive_secret_name(Path::new("secrets"), Path::new("secrets/github"));
        assert_eq!(name.as_deref(), Some("github"));
    }

    #[test]
    fn sort_value_orders_mapping_keys() {
        let mut value: Value = serde_yaml::from_str("b: 2\na: 1\n").expect("yaml");
        sort_value(&mut value);
        let rendered = serde_yaml::to_string(&value).expect("yaml");
        assert!(rendered.find("a: 1").unwrap() < rendered.find("b: 2").unwrap());
    }
}
