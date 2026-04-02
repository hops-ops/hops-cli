use super::{ensure_parent_dir, load_config, save_config, SecretsConfig, CONFIG_FILE, SOPS_FILE};
use crate::commands::local::{kubectl_apply_stdin, run_cmd_output};
use clap::Args;
use dialoguer::{Input, Select};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Existing KMS ARN to place into .sops.yaml creation_rules
    #[arg(long, conflicts_with = "create_kms")]
    pub kms_arn: Option<String>,

    /// Create the KMS key through the connected cluster's AWS provider
    #[arg(long)]
    pub create_kms: bool,

    /// AWS provider config name for the Key resource
    #[arg(long, default_value = "default")]
    pub provider_config_name: String,

    /// providerConfigRef.kind for the Key resource; defaults to namespaced ProviderConfig
    #[arg(long, default_value = "ProviderConfig")]
    pub provider_config_kind: String,

    /// Namespace for the Key resource when the CRD is namespaced
    #[arg(long, default_value = "default")]
    pub namespace: String,

    /// AWS region for a newly created KMS key
    #[arg(long)]
    pub region: Option<String>,

    /// Kubernetes metadata.name for the KMS Key resource
    #[arg(long)]
    pub key_name: Option<String>,

    /// Additional default tags to store for secrets sync in key=value form
    #[arg(long, value_parser = parse_key_value)]
    pub tags: Vec<(String, String)>,
}

pub fn run(args: &InitArgs) -> Result<(), Box<dyn Error>> {
    let kms_arn = resolve_kms_arn(args)?;
    write_sops_file(&kms_arn)?;
    update_gitignore()?;
    create_example_secret()?;
    ensure_secret_tags(args)?;
    log::info!("Secrets initialization complete");
    Ok(())
}

fn resolve_kms_arn(args: &InitArgs) -> Result<String, Box<dyn Error>> {
    if let Some(kms_arn) = args.kms_arn.as_ref() {
        return Ok(kms_arn.clone());
    }

    if args.create_kms {
        return create_kms_key(args);
    }

    let selection = Select::new()
        .with_prompt("How should hops configure the SOPS KMS key?")
        .items([
            "Use an existing AWS KMS key ARN",
            "Create a new KMS key with kubectl apply via the AWS provider",
        ])
        .default(1)
        .interact_opt()?;

    match selection {
        Some(0) => Ok(Input::<String>::new()
            .with_prompt("AWS KMS ARN for SOPS creation_rules")
            .interact_text()?),
        Some(1) => create_kms_key(args),
        _ => Err("No KMS key option selected".into()),
    }
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

    for required in ["secret/", ".tmp/", ".DS_Store"] {
        if !lines.iter().any(|line| line == required) {
            lines.push(required.to_string());
        }
    }

    fs::write(path, format!("{}\n", lines.join("\n")))?;
    log::info!("Updated {}", path.display());
    Ok(())
}

fn create_example_secret() -> Result<(), Box<dyn Error>> {
    let path = Path::new("secret/devops/example.json");
    if path.exists() {
        log::warn!("{} already exists; leaving it unchanged", path.display());
        return Ok(());
    }

    ensure_parent_dir(path)?;
    fs::write(path, "{\n  \"foo\": \"bar\"\n}\n")?;
    log::info!("Created {}", path.display());
    Ok(())
}

fn ensure_secret_tags(args: &InitArgs) -> Result<(), Box<dyn Error>> {
    let mut config = load_config()?;
    let mut tags = config.secrets.tags.clone().unwrap_or_else(HashMap::new);
    tags.insert("hops.ops.com.ai/cli".to_string(), "true".to_string());

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

#[derive(Clone, Debug)]
struct KeyResource {
    api_version: String,
    resource_name: String,
    namespaced: bool,
}

fn create_kms_key(args: &InitArgs) -> Result<String, Box<dyn Error>> {
    let key_resource = discover_key_resource()?;
    let region = resolve_region(args)?;
    let key_name = match args.key_name.clone() {
        Some(key_name) => key_name,
        None => Input::<String>::new()
            .with_prompt("Kubernetes name for the SOPS KMS key resource")
            .default(default_key_name())
            .interact_text()?,
    };

    log::info!(
        "Applying KMS Key '{}' in region '{}' using {}",
        key_name,
        region,
        key_resource.api_version
    );

    let manifest = render_key_manifest(args, &key_resource, &key_name, &region);
    kubectl_apply_stdin(&manifest)?;
    wait_for_key_arn(args, &key_resource, &key_name)
}

fn discover_key_resource() -> Result<KeyResource, Box<dyn Error>> {
    let output = run_cmd_output("kubectl", &["get", "crd", "-o", "json"])?;
    let value: JsonValue = serde_json::from_str(&output)?;
    let items = value
        .get("items")
        .and_then(JsonValue::as_array)
        .ok_or("kubectl get crd returned no CRD items")?;

    for item in items {
        let spec = item.get("spec").ok_or("CRD item missing spec")?;
        let group = spec
            .get("group")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        let kind = spec
            .get("names")
            .and_then(|names| names.get("kind"))
            .and_then(JsonValue::as_str);
        if kind != Some("Key") || !group.starts_with("kms.aws.") {
            continue;
        }

        let version = spec
            .get("versions")
            .and_then(JsonValue::as_array)
            .and_then(|versions| {
                versions.iter().find_map(|version| {
                    let served = version
                        .get("served")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false);
                    let storage = version
                        .get("storage")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false);
                    if served && storage {
                        version.get("name").and_then(JsonValue::as_str)
                    } else {
                        None
                    }
                })
            })
            .or_else(|| {
                spec.get("versions")
                    .and_then(JsonValue::as_array)
                    .and_then(|versions| versions.first())
                    .and_then(|version| version.get("name"))
                    .and_then(JsonValue::as_str)
            })
            .ok_or("Unable to determine KMS Key CRD version")?;

        let plural = spec
            .get("names")
            .and_then(|names| names.get("plural"))
            .and_then(JsonValue::as_str)
            .ok_or("Unable to determine KMS Key CRD plural")?;

        let namespaced = spec
            .get("scope")
            .and_then(JsonValue::as_str)
            .map(|scope| scope.eq_ignore_ascii_case("Namespaced"))
            .unwrap_or(false);

        return Ok(KeyResource {
            api_version: format!("{}/{}", group, version),
            resource_name: format!("{}.{}", plural, group),
            namespaced,
        });
    }

    Err("Could not find a KMS Key CRD from the AWS provider in the connected cluster".into())
}

fn resolve_region(args: &InitArgs) -> Result<String, Box<dyn Error>> {
    if let Some(region) = args.region.as_deref() {
        let region = region.trim();
        if !region.is_empty() {
            return Ok(region.to_string());
        }
    }

    for env_name in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Ok(value) = std::env::var(env_name) {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }

    Ok(Input::<String>::new()
        .with_prompt("AWS region for the SOPS KMS key")
        .interact_text()?)
}

fn default_key_name() -> String {
    let repo = super::repo_name().unwrap_or_else(|_| "hops".to_string());
    let mut name = repo
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();

    while name.contains("--") {
        name = name.replace("--", "-");
    }
    name = name.trim_matches('-').to_string();

    let mut key_name = format!("{}-sops", name);
    if key_name.len() > 63 {
        key_name.truncate(63);
        key_name = key_name.trim_matches('-').to_string();
    }

    if key_name.is_empty() {
        "hops-sops".to_string()
    } else {
        key_name
    }
}

fn render_key_manifest(
    args: &InitArgs,
    key_resource: &KeyResource,
    key_name: &str,
    region: &str,
) -> String {
    let namespace = if key_resource.namespaced {
        format!("  namespace: {}\n", args.namespace)
    } else {
        String::new()
    };

    format!(
        "apiVersion: {api_version}\nkind: Key\nmetadata:\n  name: {key_name}\n{namespace}spec:\n  managementPolicies:\n    - Observe\n    - Create\n    - Update\n    - LateInitialize\n  providerConfigRef:\n    name: {provider_config_name}\n    kind: {provider_config_kind}\n  forProvider:\n    region: {region}\n    description: SOPS key for hops secrets\n    enableKeyRotation: true\n",
        api_version = key_resource.api_version,
        key_name = key_name,
        namespace = namespace,
        provider_config_name = args.provider_config_name,
        provider_config_kind = args.provider_config_kind,
        region = region,
    )
}

fn wait_for_key_arn(
    args: &InitArgs,
    key_resource: &KeyResource,
    key_name: &str,
) -> Result<String, Box<dyn Error>> {
    let start = Instant::now();
    let timeout = Duration::from_secs(300);

    loop {
        let mut command_args = vec!["get", key_resource.resource_name.as_str(), key_name];
        if key_resource.namespaced {
            command_args.push("-n");
            command_args.push(args.namespace.as_str());
        }
        command_args.push("-o");
        command_args.push("json");

        match run_cmd_output("kubectl", &command_args) {
            Ok(output) => {
                let value: JsonValue = serde_json::from_str(&output)?;
                if let Some(arn) = value
                    .get("status")
                    .and_then(|status| status.get("atProvider"))
                    .and_then(|at_provider| at_provider.get("arn"))
                    .and_then(JsonValue::as_str)
                    .filter(|arn| !arn.trim().is_empty())
                {
                    log::info!("Resolved KMS key ARN {}", arn);
                    return Ok(arn.to_string());
                }
            }
            Err(err) => {
                log::debug!("Waiting for KMS key ARN: {}", err);
            }
        }

        if start.elapsed() >= timeout {
            return Err(format!(
                "Timed out waiting for KMS key '{}' to report an ARN",
                key_name
            )
            .into());
        }

        thread::sleep(Duration::from_secs(5));
    }
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
