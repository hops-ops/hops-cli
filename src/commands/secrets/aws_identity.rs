use super::{require_command, run_command_output, selected_aws_profile, SOPS_FILE};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Optional repository binding. With neither field present, SOPS and the AWS
/// adapters retain their original environment/default credential behavior.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(super) struct AwsIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl AwsIdentity {
    pub fn validate(&self) -> Result<(), Box<dyn Error>> {
        if let Some(profile) = &self.profile {
            if profile.trim().is_empty()
                || profile.trim() != profile
                || profile.chars().any(char::is_control)
            {
                return Err("secrets.aws.profile must be a non-empty profile name without surrounding whitespace or control characters".into());
            }
        }
        if let Some(account) = &self.account_id {
            if account.len() != 12 || !account.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(
                    "secrets.aws.account_id must be a quoted 12-digit AWS account ID".into(),
                );
            }
        }
        Ok(())
    }

    pub fn selected_profile(&self) -> Option<String> {
        self.profile.clone().or_else(selected_aws_profile)
    }

    pub fn verify_account(&self, account: &str) -> Result<(), Box<dyn Error>> {
        if let Some(expected) = &self.account_id {
            if account != expected {
                return Err(format!(
                    "AWS account mismatch: secrets.aws.account_id requires {}, but the selected credentials belong to {}. No operation was performed.",
                    expected, account
                ).into());
            }
        }
        Ok(())
    }

    pub fn verify_cli_account(&self, region: &str) -> Result<(), Box<dyn Error>> {
        if self.account_id.is_none() {
            return Ok(());
        }
        require_command("aws")?;
        let profile = self.selected_profile();
        let mut args = vec![
            "sts",
            "get-caller-identity",
            "--region",
            region,
            "--output",
            "json",
        ];
        if let Some(profile) = &profile {
            args.extend(["--profile", profile]);
        }
        let output = run_command_output("aws", &args).map_err(|err| {
            format!("Unable to verify secrets.aws.account_id: {err}. Ensure the selected AWS profile is logged in.")
        })?;
        let response: serde_json::Value = serde_json::from_slice(&output)?;
        let account = response["Account"]
            .as_str()
            .ok_or("AWS account ID not available from STS GetCallerIdentity")?;
        self.verify_account(account)
    }

    fn configured(&self) -> bool {
        self.profile.is_some() || self.account_id.is_some()
    }

    pub fn sops_output(&self, mode: &str, source: &str) -> Result<Vec<u8>, Box<dyn Error>> {
        let flag = match mode {
            "encrypt" => "--encrypt",
            "decrypt" => "--decrypt",
            _ => return Err(format!("Unsupported sops mode: {mode}").into()),
        };
        let mut command = Command::new("sops");
        command.args([flag, "--input-type=raw", "--output-type=raw"]);
        if self.configured() {
            if let Some(profile) = self.selected_profile() {
                // The flag alone does not select the profile for all SOPS paths
                // (notably decryption). Set its environment as well and prevent
                // ambient static credentials from overriding the named profile.
                command.args(["--aws-profile", &profile]);
                command.env("AWS_PROFILE", &profile);
                command.env("AWS_DEFAULT_PROFILE", &profile);
                for key in [
                    "AWS_ACCESS_KEY_ID",
                    "AWS_SECRET_ACCESS_KEY",
                    "AWS_SESSION_TOKEN",
                    "AWS_SECURITY_TOKEN",
                    "AWS_ROLE_ARN",
                    "AWS_WEB_IDENTITY_TOKEN_FILE",
                    "AWS_ROLE_SESSION_NAME",
                ] {
                    command.env_remove(key);
                }
            }
        }
        let output = command.arg(source).output()?;
        if !output.status.success() {
            return Err(format!(
                "sops exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(output.stdout)
    }

    /// SOPS can use profiles embedded in creation rules or ciphertext metadata.
    /// Refuse conflicting bindings rather than checking one caller and allowing
    /// SOPS to select another. Never rewrite recipients or encrypted metadata.
    pub fn validate_sops_profiles(&self, source: &Path, mode: &str) -> Result<(), Box<dyn Error>> {
        if !self.configured() {
            return Ok(());
        }
        let profile = self.selected_profile();
        if mode == "encrypt" {
            if let Some(path) = sops_config_path()? {
                let config: Value = serde_yaml::from_str(&fs::read_to_string(&path)?)?;
                validate_profiles(&config["creation_rules"], profile.as_deref())?;
            }
        } else if mode == "decrypt" {
            validate_ciphertext_profiles(source, profile.as_deref())?;
        }
        Ok(())
    }
}

fn sops_config_path() -> Result<Option<PathBuf>, Box<dyn Error>> {
    if let Some(path) = std::env::var_os("SOPS_CONFIG").filter(|value| !value.is_empty()) {
        return Ok(Some(PathBuf::from(path)));
    }
    Ok(std::env::current_dir()?
        .ancestors()
        .map(|path| path.join(SOPS_FILE))
        .find(|path| path.is_file()))
}

fn validate_ciphertext_profiles(path: &Path, profile: Option<&str>) -> Result<(), Box<dyn Error>> {
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            validate_ciphertext_profiles(&entry?.path(), profile)?;
        }
    } else if path.is_file() {
        let ciphertext: Value = serde_yaml::from_slice(&fs::read(path)?)
            .map_err(|_| format!("Cannot inspect SOPS metadata in {}", path.display()))?;
        validate_profiles(&ciphertext["sops"]["kms"], profile)?;
    }
    Ok(())
}

fn validate_profiles(value: &Value, profile: Option<&str>) -> Result<(), Box<dyn Error>> {
    match value {
        Value::Mapping(mapping) => {
            if let Some(stored) = mapping
                .get(Value::String("aws_profile".into()))
                .and_then(Value::as_str)
            {
                if !stored.is_empty() && Some(stored) != profile {
                    let profile = profile.unwrap_or("<default credential chain>");
                    return Err(format!("SOPS aws_profile '{stored}' conflicts with the selected Hops AWS profile '{profile}'. Existing SOPS configuration and ciphertext were left unchanged.").into());
                }
            }
            for child in mapping.values() {
                validate_profiles(child, profile)?;
            }
        }
        Value::Sequence(values) => {
            for child in values {
                validate_profiles(child, profile)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_configs_do_not_acquire_identity_fields_when_saved() {
        let config: super::super::RepoConfig =
            serde_yaml::from_str("secrets:\n  aws:\n    path: aws\n    region: us-east-2\n")
                .unwrap();
        config.secrets.aws.identity.validate().unwrap();
        let value = serde_yaml::to_value(config).unwrap();
        let aws = value["secrets"]["aws"].as_mapping().unwrap();
        assert!(!aws.contains_key(Value::String("profile".into())));
        assert!(!aws.contains_key(Value::String("account_id".into())));
    }

    #[test]
    fn identity_validation_preserves_leading_zeroes_and_rejects_ambiguous_values() {
        let valid: AwsIdentity =
            serde_yaml::from_str("profile: repo\naccount_id: '012345678901'\n").unwrap();
        valid.validate().unwrap();
        assert!(valid.verify_account("012345678901").is_ok());
        assert!(valid.verify_account("123456789012").is_err());
        for profile in ["", " ", " repo", "repo ", "repo\nother"] {
            let identity = AwsIdentity {
                profile: Some(profile.into()),
                ..Default::default()
            };
            assert!(identity.validate().is_err());
        }
        for account in ["", "123", "1234567890123", "12345678901x", " 123456789012"] {
            let identity = AwsIdentity {
                account_id: Some(account.into()),
                ..Default::default()
            };
            assert!(identity.validate().is_err());
        }
    }

    #[test]
    fn sops_profile_validation_handles_nested_key_groups_and_does_not_change_keys() {
        let mut rules: Value = serde_yaml::from_str("- key_groups:\n  - kms:\n    - arn: 'arn:aws:kms:us-east-1:111111111111:key/key'\n      aws_profile: repo\n").unwrap();
        let before = rules.clone();
        validate_profiles(&rules, Some("repo")).unwrap();
        assert_eq!(rules, before);
        assert!(validate_profiles(&rules, Some("other")).is_err());
        rules[0]["key_groups"][0]["kms"][0]["aws_profile"] = Value::String(String::new());
        validate_profiles(&rules, Some("repo")).unwrap();
    }
}
