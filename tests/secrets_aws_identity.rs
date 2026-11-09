#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Repo {
    root: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("hops-aws-identity-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("bin")).unwrap();
        let repo = Self { root };
        repo.executable(
            "aws",
            r#"#!/bin/sh
printf '%s\n' "$@" > "$HOPS_TEST_ROOT/aws-args"
if [ "${HOPS_TEST_AWS_FAIL:-}" = 1 ]; then
  echo 'SSO session expired for selected profile' >&2
  exit 1
fi
case "$1 $2" in
  'sts get-caller-identity') printf '{"Account":"%s"}\n' "$HOPS_TEST_ACCOUNT" ;;
  'configure export-credentials') echo 'fixture stops before any AWS SDK call' >&2; exit 1 ;;
  *) echo 'unexpected AWS command' >&2; exit 1 ;;
esac
"#,
        );
        repo.executable("sops", r#"#!/bin/sh
printf '%s\n' "$@" > "$HOPS_TEST_ROOT/sops-args"
printf '%s\n' "${AWS_PROFILE-unset}" "${AWS_DEFAULT_PROFILE-unset}" "${AWS_ACCESS_KEY_ID-unset}" "${AWS_SECRET_ACCESS_KEY-unset}" "${AWS_SESSION_TOKEN-unset}" > "$HOPS_TEST_ROOT/sops-env"
printf '%s\n' "${AWS_ROLE_ARN-unset}" "${AWS_WEB_IDENTITY_TOKEN_FILE-unset}" "${AWS_ROLE_SESSION_NAME-unset}" > "$HOPS_TEST_ROOT/sops-web-env"
printf 'fixture-output'
"#);
        repo
    }

    fn executable(&self, name: &str, contents: &str) {
        let path = self.root.join("bin").join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn write(&self, path: &str, value: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }

    fn config(&self, fields: &str) {
        self.write(
            ".hops.yaml",
            &format!("secrets:\n  aws:\n    region: us-east-2\n{fields}"),
        );
    }

    fn source(&self, mode: &str) -> (&str, &str) {
        let (source, destination, content) = if mode == "encrypt" {
            ("secrets/input", "secrets-encrypted", "fixture-plaintext")
        } else {
            (
                "secrets-encrypted/input",
                "secrets",
                r#"{"sops":{"kms":[{"aws_profile":""}]},"data":"fixture-ciphertext"}"#,
            )
        };
        self.write(source, content);
        (source, destination)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hops-cli"));
        command
            .args(args)
            .current_dir(&self.root)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("HOME", self.root.join("home"))
            .env("AWS_CONFIG_FILE", self.root.join("aws-config"))
            .env(
                "AWS_SHARED_CREDENTIALS_FILE",
                self.root.join("aws-credentials"),
            )
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("AWS_PROFILE", "ambient-profile")
            .env("AWS_DEFAULT_PROFILE", "ambient-default")
            .env("AWS_ACCESS_KEY_ID", "fixture-ambient-access")
            .env("AWS_SECRET_ACCESS_KEY", "fixture-ambient-secret")
            .env("AWS_SESSION_TOKEN", "fixture-ambient-token")
            .env("AWS_ROLE_ARN", "arn:aws:iam::999999999999:role/ambient")
            .env(
                "AWS_WEB_IDENTITY_TOKEN_FILE",
                self.root.join("ambient-token"),
            )
            .env("AWS_ROLE_SESSION_NAME", "ambient-session")
            .env("HOPS_TEST_ROOT", &self.root)
            .env("HOPS_TEST_ACCOUNT", "111111111111")
            .env_remove("SOPS_CONFIG")
            .env_remove("HOPS_TEST_AWS_FAIL");
        command
    }

    fn assert_failure(&self, output: Output, message: &str, destination: &str) {
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "{stderr}");
        assert!(!self.root.join("sops-args").exists());
        assert!(!self.root.join(destination).exists());
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn unconfigured_sops_retains_its_original_arguments_and_environment() {
    let repo = Repo::new();
    repo.source("encrypt");
    let output = repo.command(&["secrets", "encrypt"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!repo.read("sops-args").contains("--aws-profile"));
    assert!(repo
        .read("sops-env")
        .starts_with("ambient-profile\nambient-default\nfixture-ambient-access\n"));
    assert!(!repo.root.join("aws-args").exists());
}

#[test]
fn repo_binding_is_used_by_both_sops_operations_and_checked_before_output() {
    for mode in ["encrypt", "decrypt"] {
        let repo = Repo::new();
        repo.config("    profile: repo-profile\n    account_id: '111111111111'\n");
        repo.write(
            ".sops.yaml",
            "creation_rules:\n  - kms: arn:aws:kms:us-east-2:222222222222:key/fixture\n",
        );
        let (source, destination) = repo.source(mode);
        let original = repo.read(source);
        let config = repo.read(".sops.yaml");
        let output = repo.command(&["secrets", mode]).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(repo.read("aws-args").contains("--profile\nrepo-profile\n"));
        assert!(repo
            .read("sops-args")
            .contains("--aws-profile\nrepo-profile\n"));
        assert_eq!(
            repo.read("sops-env"),
            "repo-profile\nrepo-profile\nunset\nunset\nunset\n"
        );
        assert_eq!(repo.read("sops-web-env"), "unset\nunset\nunset\n");
        assert_eq!(repo.read(source), original);
        assert_eq!(repo.read(".sops.yaml"), config);
        assert_eq!(repo.read(&format!("{destination}/input")), "fixture-output");
    }
}

#[test]
fn wrong_account_and_expired_login_produce_no_sops_output() {
    for mode in ["encrypt", "decrypt"] {
        for expired in [false, true] {
            let repo = Repo::new();
            repo.config("    profile: repo-profile\n    account_id: '222222222222'\n");
            let (_, destination) = repo.source(mode);
            let mut command = repo.command(&["secrets", mode]);
            if expired {
                command.env("HOPS_TEST_AWS_FAIL", "1");
            }
            repo.assert_failure(
                command.output().unwrap(),
                if expired {
                    "logged in"
                } else {
                    "AWS account mismatch"
                },
                destination,
            );
        }
    }
}

#[test]
fn profile_only_binding_needs_no_separate_sts_check() {
    let repo = Repo::new();
    repo.config("    profile: repo-profile\n");
    repo.source("encrypt");
    let output = repo.command(&["secrets", "encrypt"]).output().unwrap();
    assert!(output.status.success());
    assert!(!repo.root.join("aws-args").exists());
    assert!(repo
        .read("sops-args")
        .contains("--aws-profile\nrepo-profile\n"));
}

#[test]
fn account_only_binding_uses_the_environment_profile() {
    let repo = Repo::new();
    repo.config("    account_id: '111111111111'\n");
    repo.source("encrypt");
    let output = repo.command(&["secrets", "encrypt"]).output().unwrap();
    assert!(output.status.success());
    assert!(repo
        .read("aws-args")
        .contains("--profile\nambient-profile\n"));
    assert!(repo
        .read("sops-env")
        .starts_with("ambient-profile\nambient-profile\nunset\n"));
}

#[test]
fn conflicting_sops_profiles_are_refused_without_changing_configuration_or_ciphertext() {
    for mode in ["encrypt", "decrypt"] {
        let repo = Repo::new();
        repo.config("    profile: repo-profile\n    account_id: '111111111111'\n");
        let (source, destination) = repo.source(mode);
        if mode == "encrypt" {
            repo.write(
                ".sops.yaml",
                "creation_rules:\n  - kms: fixture-key\n    aws_profile: other-profile\n",
            );
        } else {
            repo.write(
                source,
                r#"{"sops":{"kms":[{"aws_profile":"other-profile"}]},"data":"fixture-ciphertext"}"#,
            );
        }
        let original = repo.read(source);
        let output = repo.command(&["secrets", mode]).output().unwrap();
        repo.assert_failure(
            output,
            "conflicts with the selected Hops AWS profile",
            destination,
        );
        assert_eq!(repo.read(source), original);
        assert!(!repo.root.join("aws-args").exists());
    }
}

#[test]
fn invalid_identity_configuration_is_rejected_without_contacting_aws_or_sops() {
    for fields in [
        "    profile: ''\n",
        "    profile: ' repo'\n",
        "    account_id: '123'\n",
        "    account_id: '11111111111x'\n",
    ] {
        let repo = Repo::new();
        repo.config(fields);
        repo.source("encrypt");
        repo.assert_failure(
            repo.command(&["secrets", "encrypt"]).output().unwrap(),
            "secrets.aws.",
            "secrets-encrypted",
        );
        assert!(!repo.root.join("aws-args").exists());
    }
}

#[test]
fn aws_commands_use_the_bound_profile_and_legacy_environment_selection() {
    for bound in [false, true] {
        for args in [
            vec!["secrets", "sync", "aws", "--yes", "--cleanup"],
            vec!["secrets", "list"],
        ] {
            let repo = Repo::new();
            if bound {
                repo.config("    profile: repo-profile\n    account_id: '111111111111'\n");
            }
            repo.write("secrets/aws/input", "fixture-value");
            let output = repo.command(&args).output().unwrap();
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr)
                .contains("fixture stops before any AWS SDK call"));
            assert_eq!(
                repo.read("aws-args"),
                format!(
                    "configure\nexport-credentials\n--profile\n{}\n--format\nprocess\n",
                    if bound {
                        "repo-profile"
                    } else {
                        "ambient-profile"
                    }
                )
            );
        }
    }
}

#[test]
fn account_only_binding_without_a_named_profile_keeps_default_credentials() {
    let repo = Repo::new();
    repo.config("    account_id: '111111111111'\n");
    repo.source("encrypt");
    let output = repo
        .command(&["secrets", "encrypt"])
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_DEFAULT_PROFILE")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!repo.read("aws-args").contains("--profile"));
    assert!(repo
        .read("sops-env")
        .starts_with("unset\nunset\nfixture-ambient-access\n"));
}

#[test]
fn sops_config_override_is_checked_before_any_operation() {
    let repo = Repo::new();
    repo.config("    profile: repo-profile\n    account_id: '111111111111'\n");
    repo.source("encrypt");
    repo.write(
        ".sops.yaml",
        "creation_rules:\n  - aws_profile: repo-profile\n",
    );
    repo.write(
        "other-sops.yaml",
        "creation_rules:\n  - aws_profile: other-profile\n",
    );
    let output = repo
        .command(&["secrets", "encrypt"])
        .env("SOPS_CONFIG", repo.root.join("other-sops.yaml"))
        .output()
        .unwrap();
    repo.assert_failure(
        output,
        "conflicts with the selected Hops AWS profile",
        "secrets-encrypted",
    );
}

#[test]
fn explicit_sops_default_profile_cannot_override_an_account_only_default_chain() {
    let repo = Repo::new();
    repo.config("    account_id: '111111111111'\n");
    repo.source("decrypt");
    repo.write(
        "secrets-encrypted/input",
        r#"{"sops":{"kms":[{"aws_profile":"default"}]},"data":"fixture-ciphertext"}"#,
    );
    let output = repo
        .command(&["secrets", "decrypt"])
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_DEFAULT_PROFILE")
        .output()
        .unwrap();
    repo.assert_failure(
        output,
        "conflicts with the selected Hops AWS profile",
        "secrets",
    );
}

#[test]
fn grouped_ciphertext_profile_conflicts_stop_before_output() {
    for conflicting_group in 0..2 {
        let repo = Repo::new();
        repo.config("    profile: repo-profile\n    account_id: '111111111111'\n");
        let (source, destination) = repo.source("decrypt");
        let mut groups = serde_json::json!([
            {"kms": [{"aws_profile": "repo-profile"}]},
            {"kms": [{"aws_profile": "repo-profile"}]}
        ]);
        groups[conflicting_group]["kms"][0]["aws_profile"] = "other-profile".into();
        let ciphertext = serde_json::json!({
            "sops": {"key_groups": groups, "shamir_threshold": 2},
            "data": "fixture-ciphertext"
        })
        .to_string();
        repo.write(source, &ciphertext);
        let output = repo.command(&["secrets", "decrypt"]).output().unwrap();
        repo.assert_failure(
            output,
            "conflicts with the selected Hops AWS profile",
            destination,
        );
        assert_eq!(repo.read(source), ciphertext);
        assert!(!repo.root.join("aws-args").exists());
    }
}

#[test]
fn grouped_ciphertext_allows_matching_empty_and_missing_profiles() {
    let repo = Repo::new();
    repo.config("    profile: repo-profile\n    account_id: '111111111111'\n");
    let (source, _) = repo.source("decrypt");
    let ciphertext = serde_json::json!({
        "sops": {"key_groups": [
            {"kms": [{"aws_profile": "repo-profile"}]},
            {"kms": [{"aws_profile": ""}, {}]}
        ]},
        "data": "fixture-ciphertext"
    })
    .to_string();
    repo.write(source, &ciphertext);
    let output = repo.command(&["secrets", "decrypt"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(repo.read(source), ciphertext);
    assert_eq!(repo.read("secrets/input"), "fixture-output");
}

#[test]
fn encryption_context_profile_names_are_not_identity_settings() {
    for (mode, grouped) in [("encrypt", true), ("decrypt", false), ("decrypt", true)] {
        let repo = Repo::new();
        repo.config("    profile: repo-profile\n    account_id: '111111111111'\n");
        let (source, _) = repo.source(mode);
        let key = serde_json::json!({
            "arn": "arn:aws:kms:us-east-2:111111111111:key/fixture",
            "aws_profile": "repo-profile",
            "context": {"aws_profile": "encryption-context-data"}
        });
        let keys = if grouped {
            serde_json::json!({"key_groups": [{"kms": [key]}]})
        } else {
            serde_json::json!({"kms": [key]})
        };
        if mode == "encrypt" {
            repo.write(
                ".sops.yaml",
                &serde_json::json!({"creation_rules": [keys]}).to_string(),
            );
        } else {
            repo.write(
                source,
                &serde_json::json!({"sops": keys, "data": "fixture-ciphertext"}).to_string(),
            );
        }
        let original = repo.read(source);
        let config = fs::read(repo.root.join(".sops.yaml")).ok();
        let output = repo.command(&["secrets", mode]).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(repo.read(source), original);
        assert_eq!(fs::read(repo.root.join(".sops.yaml")).ok(), config);
    }
}
