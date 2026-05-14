//! `hops auth bootstrap <cluster>` — generate the durable AuthStack
//! secret plaintexts into the repo's `secrets/` tree.
//!
//! Writes two files matching the AuthStack composition's ExternalSecret
//! contract (AWS SM secret path / JSON property):
//!
//!   <plaintext>/<aws>/<cluster>/zitadel/masterkey/masterkey
//!   <plaintext>/<aws>/<cluster>/zitadel/admin-password/password
//!
//! The platform's normal secrets pipeline takes it from there:
//!
//!   hops secrets encrypt    # SOPS-encrypts into secrets-encrypted/
//!   hops secrets sync aws   # pushes to AWS Secrets Manager
//!
//! Idempotent: existing plaintexts are left alone unless `--force`.

use crate::commands::secrets;
use clap::Args;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Args, Debug)]
pub struct BootstrapArgs {
    /// Cluster name. Becomes a path segment under the AWS secrets root
    /// (e.g. `secrets/aws/pat-local/zitadel/masterkey/masterkey`).
    pub cluster: String,

    /// Override the default `<cluster>/zitadel` path prefix beneath the
    /// AWS secrets root. Use when the AuthStack manifest's
    /// `externalSecrets.*.secretPath` values don't start with
    /// `<cluster>/zitadel`.
    #[arg(long)]
    pub prefix: Option<String>,

    /// Overwrite plaintexts that already exist. The default is to leave
    /// existing files alone so re-running `bootstrap` is safe.
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: &BootstrapArgs) -> Result<(), Box<dyn Error>> {
    let (plaintext_root, _encrypted_root) = secrets::configured_secret_paths()?;
    let aws_settings = secrets::configured_aws_settings()?;
    let prefix = args
        .prefix
        .clone()
        .unwrap_or_else(|| format!("{}/zitadel", args.cluster));

    let masterkey_path = plaintext_root
        .join(&aws_settings.path)
        .join(&prefix)
        .join("masterkey")
        .join("masterkey");
    let admin_pwd_path = plaintext_root
        .join(&aws_settings.path)
        .join(&prefix)
        .join("admin-password")
        .join("password");

    log::info!("Bootstrapping AuthStack durable secret plaintexts:");
    write_secret(&masterkey_path, &generate_32_char_random(), args.force)?;
    write_secret(&admin_pwd_path, &generate_complex_password(), args.force)?;

    log::info!("Next:");
    log::info!("  hops secrets encrypt   # SOPS-encrypts into secrets-encrypted/");
    log::info!("  hops secrets sync aws  # pushes to AWS Secrets Manager");
    Ok(())
}

fn write_secret(path: &PathBuf, value: &str, force: bool) -> Result<(), Box<dyn Error>> {
    if path.exists() && !force {
        log::info!("  skip   {}: already present (use --force to overwrite)", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, value)?;
    let verb = if force { "update" } else { "create" };
    log::info!("  {}  {}", verb, path.display());
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
