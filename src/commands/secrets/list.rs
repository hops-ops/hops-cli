use super::{aws_clients, collect_local_secret_names, configured_tags, repo_name, SECRET_DIR};
use rusoto_secretsmanager::{ListSecretsRequest, SecretsManager, SecretsManagerClient};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::path::Path;

pub fn run() -> Result<(), Box<dyn Error>> {
    let local_names = collect_local_secret_names(Path::new(SECRET_DIR));
    let local_lookup: HashSet<String> = local_names.iter().cloned().collect();
    let repo = repo_name()?;

    let mut expected_tags = configured_tags()?;
    expected_tags.push(("hops".to_string(), "true".to_string()));
    expected_tags.push(("hops-secrets-repo".to_string(), repo.clone()));
    expected_tags.push(("hops.ops.com.ai/cli".to_string(), "true".to_string()));
    expected_tags.sort();
    expected_tags.dedup();

    let runtime = tokio::runtime::Runtime::new()?;
    let (client, _) = aws_clients()?;
    let remote_secrets = fetch_remote_secrets(&runtime, &client)?;

    let mut remote_by_name = HashMap::new();
    for secret in remote_secrets {
        if secret.repo_tag.as_deref() == Some(repo.as_str()) || local_lookup.contains(&secret.name)
        {
            remote_by_name.insert(secret.name.clone(), secret);
        }
    }

    let mut names = BTreeSet::new();
    for name in local_names {
        names.insert(name);
    }
    for name in remote_by_name.keys() {
        names.insert(name.clone());
    }

    println!("Repo secrets for {}", repo);
    println!(
        "{:<40} {:<8} {:<8} {:<24} Status",
        "Name", "Local", "Remote", "KMS Key"
    );

    for name in names {
        let local = local_lookup.contains(&name);
        let remote = remote_by_name.get(&name);
        let status = match (local, remote.is_some()) {
            (true, true) => {
                let tags_match = remote
                    .map(|secret| expected_tags.iter().all(|tag| secret.tags.contains(tag)))
                    .unwrap_or(false);
                if tags_match {
                    "ok"
                } else {
                    "remote tags differ"
                }
            }
            (true, false) => "missing remote secret",
            (false, true) => "missing local secret",
            (false, false) => "-",
        };

        let kms = remote
            .and_then(|secret| secret.kms_key_id.clone())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<40} {:<8} {:<8} {:<24} {}",
            name,
            yes_no(local),
            yes_no(remote.is_some()),
            shorten_kms_key(&kms),
            status
        );
    }

    Ok(())
}

#[derive(Clone, Debug)]
struct RemoteSecret {
    name: String,
    tags: Vec<(String, String)>,
    repo_tag: Option<String>,
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
                let mut repo_tag = None;
                for tag in entry.tags.unwrap_or_default() {
                    if let (Some(key), Some(value)) = (tag.key, tag.value) {
                        if key == "hops-secrets-repo" {
                            repo_tag = Some(value.clone());
                        }
                        tags.push((key, value));
                    }
                }

                results.push(RemoteSecret {
                    name,
                    tags,
                    repo_tag,
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
