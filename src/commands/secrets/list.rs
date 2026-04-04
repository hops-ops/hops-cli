use super::{
    aws_clients, collect_local_secret_names, configured_aws_settings, configured_github_settings,
    configured_secret_paths, require_command, run_command_output_string,
};
use rusoto_secretsmanager::{ListSecretsRequest, SecretsManager, SecretsManagerClient};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::error::Error;
use std::fs;
use std::path::Path;

pub fn run() -> Result<(), Box<dyn Error>> {
    let (plaintext_dir, _) = configured_secret_paths()?;
    let aws_settings = configured_aws_settings()?;
    let aws_root = plaintext_dir.join(&aws_settings.path);
    let local_names = collect_local_secret_names(Path::new(&aws_root));
    let local_lookup: HashSet<String> = local_names.iter().cloned().collect();

    let mut expected_tags = BTreeMap::new();
    for (key, value) in aws_settings.tags {
        expected_tags.insert(key, value);
    }
    expected_tags.insert("hops.ops.com.ai/secret".to_string(), "true".to_string());

    let runtime = tokio::runtime::Runtime::new()?;
    let (client, _) = aws_clients(&aws_settings.region)?;
    let remote_secrets = fetch_remote_secrets(&runtime, &client)?;

    let mut remote_by_name = HashMap::new();
    let mut other_remote_rows = Vec::new();
    for secret in remote_secrets {
        if secret.managed || local_lookup.contains(&secret.name) {
            remote_by_name.insert(secret.name.clone(), secret);
            continue;
        }

        let status = if is_crossplane_managed(&secret) {
            "managed by crossplane"
        } else {
            "-"
        };
        let kms = secret
            .kms_key_id
            .clone()
            .unwrap_or_else(|| "aws/secretsmanager".to_string());
        other_remote_rows.push(RemoteOnlyRow {
            name: secret.name,
            tags: format_tags(&secret.tags),
            kms_key: shorten_kms_key(&kms),
            status: status.to_string(),
        });
    }
    other_remote_rows.sort_by(|left, right| left.name.cmp(&right.name));

    let mut names = BTreeSet::new();
    for name in local_names {
        names.insert(name);
    }
    for name in remote_by_name.keys() {
        names.insert(name.clone());
    }

    let mut rows = Vec::new();
    for name in names {
        let local = local_lookup.contains(&name);
        let remote = remote_by_name.get(&name);
        let missing_tags = remote
            .map(|secret| missing_expected_tags(secret, &expected_tags))
            .unwrap_or_default();
        let status = match (local, remote.is_some()) {
            (true, true) if missing_tags.is_empty() => "ok",
            (true, true) => "remote tags differ",
            (true, false) => "missing remote secret",
            (false, true) => "missing local secret",
            (false, false) => "-",
        };
        let remote_tags = remote
            .map(|secret| format_tags(&secret.tags))
            .unwrap_or_else(|| "-".to_string());
        let expected_tags_display = format_expected_tags(&expected_tags);

        let kms = remote
            .and_then(|secret| secret.kms_key_id.clone())
            .unwrap_or_else(|| "-".to_string());
        rows.push(SecretRow {
            name,
            local,
            remote: remote.is_some(),
            remote_tags,
            expected_tags: expected_tags_display,
            kms_key: shorten_kms_key(&kms),
            status: status.to_string(),
        });
    }

    println!("Managed secrets");
    print_secret_rows(&rows);
    println!();
    println!("Other AWS secrets");
    print_remote_only_rows(&other_remote_rows);
    println!();
    print_github_section()?;

    Ok(())
}

struct SecretRow {
    name: String,
    local: bool,
    remote: bool,
    remote_tags: String,
    expected_tags: String,
    kms_key: String,
    status: String,
}

struct RemoteOnlyRow {
    name: String,
    tags: String,
    kms_key: String,
    status: String,
}

struct GithubSecretRow {
    repo: String,
    name: String,
    local: bool,
    remote: bool,
    status: String,
}

#[derive(Clone, Debug)]
struct RemoteSecret {
    name: String,
    tags: Vec<(String, String)>,
    managed: bool,
    kms_key_id: Option<String>,
}

fn fetch_remote_secrets(
    runtime: &tokio::runtime::Runtime,
    client: &SecretsManagerClient,
) -> Result<Vec<RemoteSecret>, Box<dyn Error>> {
    let mut next_token = None;
    let mut results = Vec::new();

    loop {
        let response = runtime.block_on(client.list_secrets(ListSecretsRequest {
            next_token: next_token.clone(),
            ..Default::default()
        }))?;

        if let Some(secret_list) = response.secret_list {
            for entry in secret_list {
                let Some(name) = entry.name else {
                    continue;
                };

                let mut tags = Vec::new();
                let mut managed = false;
                for tag in entry.tags.unwrap_or_default() {
                    if let (Some(key), Some(value)) = (tag.key, tag.value) {
                        if key == "hops.ops.com.ai/secret" {
                            managed = true;
                        }
                        tags.push((key, value));
                    }
                }

                results.push(RemoteSecret {
                    name,
                    tags,
                    managed,
                    kms_key_id: entry.kms_key_id,
                });
            }
        }

        if let Some(token) = response.next_token {
            next_token = Some(token);
        } else {
            break;
        }
    }

    Ok(results)
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn shorten_kms_key(value: &str) -> String {
    if value == "-" {
        return value.to_string();
    }
    value.rsplit('/').next().unwrap_or(value).to_string()
}

fn format_tags(tags: &[(String, String)]) -> String {
    if tags.is_empty() {
        return "-".to_string();
    }

    let mut sorted = tags.to_vec();
    sorted.sort();
    sorted
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_expected_tags(tags: &BTreeMap<String, String>) -> String {
    if tags.is_empty() {
        return "-".to_string();
    }

    tags.iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn missing_expected_tags(
    secret: &RemoteSecret,
    expected_tags: &BTreeMap<String, String>,
) -> Vec<String> {
    expected_tags
        .iter()
        .filter(|(key, value)| {
            !secret
                .tags
                .iter()
                .any(|(actual_key, actual_value)| actual_key == *key && actual_value == *value)
        })
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

fn is_crossplane_managed(secret: &RemoteSecret) -> bool {
    secret.tags.iter().any(|(key, _)| key == "crossplane-kind")
}

fn print_secret_rows(rows: &[SecretRow]) {
    let mut name_width = "Name".len();
    let mut local_width = "Local".len();
    let mut remote_width = "Remote".len();
    let mut remote_tags_width = "Remote Tags".len();
    let mut expected_tags_width = "Expected Tags".len();
    let mut kms_key_width = "KMS Key".len();
    let mut status_width = "Status".len();

    for row in rows {
        name_width = name_width.max(row.name.len());
        local_width = local_width.max(yes_no(row.local).len());
        remote_width = remote_width.max(yes_no(row.remote).len());
        for line in lines_or_dash(&row.remote_tags) {
            remote_tags_width = remote_tags_width.max(line.len());
        }
        for line in lines_or_dash(&row.expected_tags) {
            expected_tags_width = expected_tags_width.max(line.len());
        }
        kms_key_width = kms_key_width.max(row.kms_key.len());
        for line in lines_or_dash(&row.status) {
            status_width = status_width.max(line.len());
        }
    }

    println!(
        "{:<name_width$}  {:<local_width$}  {:<remote_width$}  {:<kms_key_width$}  {:<remote_tags_width$}  {:<expected_tags_width$}  {:<status_width$}",
        "Name",
        "Local",
        "Remote",
        "KMS Key",
        "Remote Tags",
        "Expected Tags",
        "Status",
        name_width = name_width,
        local_width = local_width,
        remote_width = remote_width,
        kms_key_width = kms_key_width,
        remote_tags_width = remote_tags_width,
        expected_tags_width = expected_tags_width,
        status_width = status_width,
    );
    println!(
        "{}  {}  {}  {}  {}  {}  {}",
        "-".repeat(name_width),
        "-".repeat(local_width),
        "-".repeat(remote_width),
        "-".repeat(kms_key_width),
        "-".repeat(remote_tags_width),
        "-".repeat(expected_tags_width),
        "-".repeat(status_width),
    );

    for row in rows {
        let remote_tag_lines = lines_or_dash(&row.remote_tags);
        let expected_tag_lines = lines_or_dash(&row.expected_tags);
        let status_lines = lines_or_dash(&row.status);
        let row_height = remote_tag_lines
            .len()
            .max(expected_tag_lines.len())
            .max(status_lines.len());

        for i in 0..row_height {
            println!(
                "{:<name_width$}  {:<local_width$}  {:<remote_width$}  {:<kms_key_width$}  {:<remote_tags_width$}  {:<expected_tags_width$}  {:<status_width$}",
                if i == 0 { row.name.as_str() } else { "" },
                if i == 0 { yes_no(row.local) } else { "" },
                if i == 0 { yes_no(row.remote) } else { "" },
                if i == 0 { row.kms_key.as_str() } else { "" },
                remote_tag_lines.get(i).copied().unwrap_or(""),
                expected_tag_lines.get(i).copied().unwrap_or(""),
                status_lines.get(i).copied().unwrap_or(""),
                name_width = name_width,
                local_width = local_width,
                remote_width = remote_width,
                kms_key_width = kms_key_width,
                remote_tags_width = remote_tags_width,
                expected_tags_width = expected_tags_width,
                status_width = status_width,
            );
        }
        println!();
    }
}

fn print_remote_only_rows(rows: &[RemoteOnlyRow]) {
    if rows.is_empty() {
        println!("(none)");
        return;
    }

    let mut name_width = "Name".len();
    let mut tags_width = "Tags".len();
    let mut kms_key_width = "KMS Key".len();
    let mut status_width = "Status".len();

    for row in rows {
        name_width = name_width.max(row.name.len());
        for line in lines_or_dash(&row.tags) {
            tags_width = tags_width.max(line.len());
        }
        kms_key_width = kms_key_width.max(row.kms_key.len());
        for line in lines_or_dash(&row.status) {
            status_width = status_width.max(line.len());
        }
    }

    println!(
        "{:<name_width$}  {:<kms_key_width$}  {:<tags_width$}  {:<status_width$}",
        "Name",
        "KMS Key",
        "Tags",
        "Status",
        name_width = name_width,
        kms_key_width = kms_key_width,
        tags_width = tags_width,
        status_width = status_width,
    );
    println!(
        "{}  {}  {}  {}",
        "-".repeat(name_width),
        "-".repeat(kms_key_width),
        "-".repeat(tags_width),
        "-".repeat(status_width),
    );

    for row in rows {
        let tag_lines = lines_or_dash(&row.tags);
        let status_lines = lines_or_dash(&row.status);
        let row_height = tag_lines.len().max(status_lines.len());

        for i in 0..row_height {
            println!(
                "{:<name_width$}  {:<kms_key_width$}  {:<tags_width$}  {:<status_width$}",
                if i == 0 { row.name.as_str() } else { "" },
                if i == 0 { row.kms_key.as_str() } else { "" },
                tag_lines.get(i).copied().unwrap_or(""),
                status_lines.get(i).copied().unwrap_or(""),
                name_width = name_width,
                kms_key_width = kms_key_width,
                tags_width = tags_width,
                status_width = status_width,
            );
        }
        println!();
    }
}

fn print_github_section() -> Result<(), Box<dyn Error>> {
    println!("GitHub secrets");

    let github_settings = configured_github_settings()?;
    let (plaintext_dir, _) = configured_secret_paths()?;
    let source_root = plaintext_dir.join(&github_settings.path);
    if !source_root.exists() {
        println!("(none)");
        return Ok(());
    }

    require_command("gh")?;
    ensure_gh_auth_for_list()?;

    let owner = resolve_github_owner_for_list(github_settings.owner.as_deref())?;
    let repos = resolve_github_repos_for_list(&source_root, &github_settings)?;
    if repos.is_empty() {
        println!("(none)");
        return Ok(());
    }

    let shared_root = source_root.join(&github_settings.shared_path);
    let shared_secrets = collect_github_target_secrets_for_list(&shared_root)?;

    let mut rows = Vec::new();
    for repo in repos {
        let local_secrets =
            collect_github_repo_secret_names(&source_root, &shared_root, &shared_secrets, &repo)?;
        let remote_secrets = fetch_github_repo_secret_names(&owner, &repo)?;

        let mut names = BTreeSet::new();
        for name in &local_secrets {
            names.insert(name.clone());
        }
        for name in &remote_secrets {
            names.insert(name.clone());
        }

        for name in names {
            let local = local_secrets.contains(&name);
            let remote = remote_secrets.contains(&name);
            let status = match (local, remote) {
                (true, true) => "ok",
                (true, false) => "missing remote secret",
                (false, true) => "missing local secret",
                (false, false) => "-",
            };
            rows.push(GithubSecretRow {
                repo: repo.clone(),
                name,
                local,
                remote,
                status: status.to_string(),
            });
        }
    }

    print_github_rows(&rows);
    Ok(())
}

fn ensure_gh_auth_for_list() -> Result<(), Box<dyn Error>> {
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

fn resolve_github_owner_for_list(configured_owner: Option<&str>) -> Result<String, Box<dyn Error>> {
    let env_owner = env::var("GH_OWNER").ok();
    let env_github_owner = env::var("GITHUB_OWNER").ok();
    let owner = [
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
        None => Err(
            "GitHub owner is not configured. Set secrets.github.owner or GH_OWNER/GITHUB_OWNER."
                .into(),
        ),
    }
}

fn resolve_github_repos_for_list(
    source_root: &Path,
    settings: &super::GithubSecretsRuntimeConfig,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut repos = settings.shared_repos.clone();

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

fn collect_github_repo_secret_names(
    source_root: &Path,
    shared_root: &Path,
    shared_secrets: &[(String, String, String)],
    repo: &str,
) -> Result<HashSet<String>, Box<dyn Error>> {
    let repo_dir = source_root.join(repo);
    let repo_file = source_root.join(format!("{repo}.json"));
    let mut names = shared_secrets
        .iter()
        .map(|(name, _, _)| name.clone())
        .collect::<HashSet<_>>();

    if repo_dir.is_dir() {
        for (name, _, _) in collect_github_target_secrets_for_list(&repo_dir)? {
            names.insert(name);
        }
    } else if repo_file.is_file() {
        for (name, _, _) in collect_github_target_secrets_for_list(&repo_file)? {
            names.insert(name);
        }
    } else if !shared_root.exists() || shared_secrets.is_empty() {
        return Ok(HashSet::new());
    }

    Ok(names)
}

fn collect_github_target_secrets_for_list(
    target: &Path,
) -> Result<Vec<(String, String, String)>, Box<dyn Error>> {
    if !target.exists() {
        return Ok(Vec::new());
    }
    if target.is_file() {
        return collect_github_file_secrets_for_list(target, target);
    }

    let mut out = Vec::new();
    collect_github_dir_secrets_for_list(target, target, &mut out)?;
    Ok(out)
}

fn collect_github_dir_secrets_for_list(
    root: &Path,
    current: &Path,
    out: &mut Vec<(String, String, String)>,
) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(current)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_github_dir_secrets_for_list(root, &path, out)?;
        } else if path.is_file() {
            out.extend(collect_github_file_secrets_for_list(root, &path)?);
        }
    }
    Ok(())
}

fn collect_github_file_secrets_for_list(
    root: &Path,
    path: &Path,
) -> Result<Vec<(String, String, String)>, Box<dyn Error>> {
    if path.extension().and_then(|value| value.to_str()) == Some("json") {
        let contents = fs::read_to_string(path)?;
        let secrets = parse_github_secret_map_for_list(&contents, path)?;
        return Ok(secrets
            .into_iter()
            .map(|(name, value)| (name, value, path.display().to_string()))
            .collect());
    }

    let secret_name = github_secret_name_for_list(root, path)?;
    let secret_value = fs::read_to_string(path)?.trim().to_string();
    Ok(vec![(
        secret_name,
        secret_value,
        path.display().to_string(),
    )])
}

fn parse_github_secret_map_for_list(
    contents: &str,
    path: &Path,
) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let value: serde_json::Value = serde_json::from_str(contents)
        .map_err(|err| format!("Failed parsing JSON in {}: {}", path.display(), err))?;
    let object = value
        .as_object()
        .ok_or_else(|| format!("GitHub secret JSON must be an object: {}", path.display()))?;

    let mut secrets = Vec::new();
    for (key, value) in object {
        let secret_name = normalize_github_secret_name_for_list(key);
        let secret_value = value
            .as_str()
            .map(ToString::to_string)
            .unwrap_or_else(|| value.to_string());
        secrets.push((secret_name, secret_value));
    }
    Ok(secrets)
}

fn github_secret_name_for_list(repo_root: &Path, path: &Path) -> Result<String, Box<dyn Error>> {
    let relative = path.strip_prefix(repo_root)?;
    let raw = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("__");
    Ok(normalize_github_secret_name_for_list(
        raw.trim_end_matches(".json"),
    ))
}

fn normalize_github_secret_name_for_list(value: &str) -> String {
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

#[derive(Deserialize)]
struct GithubSecretListEntry {
    name: String,
}

fn fetch_github_repo_secret_names(
    owner: &str,
    repo: &str,
) -> Result<HashSet<String>, Box<dyn Error>> {
    let output = run_command_output_string(
        "gh",
        &[
            "secret",
            "list",
            "--repo",
            &format!("{owner}/{repo}"),
            "--json",
            "name",
        ],
    )?;
    let entries: Vec<GithubSecretListEntry> = serde_json::from_str(&output)?;
    Ok(entries.into_iter().map(|entry| entry.name).collect())
}

fn print_github_rows(rows: &[GithubSecretRow]) {
    if rows.is_empty() {
        println!("(none)");
        return;
    }

    let mut repo_width = "Repo".len();
    let mut name_width = "Name".len();
    let mut local_width = "Local".len();
    let mut remote_width = "Remote".len();
    let mut status_width = "Status".len();

    for row in rows {
        repo_width = repo_width.max(row.repo.len());
        name_width = name_width.max(row.name.len());
        local_width = local_width.max(yes_no(row.local).len());
        remote_width = remote_width.max(yes_no(row.remote).len());
        status_width = status_width.max(row.status.len());
    }

    println!(
        "{:<repo_width$}  {:<name_width$}  {:<local_width$}  {:<remote_width$}  {:<status_width$}",
        "Repo",
        "Name",
        "Local",
        "Remote",
        "Status",
        repo_width = repo_width,
        name_width = name_width,
        local_width = local_width,
        remote_width = remote_width,
        status_width = status_width,
    );
    println!(
        "{}  {}  {}  {}  {}",
        "-".repeat(repo_width),
        "-".repeat(name_width),
        "-".repeat(local_width),
        "-".repeat(remote_width),
        "-".repeat(status_width),
    );

    for row in rows {
        println!(
            "{:<repo_width$}  {:<name_width$}  {:<local_width$}  {:<remote_width$}  {:<status_width$}",
            row.repo,
            row.name,
            yes_no(row.local),
            yes_no(row.remote),
            row.status,
            repo_width = repo_width,
            name_width = name_width,
            local_width = local_width,
            remote_width = remote_width,
            status_width = status_width,
        );
    }
}

fn lines_or_dash(value: &str) -> Vec<&str> {
    if value == "-" {
        vec!["-"]
    } else {
        let lines: Vec<&str> = value.split('\n').collect();
        if lines.is_empty() {
            vec![""]
        } else {
            lines
        }
    }
}
