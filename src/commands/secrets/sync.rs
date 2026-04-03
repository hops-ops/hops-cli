use super::{aws_clients, collect_local_secret_names, configured_tags, SECRET_DIR};
use clap::Args;
use dialoguer::Confirm;
use rusoto_secretsmanager::{
    CreateSecretRequest, DeleteSecretRequest, Filter, GetSecretValueRequest, ListSecretsRequest,
    PutSecretValueRequest, SecretsManager, SecretsManagerClient, Tag, TagResourceRequest,
};
use rusoto_sts::{GetCallerIdentityRequest, Sts};
use serde_json;
use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::fs;
use std::path::Path;
use uuid::Uuid;

#[derive(Args, Debug)]
pub struct SyncArgs {
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

pub fn run(args: &SyncArgs) -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let (client, sts_client) = aws_clients()?;

    let secret_source = args
        .secret_path
        .clone()
        .unwrap_or_else(|| SECRET_DIR.to_string());
    fs::metadata(&secret_source)?;

    let mut final_tags = if args.tags.is_empty() {
        configured_tags()?
    } else {
        args.tags.clone()
    };

    final_tags.push(("hops.ops.com.ai/managed".to_string(), "true".to_string()));
    final_tags.sort();
    final_tags.dedup();

    confirm_target_account(&runtime, &sts_client, args.yes)?;

    let mut synced = 0usize;
    let mut local_synced = Vec::new();
    process_path(
        &runtime,
        &client,
        &final_tags,
        Path::new(&secret_source),
        &mut synced,
        &mut local_synced,
        args.yes,
        args.tags_only,
    );

    if args.cleanup {
        let local_names = collect_local_secret_names(Path::new(SECRET_DIR));
        delete_missing_secrets(&runtime, &client, &local_names, args.yes);
    }

    log::info!("Sync complete - {} secrets processed", synced);
    Ok(())
}

fn process_path(
    runtime: &tokio::runtime::Runtime,
    client: &SecretsManagerClient,
    tags: &[(String, String)],
    path: &Path,
    synced: &mut usize,
    local_synced: &mut Vec<String>,
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
            process_path(runtime, client, tags, &dir, synced, local_synced, yes, tags_only);
        }

        for file in &json_files {
            let secret_string = match fs::read_to_string(file) {
                Ok(contents) => contents,
                Err(err) => {
                    log::error!("Failed reading {}: {}", file.display(), err);
                    continue;
                }
            };
            let Some(secret_name) = derive_secret_name(file) else {
                log::warn!("Could not derive secret name for {}", file.display());
                continue;
            };
            sync_secret(
                runtime,
                client,
                tags,
                &secret_name,
                &secret_string,
                &file.display().to_string(),
                synced,
                local_synced,
                yes,
                tags_only,
            );
        }

        if !env_files.is_empty() {
            let mut map = serde_json::Map::new();
            for file in &env_files {
                let key = file
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("value")
                    .to_string();
                match fs::read_to_string(file) {
                    Ok(contents) => {
                        map.insert(key, serde_json::Value::String(contents.trim().to_string()));
                    }
                    Err(err) => {
                        log::error!("Failed reading {}: {}", file.display(), err);
                    }
                }
            }

            if !map.is_empty() {
                let secret_string = serde_json::Value::Object(map).to_string();
                let Some(secret_name) = derive_secret_name(path) else {
                    log::warn!("Could not derive secret name for {}", path.display());
                    return;
                };
                sync_secret(
                    runtime,
                    client,
                    tags,
                    &secret_name,
                    &secret_string,
                    &path.display().to_string(),
                    synced,
                    local_synced,
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
            } else {
                let key = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("value");
                serde_json::json!({ key: contents.trim() }).to_string()
            }
        }
        Err(err) => {
            log::error!("Failed reading {}: {}", path.display(), err);
            return;
        }
    };
    let Some(secret_name) = derive_secret_name(path) else {
        log::warn!("Could not derive secret name for {}", path.display());
        return;
    };
    sync_secret(
        runtime,
        client,
        tags,
        &secret_name,
        &secret_string,
        &path.display().to_string(),
        synced,
        local_synced,
        yes,
        tags_only,
    );
}

fn sync_secret(
    runtime: &tokio::runtime::Runtime,
    client: &SecretsManagerClient,
    tags: &[(String, String)],
    secret_name: &str,
    secret_string: &str,
    source_label: &str,
    synced: &mut usize,
    local_synced: &mut Vec<String>,
    yes: bool,
    tags_only: bool,
) {
    local_synced.push(secret_name.to_string());

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
        apply_tags(runtime, client, secret_name, tags);
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
            &format!("{} secret '{}' from '{}'?", action, secret_name, source_label),
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

    apply_tags(runtime, client, secret_name, tags);
    *synced += 1;
}

fn derive_secret_name(path: &Path) -> Option<String> {
    let mut secret_path = path.to_string_lossy().to_string();
    secret_path = secret_path.trim_end_matches(".json").to_string();
    if let Some(stripped) = secret_path.strip_prefix("./secrets/") {
        return Some(stripped.to_string());
    }
    if let Some(stripped) = secret_path.strip_prefix("secrets/") {
        return Some(stripped.to_string());
    }
    if let Some(stripped) = secret_path.strip_prefix("secrets\\") {
        return Some(stripped.replace('\\', "/"));
    }
    Some(secret_path)
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
) {
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

    if let Err(err) = runtime.block_on(client.tag_resource(request)) {
        log::warn!("Failed applying tags to {}: {}", secret_name, err);
    }
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
                values: Some(vec!["hops.ops.com.ai/managed".to_string()]),
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

fn confirm_target_account(
    runtime: &tokio::runtime::Runtime,
    client: &rusoto_sts::StsClient,
    yes: bool,
) -> Result<(), Box<dyn Error>> {
    let profile = env::var("AWS_PROFILE")
        .or_else(|_| env::var("AWS_DEFAULT_PROFILE"))
        .unwrap_or_else(|_| "default".to_string());
    let account = runtime
        .block_on(client.get_caller_identity(GetCallerIdentityRequest::default()))
        .ok()
        .and_then(|response| response.account)
        .unwrap_or_else(|| "unknown".to_string());

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
    use super::derive_secret_name;
    use std::path::Path;

    #[test]
    fn derive_secret_name_from_json_path() {
        assert_eq!(
            derive_secret_name(Path::new("secrets/examples/example.json")).as_deref(),
            Some("examples/example")
        );
    }

    #[test]
    fn derive_secret_name_from_env_dir() {
        assert_eq!(
            derive_secret_name(Path::new("secrets/github")).as_deref(),
            Some("github")
        );
    }
}
