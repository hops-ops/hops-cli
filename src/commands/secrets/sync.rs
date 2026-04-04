use super::{
    aws_clients, collect_local_secret_names, configured_aws_settings, configured_github_settings,
    configured_secret_paths, derive_secret_name, require_command, run_command_output_string,
};
use clap::{Args, Subcommand};
use dialoguer::Confirm;
use rusoto_secretsmanager::{
    CreateSecretRequest, DeleteSecretRequest, Filter, GetSecretValueRequest, ListSecretsRequest,
    PutSecretValueRequest, SecretsManager, SecretsManagerClient, Tag, TagResourceRequest,
};
use rusoto_sts::{GetCallerIdentityRequest, Sts};
use serde_json::Value as JsonValue;
use std::collections::{HashMap, HashSet};
use std::env;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use uuid::Uuid;

#[derive(Args, Debug)]
pub struct SyncArgs {
    #[command(subcommand)]
    pub target: SyncTarget,
}

#[derive(Subcommand, Debug)]
pub enum SyncTarget {
    /// Sync secrets to AWS Secrets Manager
    Aws(AwsSyncArgs),
    /// Sync secrets to GitHub repository secrets
    Github(GithubSyncArgs),
}

#[derive(Args, Debug)]
pub struct AwsSyncArgs {
    /// Secret path to sync, either a directory or a single file
    #[arg(long)]
    pub secret_path: Option<String>,

    /// Tags to apply in key=value form
    #[arg(long, value_parser = parse_key_value)]
    pub tags: Vec<(String, String)>,

    /// Skip confirmation prompts
    #[arg(short, long)]
    pub yes: bool,

    /// Check for remote repo-owned secrets that no longer exist locally and delete them
    #[arg(long)]
    pub cleanup: bool,

    /// Only update tags on existing remote secrets; skip value create/update
    #[arg(long)]
    pub tags_only: bool,
}

#[derive(Args, Debug)]
pub struct GithubSyncArgs {
    /// Secret path to sync. Defaults to <plaintext_dir>/<github.path>
    #[arg(long)]
    pub secret_path: Option<String>,

    /// Override configured repositories. Repeat to target multiple repos.
    #[arg(long = "repo")]
    pub repos: Vec<String>,

    /// Override configured GitHub owner or organization
    #[arg(long)]
    pub owner: Option<String>,

    /// Skip confirmation prompts
    #[arg(short, long)]
    pub yes: bool,
}

pub fn run(args: &SyncArgs) -> Result<(), Box<dyn Error>> {
    match &args.target {
        SyncTarget::Aws(aws_args) => run_aws(aws_args),
        SyncTarget::Github(github_args) => run_github(github_args),
    }
}

fn run_aws(args: &AwsSyncArgs) -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let aws_settings = configured_aws_settings()?;
    let (plaintext_dir, _) = configured_secret_paths()?;
    let default_source = plaintext_dir.join(&aws_settings.path);
    let naming_root = default_source.clone();
    let secret_source = args
        .secret_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or(default_source);
    fs::metadata(&secret_source)?;

    if args.cleanup
        && normalized_absolute_path(&secret_source)? != normalized_absolute_path(&naming_root)?
    {
        return Err(
            "--cleanup can only be used when syncing the full configured AWS secrets root".into(),
        );
    }

    let (client, sts_client) = aws_clients(&aws_settings.region)?;

    let mut final_tags_map = if args.tags.is_empty() {
        aws_settings.tags
    } else {
        let mut tags = HashMap::new();
        for (key, value) in &args.tags {
            tags.insert(key.clone(), value.clone());
        }
        tags
    };
    final_tags_map.insert("hops.ops.com.ai/secret".to_string(), "true".to_string());
    let mut final_tags = final_tags_map.into_iter().collect::<Vec<_>>();
    final_tags.sort();

    confirm_target_account(&runtime, &sts_client, args.yes)?;

    let mut synced = 0usize;
    process_aws_path(
        &runtime,
        &client,
        &final_tags,
        &naming_root,
        &secret_source,
        &mut synced,
        args.yes,
        args.tags_only,
    );

    if args.cleanup {
        let local_names = collect_local_secret_names(&naming_root);
        delete_missing_secrets(&runtime, &client, &local_names, args.yes);
    }

    log::info!("AWS sync complete - {} secrets processed", synced);
    Ok(())
}

fn normalized_absolute_path(path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };

    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }

    Ok(normalized)
}

fn process_aws_path(
    runtime: &tokio::runtime::Runtime,
    client: &SecretsManagerClient,
    tags: &[(String, String)],
    root: &Path,
    path: &Path,
    synced: &mut usize,
    yes: bool,
    tags_only: bool,
) {
    if path.is_dir() {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(err) => {
                log::warn!("Failed to read directory '{}': {}", path.display(), err);
                return;
            }
        };

        let mut subdirs = Vec::new();
        let mut json_files = Vec::new();
        let mut env_files = Vec::new();

        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                subdirs.push(p);
            } else if p.is_file() {
                if p.extension().and_then(|e| e.to_str()) == Some("json") {
                    json_files.push(p);
                } else {
                    env_files.push(p);
                }
            }
        }

        for dir in subdirs {
            process_aws_path(runtime, client, tags, root, &dir, synced, yes, tags_only);
        }

        for file in &json_files {
            let secret_string = match fs::read_to_string(file) {
                Ok(contents) => contents,
                Err(err) => {
                    log::error!("Failed reading {}: {}", file.display(), err);
                    continue;
                }
            };
            let Some(secret_name) = derive_secret_name(root, file) else {
                log::warn!("Could not derive secret name for {}", file.display());
                continue;
            };
            sync_aws_secret(
                runtime,
                client,
                tags,
                &secret_name,
                &secret_string,
                &file.display().to_string(),
                synced,
                yes,
                tags_only,
            );
        }

        if !env_files.is_empty() {
            let mut map = serde_json::Map::new();
            for file in &env_files {
                match fs::read_to_string(file) {
                    Ok(contents) => {
                        if is_dotenv_file(file) {
                            match parse_dotenv_secret_map(&contents) {
                                Ok(values) => map.extend(values),
                                Err(err) => {
                                    log::error!(
                                        "Failed parsing dotenv file {}: {}",
                                        file.display(),
                                        err
                                    );
                                }
                            }
                        } else {
                            let key = file
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("value")
                                .to_string();
                            map.insert(key, JsonValue::String(contents.trim().to_string()));
                        }
                    }
                    Err(err) => {
                        log::error!("Failed reading {}: {}", file.display(), err);
                    }
                }
            }

            if !map.is_empty() {
                let secret_string = JsonValue::Object(map).to_string();
                let Some(secret_name) = derive_secret_name(root, path) else {
                    log::warn!("Could not derive secret name for {}", path.display());
                    return;
                };
                sync_aws_secret(
                    runtime,
                    client,
                    tags,
                    &secret_name,
                    &secret_string,
                    &path.display().to_string(),
                    synced,
                    yes,
                    tags_only,
                );
            }
        }

        return;
    }

    if !path.is_file() {
        return;
    }

    let secret_string = match fs::read_to_string(path) {
        Ok(contents) => {
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                contents
            } else if is_dotenv_file(path) {
                match parse_dotenv_secret_map(&contents) {
                    Ok(values) => JsonValue::Object(values).to_string(),
                    Err(err) => {
                        log::error!("Failed parsing dotenv file {}: {}", path.display(), err);
                        return;
                    }
                }
            } else {
                let key = path.file_name().and_then(|n| n.to_str()).unwrap_or("value");
                serde_json::json!({ key: contents.trim() }).to_string()
            }
        }
        Err(err) => {
            log::error!("Failed reading {}: {}", path.display(), err);
            return;
        }
    };
    let Some(secret_name) = derive_secret_name(root, path) else {
        log::warn!("Could not derive secret name for {}", path.display());
        return;
    };
    sync_aws_secret(
        runtime,
        client,
        tags,
        &secret_name,
        &secret_string,
        &path.display().to_string(),
        synced,
        yes,
        tags_only,
    );
}

fn sync_aws_secret(
    runtime: &tokio::runtime::Runtime,
    client: &SecretsManagerClient,
    tags: &[(String, String)],
    secret_name: &str,
    secret_string: &str,
    source_label: &str,
    synced: &mut usize,
    yes: bool,
    tags_only: bool,
) {
    let exists = remote_secret_exists(runtime, client, secret_name);
    if tags_only {
        if !exists {
            log::info!(
                "Skipping {} because it does not exist remotely",
                secret_name
            );
            return;
        }
        if !check_tags_need_update(runtime, client, secret_name, tags) {
            log::info!("Secret {} tags already up to date", secret_name);
            return;
        }
        if !yes && !confirm(&format!("Update tags for secret '{}'?", secret_name), true) {
            return;
        }
        if let Err(err) = apply_tags(runtime, client, secret_name, tags) {
            log::error!("Failed applying tags to {}: {}", secret_name, err);
            return;
        }
        *synced += 1;
        return;
    }

    let value_unchanged = if exists {
        get_remote_secret_string(runtime, client, secret_name)
            .map(|value| value == secret_string)
            .unwrap_or(false)
    } else {
        false
    };
    let tags_need_update = exists && check_tags_need_update(runtime, client, secret_name, tags);

    if value_unchanged && !tags_need_update {
        log::info!("Secret {} unchanged; skipping", secret_name);
        return;
    }

    let action = if !exists {
        "create"
    } else if value_unchanged {
        "update tags for"
    } else {
        "update"
    };
    if !yes
        && !confirm(
            &format!(
                "{} secret '{}' from '{}'?",
                action, secret_name, source_label
            ),
            true,
        )
    {
        return;
    }

    let client_request_token = Uuid::new_v4().to_string();
    if !exists {
        let request = CreateSecretRequest {
            name: secret_name.to_string(),
            secret_string: Some(secret_string.to_string()),
            client_request_token: Some(client_request_token),
            tags: Some(
                tags.iter()
                    .map(|(key, value)| Tag {
                        key: Some(key.clone()),
                        value: Some(value.clone()),
                    })
                    .collect(),
            ),
            ..Default::default()
        };
        if let Err(err) = runtime.block_on(client.create_secret(request)) {
            log::error!("Failed to create {}: {}", secret_name, err);
            return;
        }
    } else if !value_unchanged {
        let request = PutSecretValueRequest {
            secret_id: secret_name.to_string(),
            secret_string: Some(secret_string.to_string()),
            client_request_token: Some(client_request_token),
            ..Default::default()
        };
        if let Err(err) = runtime.block_on(client.put_secret_value(request)) {
            log::error!("Failed to update {}: {}", secret_name, err);
            return;
        }
    }

    if let Err(err) = apply_tags(runtime, client, secret_name, tags) {
        log::error!("Failed applying tags to {}: {}", secret_name, err);
        return;
    }
    *synced += 1;
}

fn run_github(args: &GithubSyncArgs) -> Result<(), Box<dyn Error>> {
    require_command("gh")?;
    ensure_gh_auth()?;

    let github_settings = configured_github_settings()?;
    let (plaintext_dir, _) = configured_secret_paths()?;
    let default_source = plaintext_dir.join(&github_settings.path);
    let source_root = args
        .secret_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or(default_source);
    fs::metadata(&source_root)?;

    let owner = resolve_github_owner(args.owner.as_deref(), github_settings.owner.as_deref())?;
    let repos = resolve_github_repos(&source_root, &github_settings, &args.repos)?;
    if repos.is_empty() {
        return Err("No GitHub repos configured. Add secrets.github.shared_secrets.repos, pass --repo, or create repo directories under the GitHub secrets path.".into());
    }

    let shared_root = source_root.join(&github_settings.shared_path);
    let shared_secrets = collect_github_target_secrets(&shared_root)?;

    let mut synced = 0usize;
    for repo in repos {
        sync_github_repo(
            &owner,
            &repo,
            &source_root,
            &shared_root,
            &shared_secrets,
            args.yes,
            &mut synced,
        )?;
    }

    log::info!("GitHub sync complete - {} secrets processed", synced);
    Ok(())
}

fn ensure_gh_auth() -> Result<(), Box<dyn Error>> {
    let token = run_command_output_string("gh", &["auth", "token"]).map_err(|err| {
        format!(
            "failed to read GitHub auth token: {}\nRun `gh auth login` first.",
            err
        )
    })?;
    if token.trim().is_empty() {
        return Err("`gh auth token` returned an empty token. Run `gh auth login`.".into());
    }
    Ok(())
}

fn resolve_github_owner(
    cli_owner: Option<&str>,
    configured_owner: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let env_owner = env::var("GH_OWNER").ok();
    let env_github_owner = env::var("GITHUB_OWNER").ok();
    let owner = [
        cli_owner,
        configured_owner,
        env_owner.as_deref(),
        env_github_owner.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|value| !value.is_empty())
    .map(str::to_string);

    match owner {
        Some(owner) => Ok(owner),
        None => Err("GitHub owner is not configured. Set secrets.github.owner, pass --owner, or set GH_OWNER/GITHUB_OWNER.".into()),
    }
}

fn resolve_github_repos(
    source_root: &Path,
    settings: &super::GithubSecretsRuntimeConfig,
    cli_repos: &[String],
) -> Result<Vec<String>, Box<dyn Error>> {
    if !cli_repos.is_empty() {
        return Ok(cli_repos.to_vec());
    }
    if !settings.shared_repos.is_empty() {
        return Ok(settings.shared_repos.clone());
    }

    let mut repos = Vec::new();
    for entry in fs::read_dir(source_root)? {
        let path = entry?.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
                if name == settings.shared_path {
                    continue;
                }
                repos.push(name.to_string());
            }
        } else if path.extension().and_then(|value| value.to_str()) == Some("json") {
            if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
                if stem == settings.shared_path {
                    continue;
                }
                repos.push(stem.to_string());
            }
        }
    }
    repos.sort();
    repos.dedup();
    Ok(repos)
}

fn sync_github_repo(
    owner: &str,
    repo: &str,
    source_root: &Path,
    shared_root: &Path,
    shared_secrets: &[(String, String, String)],
    yes: bool,
    synced: &mut usize,
) -> Result<(), Box<dyn Error>> {
    let repo_dir = source_root.join(repo);
    let repo_file = source_root.join(format!("{repo}.json"));
    let mut merged = std::collections::BTreeMap::<String, (String, String)>::new();

    for (secret_name, secret_value, source_label) in shared_secrets {
        merged.insert(
            secret_name.clone(),
            (secret_value.clone(), source_label.clone()),
        );
    }

    if repo_dir.is_dir() {
        for (secret_name, secret_value, source_label) in collect_github_target_secrets(&repo_dir)? {
            merged.insert(secret_name, (secret_value, source_label));
        }
    } else if repo_file.is_file() {
        for (secret_name, secret_value, source_label) in collect_github_target_secrets(&repo_file)?
        {
            merged.insert(secret_name, (secret_value, source_label));
        }
    } else if shared_root.exists() && !shared_secrets.is_empty() {
        log::info!(
            "Applying only shared GitHub secrets to '{}/{}' (no repo-specific secrets found).",
            owner,
            repo
        );
    } else {
        log::warn!(
            "No secret source found for GitHub repo '{}'. Expected '{}' or '{}'.",
            repo,
            repo_dir.display(),
            repo_file.display()
        );
    }

    for (secret_name, (secret_value, source_label)) in merged {
        set_github_secret(owner, repo, &secret_name, &secret_value, &source_label, yes)?;
        *synced += 1;
    }
    Ok(())
}

fn collect_github_target_secrets(
    target: &Path,
) -> Result<Vec<(String, String, String)>, Box<dyn Error>> {
    if !target.exists() {
        return Ok(Vec::new());
    }
    if target.is_file() {
        return collect_github_file_secrets(target, target);
    }

    let mut out = Vec::new();
    collect_github_dir_secrets(target, target, &mut out)?;
    Ok(out)
}

fn collect_github_dir_secrets(
    root: &Path,
    current: &Path,
    out: &mut Vec<(String, String, String)>,
) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(current)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_github_dir_secrets(root, &path, out)?;
        } else if path.is_file() {
            out.extend(collect_github_file_secrets(root, &path)?);
        }
    }
    Ok(())
}

fn collect_github_file_secrets(
    root: &Path,
    path: &Path,
) -> Result<Vec<(String, String, String)>, Box<dyn Error>> {
    let contents = fs::read_to_string(path)?;
    if path.extension().and_then(|value| value.to_str()) == Some("json") {
        let secrets = parse_github_secret_map(&contents, path)?;
        return Ok(secrets
            .into_iter()
            .map(|(name, value)| (name, value, path.display().to_string()))
            .collect());
    }
    if is_dotenv_file(path) {
        let secrets = parse_github_dotenv_secret_map(&contents, path)?;
        return Ok(secrets
            .into_iter()
            .map(|(name, value)| (name, value, path.display().to_string()))
            .collect());
    }

    let secret_name = github_secret_name(root, path)?;
    let secret_value = contents.trim().to_string();
    Ok(vec![(
        secret_name,
        secret_value,
        path.display().to_string(),
    )])
}

fn parse_github_secret_map(
    contents: &str,
    path: &Path,
) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let value: JsonValue = serde_json::from_str(contents)
        .map_err(|err| format!("Failed parsing JSON in {}: {}", path.display(), err))?;
    let object = value
        .as_object()
        .ok_or_else(|| format!("GitHub secret JSON must be an object: {}", path.display()))?;

    let mut secrets = Vec::new();
    for (key, value) in object {
        let secret_name = normalize_github_secret_name(key);
        let secret_value = value
            .as_str()
            .map(ToString::to_string)
            .unwrap_or_else(|| value.to_string());
        secrets.push((secret_name, secret_value));
    }
    Ok(secrets)
}

fn parse_github_dotenv_secret_map(
    contents: &str,
    path: &Path,
) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let values = parse_dotenv_secret_map(contents)
        .map_err(|err| format!("Failed parsing dotenv file {}: {}", path.display(), err))?;

    let mut secrets = Vec::new();
    for (key, value) in values {
        let secret_name = normalize_github_secret_name(&key);
        let secret_value = value
            .as_str()
            .map(ToString::to_string)
            .unwrap_or_else(|| value.to_string());
        secrets.push((secret_name, secret_value));
    }
    Ok(secrets)
}

fn github_secret_name(repo_root: &Path, path: &Path) -> Result<String, Box<dyn Error>> {
    let relative = path.strip_prefix(repo_root)?;
    let raw = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("__");
    Ok(normalize_github_secret_name(raw.trim_end_matches(".json")))
}

fn normalize_github_secret_name(value: &str) -> String {
    let mut out = String::new();
    let mut prev_underscore = false;
    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_uppercase()
        } else {
            '_'
        };
        if mapped == '_' {
            if !prev_underscore {
                out.push(mapped);
            }
            prev_underscore = true;
        } else {
            out.push(mapped);
            prev_underscore = false;
        }
    }
    out.trim_matches('_').to_string()
}

fn is_dotenv_file(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some(".env")
}

fn parse_dotenv_secret_map(contents: &str) -> Result<serde_json::Map<String, JsonValue>, String> {
    let mut map = serde_json::Map::new();

    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("invalid dotenv entry on line {}", index + 1));
        };

        let key = key.trim();
        if key.is_empty() {
            return Err(format!("empty dotenv key on line {}", index + 1));
        }

        let value = strip_matching_quotes(value.trim());
        map.insert(key.to_string(), JsonValue::String(value.to_string()));
    }

    Ok(map)
}

fn strip_matching_quotes(value: &str) -> &str {
    if value.len() >= 2 {
        let quoted = (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''));
        if quoted {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn set_github_secret(
    owner: &str,
    repo: &str,
    secret_name: &str,
    secret_value: &str,
    source_label: &str,
    yes: bool,
) -> Result<(), Box<dyn Error>> {
    if !yes
        && !confirm(
            &format!(
                "Set GitHub secret '{}' in '{}/{}' from '{}'?",
                secret_name, owner, repo, source_label
            ),
            true,
        )
    {
        return Ok(());
    }

    let mut child = Command::new("gh")
        .args([
            "secret",
            "set",
            secret_name,
            "--repo",
            &format!("{}/{}", owner, repo),
        ])
        .stdin(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(secret_value.as_bytes())?;
    } else {
        return Err("failed to open stdin for `gh secret set`".into());
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(format!("gh secret set exited with {}", status).into());
    }
    log::info!(
        "Set GitHub secret '{}' in '{}/{}'",
        secret_name,
        owner,
        repo
    );
    Ok(())
}

fn remote_secret_exists(
    runtime: &tokio::runtime::Runtime,
    client: &SecretsManagerClient,
    secret_name: &str,
) -> bool {
    match runtime.block_on(client.get_secret_value(GetSecretValueRequest {
        secret_id: secret_name.to_string(),
        ..Default::default()
    })) {
        Ok(_) => true,
        Err(err) => {
            let text = err.to_string();
            !(text.contains("ResourceNotFoundException")
                || text.contains("can't find the specified secret"))
                && {
                    log::error!("Failed to inspect {}: {}", secret_name, text);
                    false
                }
        }
    }
}

fn get_remote_secret_string(
    runtime: &tokio::runtime::Runtime,
    client: &SecretsManagerClient,
    secret_name: &str,
) -> Option<String> {
    runtime
        .block_on(client.get_secret_value(GetSecretValueRequest {
            secret_id: secret_name.to_string(),
            ..Default::default()
        }))
        .ok()
        .and_then(|response| response.secret_string)
}

fn check_tags_need_update(
    runtime: &tokio::runtime::Runtime,
    client: &SecretsManagerClient,
    secret_name: &str,
    tags: &[(String, String)],
) -> bool {
    let request = ListSecretsRequest {
        filters: Some(vec![Filter {
            key: Some("name".to_string()),
            values: Some(vec![secret_name.to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    match runtime.block_on(client.list_secrets(request)) {
        Ok(response) => {
            let current_tags = response
                .secret_list
                .unwrap_or_default()
                .into_iter()
                .find(|secret| secret.name.as_deref() == Some(secret_name))
                .and_then(|secret| secret.tags)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|tag| match (tag.key, tag.value) {
                    (Some(key), Some(value)) => Some((key, value)),
                    _ => None,
                })
                .collect::<Vec<_>>();

            tags.iter()
                .any(|expected| !current_tags.iter().any(|actual| actual == expected))
        }
        Err(err) => {
            log::warn!("Failed checking tags for {}: {}", secret_name, err);
            true
        }
    }
}

fn apply_tags(
    runtime: &tokio::runtime::Runtime,
    client: &SecretsManagerClient,
    secret_name: &str,
    tags: &[(String, String)],
) -> Result<(), Box<dyn Error>> {
    let request = TagResourceRequest {
        secret_id: secret_name.to_string(),
        tags: tags
            .iter()
            .map(|(key, value)| Tag {
                key: Some(key.clone()),
                value: Some(value.clone()),
            })
            .collect(),
    };

    runtime.block_on(client.tag_resource(request))?;
    Ok(())
}

fn delete_missing_secrets(
    runtime: &tokio::runtime::Runtime,
    client: &SecretsManagerClient,
    local_secrets: &[String],
    yes: bool,
) {
    let local_set = local_secrets.iter().cloned().collect::<HashSet<_>>();
    let mut next_token = None;

    loop {
        let response = match runtime.block_on(client.list_secrets(ListSecretsRequest {
            next_token: next_token.clone(),
            filters: Some(vec![Filter {
                key: Some("tag-key".to_string()),
                values: Some(vec!["hops.ops.com.ai/secret".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        })) {
            Ok(response) => response,
            Err(err) => {
                log::error!("Failed listing secrets for cleanup: {}", err);
                return;
            }
        };

        for secret in response.secret_list.unwrap_or_default() {
            if !has_managed_secret_tag(secret.tags.as_ref()) {
                continue;
            }
            let Some(secret_name) = secret.name else {
                continue;
            };
            if local_set.contains(&secret_name) {
                continue;
            }

            let should_delete = yes
                || confirm(
                    &format!(
                        "Delete remote secret '{}' because it no longer exists locally?",
                        secret_name
                    ),
                    false,
                );
            if !should_delete {
                continue;
            }

            if let Err(err) = runtime.block_on(client.delete_secret(DeleteSecretRequest {
                secret_id: secret_name.clone(),
                force_delete_without_recovery: Some(true),
                ..Default::default()
            })) {
                log::error!("Failed deleting {}: {}", secret_name, err);
            } else {
                log::info!("Deleted {}", secret_name);
            }
        }

        if let Some(token) = response.next_token {
            next_token = Some(token);
        } else {
            break;
        }
    }
}

fn has_managed_secret_tag(tags: Option<&Vec<rusoto_secretsmanager::Tag>>) -> bool {
    tags.into_iter().flatten().any(|tag| {
        tag.key.as_deref() == Some("hops.ops.com.ai/secret") && tag.value.as_deref() == Some("true")
    })
}

fn confirm_target_account(
    runtime: &tokio::runtime::Runtime,
    client: &rusoto_sts::StsClient,
    yes: bool,
) -> Result<(), Box<dyn Error>> {
    let profile = env::var("AWS_PROFILE")
        .or_else(|_| env::var("AWS_DEFAULT_PROFILE"))
        .unwrap_or_else(|_| "default".to_string());
    let response = runtime
        .block_on(client.get_caller_identity(GetCallerIdentityRequest::default()))
        .map_err(|err| format!("Failed to determine AWS account: {}", err))?;
    let account = response
        .account
        .ok_or("AWS account ID not available from STS GetCallerIdentity")?;

    if yes
        || confirm(
            &format!(
                "Continue syncing secrets with AWS profile '{}' targeting account '{}'?",
                profile, account
            ),
            true,
        )
    {
        Ok(())
    } else {
        Err("Secrets sync cancelled".into())
    }
}

fn confirm(prompt: &str, default: bool) -> bool {
    Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact()
        .unwrap_or(false)
}

fn parse_key_value(value: &str) -> Result<(String, String), String> {
    let mut parts = value.splitn(2, '=');
    let key = parts.next().ok_or("Empty key")?;
    let value = parts.next().ok_or("Missing value after '='")?;
    Ok((key.to_string(), value.to_string()))
}

#[cfg(test)]
mod tests {
    use crate::commands::secrets::derive_secret_name;
    use serde_json::{json, Value as JsonValue};
    use std::path::Path;

    use super::{
        normalize_github_secret_name, parse_dotenv_secret_map, parse_github_dotenv_secret_map,
    };

    #[test]
    fn derive_secret_name_from_json_path() {
        assert_eq!(
            derive_secret_name(
                Path::new("secrets"),
                Path::new("secrets/examples/example.json")
            )
            .as_deref(),
            Some("examples/example")
        );
    }

    #[test]
    fn derive_secret_name_from_env_dir() {
        assert_eq!(
            derive_secret_name(Path::new("secrets"), Path::new("secrets/github")).as_deref(),
            Some("github")
        );
    }

    #[test]
    fn normalize_github_secret_name_uppercases_and_flattens() {
        assert_eq!(normalize_github_secret_name("token"), "TOKEN");
        assert_eq!(
            normalize_github_secret_name("actions/npm-token"),
            "ACTIONS_NPM_TOKEN"
        );
        assert_eq!(
            normalize_github_secret_name("app__prod.database-url"),
            "APP_PROD_DATABASE_URL"
        );
    }

    #[test]
    fn parse_dotenv_secret_map_reads_key_values() {
        let parsed = parse_dotenv_secret_map("FOO=bar\nBAZ=qux\n").unwrap();
        assert_eq!(
            JsonValue::Object(parsed),
            json!({"FOO": "bar", "BAZ": "qux"})
        );
    }

    #[test]
    fn parse_dotenv_secret_map_skips_comments_and_export() {
        let parsed = parse_dotenv_secret_map("# comment\nexport FOO=\"bar\"\n\n").unwrap();
        assert_eq!(JsonValue::Object(parsed), json!({"FOO": "bar"}));
    }

    #[test]
    fn parse_github_dotenv_secret_map_expands_to_individual_secrets() {
        let parsed =
            parse_github_dotenv_secret_map("foo=bar\napp-token=baz\n", Path::new(".env")).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("APP_TOKEN".to_string(), "baz".to_string()),
                ("FOO".to_string(), "bar".to_string())
            ]
        );
    }
}
