use super::{
    configured_aws_settings, configured_github_settings, configured_secret_paths, load_config,
    require_command, save_config, sort_value, CONFIG_FILE, SOPS_FILE,
};
use crate::commands::local::{kubectl_apply_stdin, run_cmd_output};
use clap::Args;
use dialoguer::{Input, Select};
use serde_json::Value as JsonValue;
use serde_yaml::{Mapping, Value};
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

const KMS_KEY_RESOURCE: &str = "key.kms.aws.m.upbound.io";
const DEFAULT_KMS_PROVIDER_CONFIG: &str = "default";
const DEFAULT_KMS_NAMESPACE: &str = "default";
const DEFAULT_KMS_WAIT_SECONDS: u64 = 300;

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Existing KMS ARN to use directly (skip interactive prompt)
    #[arg(long)]
    pub kms_arn: Option<String>,

    /// Create the KMS key via the connected control plane and wait for it to be ready
    #[arg(long, conflicts_with = "kms_arn")]
    pub create_kms: bool,

    /// Crossplane managed resource name to use when creating a KMS key
    #[arg(long)]
    pub kms_resource_name: Option<String>,

    /// Crossplane ProviderConfig name for control plane KMS key creation
    #[arg(long, default_value = DEFAULT_KMS_PROVIDER_CONFIG)]
    pub kms_provider_config: String,

    /// Namespace for the control plane-managed KMS key and ProviderConfig
    #[arg(long, default_value = DEFAULT_KMS_NAMESPACE)]
    pub kms_namespace: String,

    /// Optional description to set on a control plane-managed KMS key
    #[arg(long)]
    pub kms_description: Option<String>,

    /// AWS region for the control plane-managed KMS key
    #[arg(long)]
    pub kms_region: Option<String>,

    /// Seconds to wait for a control plane-managed KMS key to become ready
    #[arg(long, default_value_t = DEFAULT_KMS_WAIT_SECONDS)]
    pub kms_wait_seconds: u64,

    /// Create example secret inputs under secrets/
    #[arg(long, conflicts_with = "no_examples")]
    pub examples: bool,

    /// Skip creating example secret inputs
    #[arg(long)]
    pub no_examples: bool,

    /// Additional default tags to store for secrets sync in key=value form
    #[arg(long, value_parser = parse_key_value)]
    pub tags: Vec<(String, String)>,
}

pub fn run(args: &InitArgs) -> Result<(), Box<dyn Error>> {
    log::info!("Initializing secrets configuration...");
    configure_secret_paths()?;
    configure_target_paths()?;
    update_gitignore()?;
    ensure_secret_tags(args)?;
    configure_kms(args)?;
    maybe_create_examples(args)?;
    log::info!("Secrets initialization complete");
    Ok(())
}

fn configure_kms(args: &InitArgs) -> Result<(), Box<dyn Error>> {
    if let Some(arn) = args.kms_arn.as_ref() {
        write_sops_file(arn)?;
        return Ok(());
    }

    if args.create_kms {
        create_control_plane_kms_key(args)?;
        return Ok(());
    }

    if let Some(existing_kms) = existing_sops_kms_key()? {
        log::info!(
            "{} already exists and references this KMS key: {}",
            SOPS_FILE,
            existing_kms
        );
        if prompt_yes_no("Continue using the existing SOPS KMS key?", true)? {
            return Ok(());
        }
    }

    let arn = select_kms_configuration()?;
    match arn {
        KmsSelection::ExistingArn(arn) => write_sops_file(&arn)?,
        KmsSelection::CreateViaControlPlane => create_control_plane_kms_key(args)?,
    }
    Ok(())
}

enum KmsSelection {
    ExistingArn(String),
    CreateViaControlPlane,
}

fn select_kms_configuration() -> Result<KmsSelection, Box<dyn Error>> {
    let items = [
        "Create a new KMS key via the control plane",
        "Use an existing KMS key ARN",
    ];
    let selection = Select::new()
        .with_prompt("Choose how to configure SOPS KMS")
        .items(items)
        .default(0)
        .interact()?;

    match selection {
        0 => Ok(KmsSelection::CreateViaControlPlane),
        1 => Ok(KmsSelection::ExistingArn(prompt_for_kms_arn()?)),
        _ => Err("invalid KMS selection".into()),
    }
}

fn prompt_for_kms_arn() -> Result<String, Box<dyn Error>> {
    log::info!(
        "Paste an existing KMS key ARN. You can find it in the AWS console under KMS > Customer managed keys, or with `aws kms list-keys`."
    );
    let arn: String = Input::new().with_prompt("KMS key ARN").interact_text()?;
    let arn = arn.trim().to_string();
    if arn.is_empty() {
        return Err("KMS key ARN cannot be empty".into());
    }
    Ok(arn)
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

fn existing_sops_kms_key() -> Result<Option<String>, Box<dyn Error>> {
    let path = Path::new(SOPS_FILE);
    if !path.exists() {
        return Ok(None);
    }

    let value: Value = serde_yaml::from_str(&fs::read_to_string(path)?)?;
    let Some(rules) = value
        .as_mapping()
        .and_then(|root| root.get(vs("creation_rules")))
        .and_then(Value::as_sequence)
    else {
        return Ok(None);
    };

    for rule in rules {
        if let Some(kms) = rule
            .as_mapping()
            .and_then(|entry| entry.get(vs("kms")))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok(Some(kms.to_string()));
        }
    }

    Ok(None)
}

fn update_gitignore() -> Result<(), Box<dyn Error>> {
    let (plaintext_dir, _) = configured_secret_paths()?;
    let path = Path::new(".gitignore");
    let mut lines = if path.exists() {
        fs::read_to_string(path)?
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let plaintext_entry = normalize_gitignore_dir(&plaintext_dir);
    for required in [plaintext_entry.as_str(), ".tmp/", ".DS_Store"] {
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
    let mut tags = config.secrets.aws.tags.clone().unwrap_or_else(HashMap::new);
    tags.insert("hops.ops.com.ai/secret".to_string(), "true".to_string());

    for (key, value) in &args.tags {
        tags.insert(key.clone(), value.clone());
    }

    if args.tags.is_empty() && prompt_yes_no("Add tags to secrets when syncing to AWS?", false)? {
        loop {
            let key: String = Input::new()
                .with_prompt("Default tag key")
                .allow_empty(true)
                .interact_text()?;

            let key = key.trim().to_string();
            if key.is_empty() {
                break;
            }

            let value: String = Input::new()
                .with_prompt(format!("Default value for tag '{}'", key))
                .interact_text()?;
            tags.insert(key, value);
        }
    } else if args.tags.is_empty() {
        log::info!(
            "Skipping optional default sync tags. `hops secrets sync` will still apply hops.ops.com.ai/secret=true."
        );
    }

    config.secrets.aws.tags = Some(tags);
    save_config(&config)?;
    log::info!("Saved default secret tags to {}", CONFIG_FILE);
    Ok(())
}

fn configure_secret_paths() -> Result<(), Box<dyn Error>> {
    let mut config = load_config()?;
    let (current_plaintext, current_encrypted) = configured_secret_paths()?;

    let plaintext: String = Input::new()
        .with_prompt("Directory for unencrypted secrets")
        .default(current_plaintext.display().to_string())
        .interact_text()?;
    let encrypted: String = Input::new()
        .with_prompt("Directory for encrypted secrets")
        .default(current_encrypted.display().to_string())
        .interact_text()?;

    let plaintext = plaintext.trim().to_string();
    let encrypted = encrypted.trim().to_string();
    if plaintext.is_empty() || encrypted.is_empty() {
        return Err("Secret directories cannot be empty".into());
    }

    config.secrets.plaintext_dir = Some(plaintext);
    config.secrets.encrypted_dir = Some(encrypted);
    save_config(&config)?;
    log::info!("Saved secret directories to {}", CONFIG_FILE);
    Ok(())
}

fn configure_target_paths() -> Result<(), Box<dyn Error>> {
    let mut config = load_config()?;
    let aws = configured_aws_settings()?;
    let github = configured_github_settings()?;

    let aws_path: String = Input::new()
        .with_prompt("Subdirectory for AWS secrets")
        .default(aws.path)
        .interact_text()?;
    let aws_region: String = Input::new()
        .with_prompt("AWS region for Secrets Manager and KMS")
        .default(aws.region)
        .interact_text()?;
    let github_path: String = Input::new()
        .with_prompt("Subdirectory for GitHub secrets")
        .default(github.path)
        .interact_text()?;
    let github_shared_path: String = Input::new()
        .with_prompt("Subdirectory for shared GitHub secrets")
        .default(github.shared_path)
        .interact_text()?;
    let github_owner: String = Input::new()
        .with_prompt("GitHub owner or organization for repo secrets")
        .allow_empty(true)
        .default(github.owner.unwrap_or_default())
        .interact_text()?;
    let github_repos: String = Input::new()
        .with_prompt("Default GitHub repos for shared secrets (comma-separated, optional)")
        .allow_empty(true)
        .default(github.shared_repos.join(","))
        .interact_text()?;

    config.secrets.aws.path = Some(aws_path.trim().to_string());
    config.secrets.aws.region = Some(aws_region.trim().to_string());
    config.secrets.github.path = Some(github_path.trim().to_string());
    config.secrets.github.shared_secrets.path = Some(github_shared_path.trim().to_string());
    config.secrets.github.owner = non_empty(github_owner.trim());
    config.secrets.github.shared_secrets.repos = parse_csv(github_repos.trim());
    save_config(&config)?;
    log::info!("Saved secrets target settings to {}", CONFIG_FILE);
    Ok(())
}

fn maybe_create_examples(args: &InitArgs) -> Result<(), Box<dyn Error>> {
    let (plaintext_dir, _) = configured_secret_paths()?;
    let aws = configured_aws_settings()?;
    let github = configured_github_settings()?;
    let should_create = if args.examples {
        true
    } else if args.no_examples {
        false
    } else {
        prompt_yes_no(
            &format!(
                "Create example secret inputs under {}/?",
                plaintext_dir.display()
            ),
            false,
        )?
    };

    if !should_create {
        return Ok(());
    }

    create_example_secret_inputs(
        &plaintext_dir.join(&aws.path),
        &plaintext_dir.join(&github.path),
        &plaintext_dir.join(&github.path).join(&github.shared_path),
    )?;
    log::warn!(
        "Example secrets are real sync inputs. Remove or replace them before running `hops secrets sync` in a real repo."
    );
    log::info!("Secret input rollups:");
    log::info!(
        "  {}/examples/app.json -> AWS secret 'examples/app' (JSON object preserved as-is)",
        plaintext_dir.join(&aws.path).display()
    );
    log::info!(
        "  {}/examples/github/{{token,owner}} -> AWS secret 'examples/github' (directory rolls up into a JSON object keyed by filenames)",
        plaintext_dir.join(&aws.path).display()
    );
    log::info!(
        "  {}/sample-repo/NPM_TOKEN -> GitHub secret 'NPM_TOKEN' in repo 'sample-repo'",
        plaintext_dir.join(&github.path).display()
    );
    log::info!(
        "  GitHub secrets do not roll up into one JSON blob: each file is one secret, and JSON files expand into one secret per top-level key"
    );
    log::info!(
        "  {}/ORG_TOKEN -> shared GitHub secret synced to every targeted repo unless overridden per repo",
        plaintext_dir
            .join(&github.path)
            .join(&github.shared_path)
            .display()
    );
    log::info!(
        "  For GitHub shared secrets, target repos come from `secrets.github.shared_secrets.repos` or repeated `--repo` flags on `hops secrets sync github`"
    );
    Ok(())
}

fn create_example_secret_inputs(
    aws_root: &Path,
    github_root: &Path,
    github_shared_root: &Path,
) -> Result<(), Box<dyn Error>> {
    let json_example = aws_root.join("examples/app.json");
    let env_dir = aws_root.join("examples/github");
    let github_repo = github_root.join("sample-repo");

    write_example_file(
        &json_example,
        "{\n  \"DATABASE_URL\": \"postgres://app:change-me@example.internal:5432/app\",\n  \"API_KEY\": \"replace-me\"\n}\n",
    )?;
    write_example_file(&env_dir.join("token"), "ghp_replace_me\n")?;
    write_example_file(&env_dir.join("owner"), "hops-ops\n")?;
    write_example_file(&github_repo.join("NPM_TOKEN"), "npm_replace_me\n")?;
    write_example_file(
        &github_repo.join("actions.json"),
        "{\n  \"SLACK_WEBHOOK\": \"https://hooks.slack.com/services/replace/me\"\n}\n",
    )?;
    write_example_file(
        &github_shared_root.join("ORG_TOKEN"),
        "org_shared_replace_me\n",
    )?;

    Ok(())
}

fn write_example_file(path: &Path, contents: &str) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        log::info!(
            "Leaving existing example file unchanged: {}",
            path.display()
        );
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, contents)?;
    log::info!("Created {}", path.display());
    Ok(())
}

fn create_control_plane_kms_key(args: &InitArgs) -> Result<(), Box<dyn Error>> {
    require_command("kubectl")?;

    let resource_name = args
        .kms_resource_name
        .clone()
        .unwrap_or_else(default_kms_resource_name);
    let description = args
        .kms_description
        .clone()
        .unwrap_or_else(default_kms_description);
    let region = resolve_kms_region(args)?;

    let manifest = build_kms_key_manifest(
        &resource_name,
        &args.kms_namespace,
        &args.kms_provider_config,
        &region,
        &description,
        None,
    )?;
    let rendered = render_yaml(&manifest)?;

    log::info!(
        "Creating KMS key {} via control plane in region {}...",
        resource_name,
        region
    );
    kubectl_apply_stdin(&rendered)?;

    log::info!("Waiting for {} to become ready...", resource_name);
    let live = wait_for_kms_key_ready(&resource_name, &args.kms_namespace, args.kms_wait_seconds)?;

    let arn = live
        .get("status")
        .and_then(|status| status.get("atProvider"))
        .and_then(|provider| provider.get("arn"))
        .and_then(JsonValue::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or("KMS key became ready but status.atProvider.arn is missing")?;

    write_sops_file(arn)?;

    let external_name = resolved_kms_external_name(&live)
        .ok_or("KMS key became ready but external name could not be determined")?;
    let gitops_manifest = build_kms_key_manifest(
        &resource_name,
        &args.kms_namespace,
        &args.kms_provider_config,
        &region,
        &description,
        Some(&external_name),
    )?;

    println!("If you want to track this KMS key via GitOps, add the following:\n");
    print!("{}", render_yaml(&gitops_manifest)?);

    Ok(())
}

fn wait_for_kms_key_ready(
    name: &str,
    namespace: &str,
    wait_seconds: u64,
) -> Result<JsonValue, Box<dyn Error>> {
    let attempts = std::cmp::max(1, wait_seconds / 5);
    let mut last_summary = None;

    for _ in 0..attempts {
        let raw = run_cmd_output(
            "kubectl",
            &["get", KMS_KEY_RESOURCE, name, "-n", namespace, "-o", "json"],
        )?;
        let item: JsonValue = serde_json::from_str(&raw)?;

        if is_condition_true(&item, "Ready")
            && item
                .get("status")
                .and_then(|status| status.get("atProvider"))
                .and_then(|provider| provider.get("arn"))
                .and_then(JsonValue::as_str)
                .filter(|value| !value.trim().is_empty())
                .is_some()
        {
            return Ok(item);
        }

        last_summary = condition_summary(&item);
        thread::sleep(Duration::from_secs(5));
    }

    let suffix = last_summary
        .map(|summary| format!(" Last observed conditions: {summary}"))
        .unwrap_or_default();
    Err(format!(
        "Timed out waiting {} seconds for {} {} to become ready.{}",
        wait_seconds, KMS_KEY_RESOURCE, name, suffix
    )
    .into())
}

fn build_kms_key_manifest(
    name: &str,
    namespace: &str,
    provider_config: &str,
    region: &str,
    description: &str,
    external_name: Option<&str>,
) -> Result<Value, Box<dyn Error>> {
    let mut root = Mapping::new();
    root.insert(vs("apiVersion"), vs("kms.aws.m.upbound.io/v1beta1"));
    root.insert(vs("kind"), vs("Key"));

    let mut metadata = Mapping::new();
    metadata.insert(vs("name"), vs(name));
    metadata.insert(vs("namespace"), vs(namespace));
    if let Some(external_name) = external_name {
        let mut annotations = Mapping::new();
        annotations.insert(vs("crossplane.io/external-name"), vs(external_name));
        metadata.insert(vs("annotations"), Value::Mapping(annotations));
    }
    root.insert(vs("metadata"), Value::Mapping(metadata));

    let mut spec = Mapping::new();
    let mut for_provider = Mapping::new();
    for_provider.insert(vs("description"), vs(description));
    for_provider.insert(vs("enableKeyRotation"), Value::Bool(true));
    for_provider.insert(vs("region"), vs(region));
    spec.insert(vs("forProvider"), Value::Mapping(for_provider));

    let mut provider_ref = Mapping::new();
    provider_ref.insert(vs("kind"), vs("ProviderConfig"));
    provider_ref.insert(vs("name"), vs(provider_config));
    spec.insert(vs("providerConfigRef"), Value::Mapping(provider_ref));

    root.insert(vs("spec"), Value::Mapping(spec));

    let mut value = Value::Mapping(root);
    sort_value(&mut value);
    Ok(value)
}

fn render_yaml(value: &Value) -> Result<String, Box<dyn Error>> {
    let mut rendered = serde_yaml::to_string(value)?;
    if rendered.starts_with("---\n") {
        rendered = rendered.replacen("---\n", "", 1);
    }
    Ok(rendered)
}

fn resolve_kms_region(args: &InitArgs) -> Result<String, Box<dyn Error>> {
    if let Some(region) = args.kms_region.as_ref() {
        let region = region.trim().to_string();
        if region.is_empty() {
            return Err("KMS region cannot be empty".into());
        }
        return Ok(region);
    }

    let config = load_config()?;
    let region = config
        .secrets
        .aws
        .region
        .clone()
        .or_else(|| Some(configured_aws_settings().ok()?.region))
        .unwrap_or_default();
    if region.trim().is_empty() {
        return Err(
            "AWS region is not configured. Run `hops secrets init` again or pass --kms-region."
                .into(),
        );
    }
    Ok(region)
}

fn default_kms_resource_name() -> String {
    let repo_name = std::env::current_dir()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "hops".to_string());
    sanitize_k8s_name(&format!("{repo_name}-sops"))
}

fn default_kms_description() -> String {
    let repo_name = std::env::current_dir()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "hops".to_string());
    format!("SOPS key for {}", repo_name)
}

fn sanitize_k8s_name(input: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;

    for ch in input.chars() {
        let lower = ch.to_ascii_lowercase();
        let valid = lower.is_ascii_lowercase() || lower.is_ascii_digit();
        if valid {
            out.push(lower);
            prev_dash = false;
            continue;
        }

        if !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }

    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "sops".to_string()
    } else {
        trimmed
    }
}

fn is_condition_true(item: &JsonValue, condition_type: &str) -> bool {
    item.get("status")
        .and_then(|status| status.get("conditions"))
        .and_then(JsonValue::as_array)
        .map(|conditions| {
            conditions.iter().any(|condition| {
                condition.get("type").and_then(JsonValue::as_str) == Some(condition_type)
                    && condition.get("status").and_then(JsonValue::as_str) == Some("True")
            })
        })
        .unwrap_or(false)
}

fn condition_summary(item: &JsonValue) -> Option<String> {
    let conditions = item
        .get("status")
        .and_then(|status| status.get("conditions"))
        .and_then(JsonValue::as_array)?;

    let parts = conditions
        .iter()
        .filter_map(|condition| {
            let condition_type = condition.get("type").and_then(JsonValue::as_str)?;
            let status = condition.get("status").and_then(JsonValue::as_str)?;
            let reason = condition
                .get("reason")
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            let message = condition
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or_default();

            let mut summary = format!("{condition_type}={status}");
            if !reason.is_empty() {
                summary.push('(');
                summary.push_str(reason);
                summary.push(')');
            }
            if !message.is_empty() {
                summary.push(':');
                summary.push(' ');
                summary.push_str(message);
            }
            Some(summary)
        })
        .collect::<Vec<_>>();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn resolved_kms_external_name(item: &JsonValue) -> Option<String> {
    item.get("status")
        .and_then(|status| status.get("atProvider"))
        .and_then(|provider| provider.get("keyId"))
        .and_then(JsonValue::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            item.get("status")
                .and_then(|status| status.get("atProvider"))
                .and_then(|provider| provider.get("arn"))
                .and_then(JsonValue::as_str)
                .and_then(kms_key_id_from_arn)
        })
}

fn kms_key_id_from_arn(arn: &str) -> Option<String> {
    arn.split(":key/").nth(1).map(ToString::to_string)
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

fn vs(value: &str) -> Value {
    Value::String(value.to_string())
}

fn prompt_yes_no(prompt: &str, default: bool) -> Result<bool, Box<dyn Error>> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };

    loop {
        let response: String = Input::new()
            .with_prompt(format!("{prompt} {suffix}"))
            .allow_empty(true)
            .interact_text()?;
        match parse_yes_no(&response, default) {
            Some(value) => return Ok(value),
            None => log::warn!("Please enter 'y' or 'n'."),
        }
    }
}

fn normalize_gitignore_dir(path: &Path) -> String {
    let value = path.display().to_string();
    if value.ends_with('/') {
        value
    } else {
        format!("{value}/")
    }
}

fn parse_csv(value: &str) -> Option<Vec<String>> {
    let values = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_yes_no(input: &str, default: bool) -> Option<bool> {
    let value = input.trim().to_ascii_lowercase();
    if value.is_empty() {
        return Some(default);
    }
    if matches!(value.as_str(), "y" | "yes") {
        return Some(true);
    }
    if matches!(value.as_str(), "n" | "no") {
        return Some(false);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_k8s_name_normalizes_repo_name() {
        assert_eq!(sanitize_k8s_name("Hops Ops_repo"), "hops-ops-repo");
        assert_eq!(sanitize_k8s_name("___"), "sops");
    }

    #[test]
    fn build_kms_key_manifest_adds_external_name_when_present() {
        let manifest = build_kms_key_manifest(
            "hops-sops",
            "default",
            "default",
            "us-east-2",
            "SOPS key for hops",
            Some("1234-5678"),
        )
        .expect("manifest");
        let rendered = render_yaml(&manifest).expect("yaml");

        assert!(rendered.contains("apiVersion: kms.aws.m.upbound.io/v1beta1"));
        assert!(rendered.contains("kind: Key"));
        assert!(rendered.contains("namespace: default"));
        assert!(rendered.contains("crossplane.io/external-name: 1234-5678"));
        assert!(rendered.contains("enableKeyRotation: true"));
        assert!(rendered.contains("region: us-east-2"));
    }

    #[test]
    fn ready_condition_detection_matches_true_ready() {
        let item: JsonValue = serde_json::from_str(
            r#"{
              "status": {
                "conditions": [
                  {"type": "Synced", "status": "True"},
                  {"type": "Ready", "status": "True"}
                ]
              }
            }"#,
        )
        .expect("json");

        assert!(is_condition_true(&item, "Ready"));
        assert!(!is_condition_true(&item, "AsyncOperation"));
    }

    #[test]
    fn resolved_kms_external_name_prefers_key_id_then_arn() {
        let with_key_id: JsonValue = serde_json::from_str(
            r#"{"status":{"atProvider":{"keyId":"abc-123","arn":"arn:aws:kms:us-east-2:123:key/ignored"}}}"#,
        )
        .expect("json");
        assert_eq!(
            resolved_kms_external_name(&with_key_id).as_deref(),
            Some("abc-123")
        );

        let with_arn_only: JsonValue = serde_json::from_str(
            r#"{"status":{"atProvider":{"arn":"arn:aws:kms:us-east-2:123:key/def-456"}}}"#,
        )
        .expect("json");
        assert_eq!(
            resolved_kms_external_name(&with_arn_only).as_deref(),
            Some("def-456")
        );
    }

    #[test]
    fn example_secret_rollups_match_sync_rules() {
        assert_eq!(
            Path::new("secrets/examples/app.json")
                .strip_prefix("secrets")
                .expect("prefix")
                .to_string_lossy()
                .trim_start_matches('/')
                .trim_end_matches(".json"),
            "examples/app"
        );
        assert_eq!(
            Path::new("secrets/examples/github")
                .strip_prefix("secrets")
                .expect("prefix")
                .to_string_lossy()
                .trim_start_matches('/'),
            "examples/github"
        );
        assert_eq!(
            Path::new("secrets/examples/slack-webhook-url")
                .strip_prefix("secrets")
                .expect("prefix")
                .to_string_lossy()
                .trim_start_matches('/'),
            "examples/slack-webhook-url"
        );
    }

    #[test]
    fn parse_yes_no_accepts_expected_values() {
        assert_eq!(parse_yes_no("y", false), Some(true));
        assert_eq!(parse_yes_no("yes", false), Some(true));
        assert_eq!(parse_yes_no("n", true), Some(false));
        assert_eq!(parse_yes_no("no", true), Some(false));
        assert_eq!(parse_yes_no("", true), Some(true));
        assert_eq!(parse_yes_no("", false), Some(false));
        assert_eq!(parse_yes_no("maybe", false), None);
    }
}
