//! Adopt hand-made Zitadel inputs before an AuthStack starts generating them.
//!
//! With `spec.externalSecrets.generate`, AuthStack generates the masterkey and
//! first admin password, pushes them to `secretPath` only if that store entry
//! does not exist, and pulls the entry into the masterkey Secret. A cluster
//! created before that has a hand-made masterkey Secret and no store entry, so
//! the first reconcile would replace the key Zitadel's database is encrypted
//! with. Before the cluster manifests are applied, hops copies such a Secret
//! (and the admin password, when one exists) to the store entry with
//! `updatePolicy: IfNotExists`; generation then finds the entry and skips.
//! Values pass between kubectl invocations and are never logged.

use crate::commands::local::{kubectl_apply_stdin, kubectl_command};
use serde::Deserialize;
use serde_yaml::Value;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const ADOPT_NAME: &str = "zitadel-inputs-adopted";
const ADMIN_PASSWORD_SECRET: &str = "zitadel-admin-password";
const SYNC_WAIT: Duration = Duration::from_secs(120);

/// What an AuthStack with generated inputs reads and writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedInputs {
    pub namespace: String,
    pub store: String,
    pub path: String,
    pub masterkey_secret: String,
}

/// Seed the store from existing Secrets for every AuthStack in `cluster_dir`
/// that generates its inputs. No-op when nothing needs adopting.
pub fn adopt_existing_inputs(cluster_dir: &Path) -> Result<(), Box<dyn Error>> {
    for inputs in generated_inputs_in(cluster_dir)? {
        adopt(&inputs)?;
    }
    Ok(())
}

fn generated_inputs_in(dir: &Path) -> Result<Vec<GeneratedInputs>, Box<dyn Error>> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("yaml" | "yml")
            ) {
                let raw = fs::read_to_string(&path)?;
                for document in serde_yaml::Deserializer::from_str(&raw) {
                    // Unparseable files are the apply step's to report.
                    if let Ok(doc) = Value::deserialize(document) {
                        found.extend(generated_inputs(&doc));
                    }
                }
            }
        }
    }
    Ok(found)
}

/// Mirror AuthStack's own defaults: store `hops-aws-secrets-manager`,
/// namespace `auth`.
fn generated_inputs(doc: &Value) -> Option<GeneratedInputs> {
    if doc.get("kind").and_then(Value::as_str) != Some("AuthStack") {
        return None;
    }
    let spec = doc.get("spec")?;
    let eso = spec.get("externalSecrets")?;
    let enabled = |key: &str| eso.get(key).and_then(Value::as_bool) == Some(true);
    if !enabled("enabled") || !enabled("generate") {
        return None;
    }
    let text = |value: Option<&Value>, default: &str| {
        value
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(default)
            .to_string()
    };
    let path = text(eso.get("secretPath"), "");
    let masterkey_secret = text(
        spec.get("firstInstance")
            .and_then(|first| first.get("masterkey"))
            .and_then(|masterkey| masterkey.get("secretRef"))
            .and_then(|secret_ref| secret_ref.get("name")),
        "",
    );
    if path.is_empty() || masterkey_secret.is_empty() {
        return None;
    }
    Some(GeneratedInputs {
        namespace: text(spec.get("namespace"), "auth"),
        store: text(eso.get("secretStoreName"), "hops-aws-secrets-manager"),
        path,
        masterkey_secret,
    })
}

fn adopt(inputs: &GeneratedInputs) -> Result<(), Box<dyn Error>> {
    let Some(masterkey) = read_secret(&inputs.namespace, &inputs.masterkey_secret)? else {
        // Fresh cluster: let AuthStack generate.
        return Ok(());
    };
    if is_externally_managed(&masterkey) {
        // Already pulled from the store; nothing hand-made to protect.
        return Ok(());
    }
    let masterkey_data = secret_data(&masterkey, "masterkey").ok_or_else(|| {
        format!(
            "Secret {}/{} has no `masterkey` key; fix it before AuthStack generates inputs",
            inputs.namespace, inputs.masterkey_secret
        )
    })?;
    // Zitadel only reads the admin password when it first initializes, so a
    // fresh one is fine when none exists.
    let admin_password = read_secret(&inputs.namespace, ADMIN_PASSWORD_SECRET)?
        .and_then(|secret| {
            secret_data(&secret, "admin-password").or_else(|| secret_data(&secret, "password"))
        })
        .map(AdminPassword::Data)
        .unwrap_or_else(|| AdminPassword::Generated(random_password()));
    log::info!(
        "Adopting existing Secret {}/{} into {} {} before AuthStack generates inputs",
        inputs.namespace,
        inputs.masterkey_secret,
        inputs.store,
        inputs.path
    );
    kubectl_apply_stdin(&adoption_manifests(
        inputs,
        &masterkey_data,
        &admin_password,
    )?)?;
    let synced = wait_for_push(&inputs.namespace);
    delete_adoption(&inputs.namespace);
    synced
}

fn read_secret(namespace: &str, name: &str) -> Result<Option<Value>, Box<dyn Error>> {
    let output = kubectl_command(&["get", "secret", name, "-n", namespace, "-o", "json"])
        .output()
        .map_err(|error| format!("kubectl get secret {namespace}/{name}: {error}"))?;
    if output.status.success() {
        return Ok(Some(serde_json::from_slice(&output.stdout)?));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("NotFound") || stderr.contains("not found") {
        return Ok(None);
    }
    Err(format!("kubectl get secret {namespace}/{name}: {}", stderr.trim()).into())
}

fn is_externally_managed(secret: &Value) -> bool {
    secret
        .get("metadata")
        .and_then(|metadata| metadata.get("ownerReferences"))
        .and_then(Value::as_sequence)
        .is_some_and(|owners| {
            owners
                .iter()
                .any(|owner| owner.get("kind").and_then(Value::as_str) == Some("ExternalSecret"))
        })
}

/// Base64 `data` value of `key`, copied as-is (never decoded).
fn secret_data(secret: &Value, key: &str) -> Option<String> {
    secret
        .get("data")?
        .get(key)?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(String::from)
}

enum AdminPassword {
    /// Base64 `data` copied from an existing Secret.
    Data(String),
    /// Plain text for `stringData`.
    Generated(String),
}

/// 32 random hex characters (UUID v4) plus the classes Zitadel's default
/// password policy requires: upper, lower, digit and symbol.
fn random_password() -> String {
    format!("{}Aa1-", uuid::Uuid::new_v4().simple())
}

fn adoption_manifests(
    inputs: &GeneratedInputs,
    masterkey_data: &str,
    admin_password: &AdminPassword,
) -> Result<String, Box<dyn Error>> {
    let namespace = &inputs.namespace;
    let mut secret = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": ADOPT_NAME, "namespace": namespace},
        "type": "Opaque",
        "data": {"masterkey": masterkey_data},
    });
    match admin_password {
        AdminPassword::Data(data) => secret["data"]["admin-password"] = data.as_str().into(),
        AdminPassword::Generated(plain) => {
            secret["stringData"] = serde_json::json!({"admin-password": plain})
        }
    }
    let push = serde_json::json!({
        "apiVersion": "external-secrets.io/v1alpha1",
        "kind": "PushSecret",
        "metadata": {"name": ADOPT_NAME, "namespace": namespace},
        "spec": {
            "refreshInterval": "1h",
            "updatePolicy": "IfNotExists",
            "deletionPolicy": "None",
            "secretStoreRefs": [{"name": inputs.store, "kind": "ClusterSecretStore"}],
            "selector": {"secret": {"name": ADOPT_NAME}},
            "data": [{"match": {"remoteRef": {"remoteKey": inputs.path}}}],
        },
    });
    Ok(format!(
        "{}---\n{}",
        serde_yaml::to_string(&secret)?,
        serde_yaml::to_string(&push)?
    ))
}

fn wait_for_push(namespace: &str) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    loop {
        let output = kubectl_command(&[
            "get",
            "pushsecret",
            ADOPT_NAME,
            "-n",
            namespace,
            "-o",
            "jsonpath={.status.conditions[?(@.type==\"Ready\")].status}|{.status.conditions[?(@.type==\"Ready\")].message}",
        ])
        .output()?;
        let status = String::from_utf8_lossy(&output.stdout).to_string();
        if status.starts_with("True|") {
            return Ok(());
        }
        if started.elapsed() >= SYNC_WAIT {
            return Err(format!(
                "PushSecret {namespace}/{ADOPT_NAME} did not sync within {}s ({status}); not applying an AuthStack that would replace the existing masterkey",
                SYNC_WAIT.as_secs()
            )
            .into());
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// The store entry is the durable copy (`deletionPolicy: None`); drop the
/// in-cluster duplicate.
fn delete_adoption(namespace: &str) {
    for kind in ["pushsecret", "secret"] {
        let _ = kubectl_command(&[
            "delete",
            kind,
            ADOPT_NAME,
            "-n",
            namespace,
            "--ignore-not-found",
        ])
        .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    const STACK: &str = "kind: AuthStack\nspec:\n  externalSecrets:\n    enabled: true\n    generate: true\n    secretStoreName: vault\n    secretPath: local/zitadel\n  firstInstance:\n    masterkey:\n      secretRef:\n        name: zitadel-masterkey\n";

    #[test]
    fn generating_authstack_is_found_with_defaults() {
        assert_eq!(
            generated_inputs(&doc(STACK)),
            Some(GeneratedInputs {
                namespace: "auth".into(),
                store: "vault".into(),
                path: "local/zitadel".into(),
                masterkey_secret: "zitadel-masterkey".into(),
            })
        );
    }

    #[test]
    fn non_generating_or_incomplete_stacks_are_skipped() {
        assert_eq!(
            generated_inputs(&doc(&STACK.replace("generate: true", "generate: false"))),
            None
        );
        assert_eq!(
            generated_inputs(&doc(&STACK.replace("    secretPath: local/zitadel\n", ""))),
            None
        );
        assert_eq!(
            generated_inputs(&doc(&STACK.replace("AuthStack", "IstioStack"))),
            None
        );
    }

    #[test]
    fn template_authstack_generates_inputs() {
        let dir = std::env::temp_dir().join(format!("hops-auth-inputs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("stacks")).unwrap();
        fs::write(
            dir.join("stacks/auth.yaml"),
            include_str!("../../../../templates/local/cluster/stacks/auth.yaml"),
        )
        .unwrap();
        let found = generated_inputs_in(&dir).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "local/zitadel");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_secret_owned_secret_is_managed() {
        let owned = doc("metadata:\n  ownerReferences:\n  - kind: ExternalSecret\n    name: zitadel-masterkey\n");
        assert!(is_externally_managed(&owned));
        assert!(!is_externally_managed(&doc("metadata:\n  name: x\n")));
    }

    #[test]
    fn secret_data_is_copied_and_passwords_meet_policy() {
        let secret = doc("data:\n  masterkey: YWJj\n  empty: ''\n");
        assert_eq!(secret_data(&secret, "masterkey").as_deref(), Some("YWJj"));
        assert_eq!(secret_data(&secret, "empty"), None);
        let password = random_password();
        assert!(password.chars().any(|c| c.is_ascii_uppercase()));
        assert!(password.chars().any(|c| c.is_ascii_lowercase()));
        assert!(password.chars().any(|c| c.is_ascii_digit()));
        assert!(password.contains('-'));
    }

    #[test]
    fn adoption_never_overwrites_the_store() {
        let inputs = generated_inputs(&doc(STACK)).unwrap();
        let yaml =
            adoption_manifests(&inputs, "YWJj", &AdminPassword::Generated("p".into())).unwrap();
        assert!(yaml.contains("masterkey: YWJj"));
        assert!(yaml.contains("admin-password: p"));
        assert!(yaml.contains("updatePolicy: IfNotExists"));
        assert!(yaml.contains("deletionPolicy: None"));
        assert!(yaml.contains("remoteKey: local/zitadel"));
    }
}
