//! `hops auth bootstrap <cluster>` — seed the durable AuthStack secrets in
//! AWS Secrets Manager.
//!
//! Writes the two values the AuthStack composition's reconciler-pattern
//! needs to be projected back via ExternalSecret on every install:
//!
//!   <prefix>/zitadel/masterkey        { "masterkey": "<32 chars>" }
//!   <prefix>/zitadel/admin-password   { "password":  "<32 chars>" }
//!
//! Idempotent: if a path is already present in AWS SM, we skip it (and
//! say so) unless `--force` is set, in which case we overwrite with a
//! freshly-generated value. The CLI never touches Kubernetes — applying
//! the AuthStack manifest stays a separate, declarative step.

use crate::commands::secrets;
use clap::Args;
use rusoto_core::RusotoError;
use rusoto_secretsmanager::{
    CreateSecretRequest, GetSecretValueError, GetSecretValueRequest, PutSecretValueRequest,
    SecretsManager, SecretsManagerClient, Tag,
};
use std::env;
use std::error::Error;
use tokio::runtime::Runtime;
use uuid::Uuid;

const DEFAULT_REGION: &str = "us-east-1";

#[derive(Args, Debug)]
pub struct BootstrapArgs {
    /// Cluster name. Becomes the leading path segment in AWS SM
    /// (e.g. `pat-local/zitadel/masterkey`).
    pub cluster: String,

    /// Override the default `<cluster>/zitadel` AWS SM path prefix.
    /// Use when the AuthStack manifest's `externalSecrets.*.secretPath`
    /// values don't start with `<cluster>/zitadel`.
    #[arg(long)]
    pub prefix: Option<String>,

    /// AWS region for AWS Secrets Manager. Defaults to AWS_REGION env var,
    /// falling back to us-east-1.
    #[arg(long)]
    pub region: Option<String>,

    /// Overwrite values that already exist in AWS SM. The default is to
    /// leave existing values alone so re-running `bootstrap` is safe.
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: &BootstrapArgs) -> Result<(), Box<dyn Error>> {
    let prefix = args
        .prefix
        .clone()
        .unwrap_or_else(|| format!("{}/zitadel", args.cluster));

    let region = args
        .region
        .clone()
        .or_else(|| env::var("AWS_REGION").ok())
        .unwrap_or_else(|| DEFAULT_REGION.to_string());

    let runtime = Runtime::new()?;
    // Reuse the AWS credential helpers from `hops secrets` so this command
    // resolves SSO profiles the same way (AWS_PROFILE → aws configure
    // export-credentials → StaticProvider).
    let (client, _sts) = secrets::aws_clients(&region)?;

    let masterkey_path = format!("{}/masterkey", prefix);
    let admin_pwd_path = format!("{}/admin-password", prefix);

    log::info!("Bootstrapping AuthStack durable secrets in region {}", region);
    log::info!("  masterkey  → {}", masterkey_path);
    log::info!("  admin-pwd  → {}", admin_pwd_path);

    upsert_json_secret(
        &runtime,
        &client,
        &masterkey_path,
        "masterkey",
        &generate_32_char_random(),
        args.cluster.as_str(),
        args.force,
    )?;
    upsert_json_secret(
        &runtime,
        &client,
        &admin_pwd_path,
        "password",
        &generate_complex_password(),
        args.cluster.as_str(),
        args.force,
    )?;

    log::info!("Bootstrap complete. Next: kubectl apply -f local/ (or your GitOps equivalent).");
    Ok(())
}

/// Idempotent put: create if missing, otherwise leave alone (or
/// overwrite if `force`). The value is wrapped as `{ <key>: <random> }`
/// to match what the AuthStack composition's ExternalSecrets read via
/// their `remoteRef.property` selectors.
fn upsert_json_secret(
    runtime: &Runtime,
    client: &SecretsManagerClient,
    path: &str,
    json_key: &str,
    value: &str,
    cluster: &str,
    force: bool,
) -> Result<(), Box<dyn Error>> {
    let json = serde_json::json!({ json_key: value }).to_string();

    let existing = runtime.block_on(client.get_secret_value(GetSecretValueRequest {
        secret_id: path.to_string(),
        ..Default::default()
    }));

    match existing {
        Ok(_) if !force => {
            log::info!("  skip   {}: already present (use --force to overwrite)", path);
            return Ok(());
        }
        Ok(_) => {
            log::info!("  update {}: overwriting per --force", path);
            runtime.block_on(client.put_secret_value(PutSecretValueRequest {
                secret_id: path.to_string(),
                secret_string: Some(json),
                // AWS SM requires a UUID-shaped idempotency token on
                // PutSecretValue as well as CreateSecret.
                client_request_token: Some(Uuid::new_v4().to_string()),
                ..Default::default()
            }))?;
        }
        Err(RusotoError::Service(GetSecretValueError::ResourceNotFound(_))) => {
            log::info!("  create {}", path);
            runtime.block_on(client.create_secret(CreateSecretRequest {
                name: path.to_string(),
                secret_string: Some(json),
                // ClientRequestToken makes CreateSecret idempotent; AWS
                // requires a UUID-shaped value.
                client_request_token: Some(Uuid::new_v4().to_string()),
                tags: Some(vec![
                    Tag {
                        key: Some("hops.ops.com.ai/secret".to_string()),
                        value: Some("true".to_string()),
                    },
                    Tag {
                        key: Some("hops.ops.com.ai/cluster".to_string()),
                        value: Some(cluster.to_string()),
                    },
                    Tag {
                        key: Some("hops.ops.com.ai/authstack".to_string()),
                        value: Some("bootstrap".to_string()),
                    },
                ]),
                ..Default::default()
            }))?;
        }
        Err(err) => return Err(err.into()),
    }

    Ok(())
}

/// Returns 32 hex characters of cryptographic randomness (~122 bits of
/// entropy from a v4 UUID). Used for values Zitadel treats as opaque
/// text — masterkey, etc.
fn generate_32_char_random() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Returns a 32-char password that satisfies Zitadel's default password
/// complexity policy (HasUppercase + HasLowercase + HasNumber +
/// HasSymbol). We start from a v4 UUID's hex form (which gives digits
/// and lowercase letters), then deterministically force one uppercase
/// letter and one symbol so all four character classes are present.
fn generate_complex_password() -> String {
    let mut out: Vec<char> = Uuid::new_v4().simple().to_string().chars().collect();

    // Position 0 becomes the forced symbol. Find an ASCII letter from
    // position 1 onward and uppercase it. (We skip position 0 so we don't
    // promote a letter that's about to be overwritten by the symbol.)
    // If somehow no letter exists in positions 1.. (all-digit 32-hex
    // rolls are ~6e-8 probability) seed position 1 directly.
    let letter_idx = out
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, c)| c.is_ascii_alphabetic())
        .map(|(i, _)| i);
    if let Some(idx) = letter_idx {
        out[idx] = out[idx].to_ascii_uppercase();
    } else {
        out[1] = 'A';
    }

    // Force a symbol at position 0. Hex chars don't include symbols, so
    // this is the only way to satisfy HasSymbol without giving up the
    // randomness of the rest of the string.
    out[0] = '!';

    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_32_char_random_is_32_chars() {
        let s = generate_32_char_random();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_32_char_random_is_random() {
        // Two consecutive calls should never collide.
        assert_ne!(generate_32_char_random(), generate_32_char_random());
    }

    #[test]
    fn generate_complex_password_satisfies_zitadel_default_policy() {
        // Zitadel default: HasUppercase + HasLowercase + HasNumber + HasSymbol.
        for _ in 0..200 {
            let pwd = generate_complex_password();
            assert_eq!(pwd.len(), 32);
            assert!(
                pwd.chars().any(|c| c.is_ascii_uppercase()),
                "no uppercase: {}",
                pwd
            );
            assert!(
                pwd.chars().any(|c| c.is_ascii_lowercase()),
                "no lowercase: {}",
                pwd
            );
            assert!(
                pwd.chars().any(|c| c.is_ascii_digit()),
                "no digit: {}",
                pwd
            );
            assert!(
                pwd.chars().any(|c| !c.is_ascii_alphanumeric()),
                "no symbol: {}",
                pwd
            );
        }
    }
}
