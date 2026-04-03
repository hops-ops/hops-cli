use super::{kms_client, load_config, save_config, SecretsConfig, CONFIG_FILE, SOPS_FILE};
use clap::Args;
use dialoguer::{Input, Select};
use rusoto_kms::{DescribeKeyRequest, Kms, ListKeysRequest};
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::Path;

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Existing KMS ARN to use directly (skip interactive prompt)
    #[arg(long)]
    pub kms_arn: Option<String>,

    /// Additional default tags to store for secrets sync in key=value form
    #[arg(long, value_parser = parse_key_value)]
    pub tags: Vec<(String, String)>,
}

pub fn run(args: &InitArgs) -> Result<(), Box<dyn Error>> {
    update_gitignore()?;
    ensure_secret_tags(args)?;
    configure_kms(args)?;
    log::info!("Secrets initialization complete");
    Ok(())
}

fn configure_kms(args: &InitArgs) -> Result<(), Box<dyn Error>> {
    if let Some(arn) = args.kms_arn.as_ref() {
        write_sops_file(arn)?;
        return Ok(());
    }

    let arn = select_existing_kms_key()?;
    write_sops_file(&arn)?;
    Ok(())
}

fn select_existing_kms_key() -> Result<String, Box<dyn Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let client = kms_client()?;

    log::info!("Fetching KMS keys...");

    let mut keys = Vec::new();
    let mut marker = None;

    loop {
        let response = runtime.block_on(client.list_keys(ListKeysRequest {
            marker: marker.clone(),
            ..Default::default()
        }))?;

        for entry in response.keys.unwrap_or_default() {
            let key_id = entry.key_id.unwrap_or_default();
            let key_arn = entry.key_arn.unwrap_or_default();
            if key_id.is_empty() || key_arn.is_empty() {
                continue;
            }

            let description = runtime
                .block_on(client.describe_key(DescribeKeyRequest {
                    key_id: key_id.clone(),
                    ..Default::default()
                }))
                .ok()
                .and_then(|resp| resp.key_metadata)
                .and_then(|meta| meta.description)
                .unwrap_or_default();

            keys.push((key_arn, description));
        }

        if response.truncated.unwrap_or(false) {
            marker = response.next_marker;
        } else {
            break;
        }
    }

    if keys.is_empty() {
        log::warn!("No KMS keys found in this account/region");
        return Ok(Input::<String>::new()
            .with_prompt("Enter KMS ARN manually")
            .interact_text()?);
    }

    let labels: Vec<String> = keys
        .iter()
        .map(|(arn, desc)| {
            if desc.is_empty() {
                arn.clone()
            } else {
                format!("{} ({})", arn, desc)
            }
        })
        .collect();

    let mut items: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    items.push("Enter ARN manually");

    let selection = Select::new()
        .with_prompt("Select a KMS key")
        .items(&items)
        .interact()?;

    if selection == keys.len() {
        return Ok(Input::<String>::new()
            .with_prompt("KMS ARN")
            .interact_text()?);
    }

    Ok(keys[selection].0.clone())
}

fn write_sops_file(kms_arn: &str) -> Result<(), Box<dyn Error>> {
    let path = Path::new(SOPS_FILE);
    if path.exists() {
        log::warn!("{} already exists; leaving it unchanged", path.display());
        return Ok(());
    }

    let contents = format!("creation_rules:\n  - kms: \"{}\"\n", kms_arn);
    fs::write(path, contents)?;
    log::info!("Created {}", path.display());
    Ok(())
}

fn update_gitignore() -> Result<(), Box<dyn Error>> {
    let path = Path::new(".gitignore");
    let mut lines = if path.exists() {
        fs::read_to_string(path)?
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    for required in ["secrets/", ".tmp/", ".DS_Store"] {
        if !lines.iter().any(|line| line == required) {
            lines.push(required.to_string());
        }
    }

    fs::write(path, format!("{}\n", lines.join("\n")))?;
    log::info!("Updated {}", path.display());
    Ok(())
}

fn ensure_secret_tags(args: &InitArgs) -> Result<(), Box<dyn Error>> {
    let mut config = load_config()?;
    let mut tags = config.secrets.tags.clone().unwrap_or_else(HashMap::new);
    tags.insert("hops.ops.com.ai/managed".to_string(), "true".to_string());

    for (key, value) in &args.tags {
        tags.insert(key.clone(), value.clone());
    }

    if args.tags.is_empty() {
        loop {
            let key: String = Input::new()
                .with_prompt("Additional default tag key for secrets sync (leave blank to finish)")
                .allow_empty(true)
                .interact_text()?;

            let key = key.trim().to_string();
            if key.is_empty() {
                break;
            }

            let value: String = Input::new()
                .with_prompt(format!("Value for tag '{}'", key))
                .interact_text()?;
            tags.insert(key, value);
        }
    }

    config.secrets = SecretsConfig { tags: Some(tags) };
    save_config(&config)?;
    log::info!("Saved default secret tags to {}", CONFIG_FILE);
    Ok(())
}

fn parse_key_value(value: &str) -> Result<(String, String), String> {
    let mut parts = value.splitn(2, '=');
    let key = parts.next().ok_or("Empty key")?.trim();
    let value = parts.next().ok_or("Missing value after '='")?.trim();

    if key.is_empty() {
        return Err("Tag key cannot be empty".to_string());
    }

    Ok((key.to_string(), value.to_string()))
}
