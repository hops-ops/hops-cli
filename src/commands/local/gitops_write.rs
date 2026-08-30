//! Write non-secret provider manifests under a .gitops/local/cluster directory.
//!
//! Credential Secrets stay live-only until a local external-secrets story exists.
//! `--gitops` writers intentionally omit Secret data.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

/// One file to materialize under the gitops root.
#[derive(Debug, Clone)]
pub struct GitopsFile {
    /// Path relative to the gitops root, e.g. `providers/aws.yaml`.
    pub rel_path: String,
    pub yaml: String,
}

/// Resolve and create the gitops directory (creates parents).
pub fn ensure_gitops_dir(gitops: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let root = if gitops.is_absolute() {
        gitops.to_path_buf()
    } else {
        std::env::current_dir()?.join(gitops)
    };
    fs::create_dir_all(&root)?;
    Ok(root)
}

/// Write non-secret manifests. Overwrites existing files with the same path.
/// Returns absolute paths written.
pub fn write_gitops_files(
    gitops: &Path,
    files: &[GitopsFile],
) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let root = ensure_gitops_dir(gitops)?;
    let mut written = Vec::new();
    for f in files {
        if f.rel_path.is_empty() || f.rel_path.contains("..") {
            return Err(format!("invalid gitops relative path: {}", f.rel_path).into());
        }
        let dest = root.join(&f.rel_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        // Ensure trailing newline for clean git diffs.
        let mut body = f.yaml.trim_end().to_string();
        body.push('\n');
        fs::write(&dest, body)?;
        written.push(dest);
    }
    // Document the secrets gap next to written providers.
    let readme = root.join("SECRETS.md");
    if !readme.exists() {
        fs::write(&readme, SECRETS_README)?;
        written.push(readme);
    }
    Ok(written)
}

const SECRETS_README: &str = r#"# Secrets (local gitops)

Provider and ProviderConfig YAML in this tree are **non-secret**.

Credential `Secret` objects are **not** written here. Cloud environments use
External Secrets / SOPS; local workbench does not have that path yet.

Until then:

1. Apply this tree with `hops local gitops cluster`.
2. Create live secrets with:
   - `hops local aws` / `github` / `zitadel` (without relying on git for credentials)
   - or a future `hops local secrets sync`

ProviderConfig resources reference secret names/keys only — fill those secrets
on the cluster out-of-band.
"#;

/// Log paths written for humans.
pub fn log_written(paths: &[PathBuf]) {
    for p in paths {
        log::info!("gitops wrote {}", p.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_nested_files_and_secrets_readme() {
        let dir = std::env::temp_dir().join(format!(
            "hops-gitops-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        let files = vec![
            GitopsFile {
                rel_path: "providers/aws.yaml".into(),
                yaml: "apiVersion: pkg.crossplane.io/v1\nkind: Provider\n".into(),
            },
            GitopsFile {
                rel_path: "providers/aws-provider-config.yaml".into(),
                yaml: "apiVersion: aws.m.upbound.io/v1beta1\nkind: ProviderConfig\n".into(),
            },
        ];
        let written = write_gitops_files(&dir, &files).unwrap();
        assert!(dir.join("providers/aws.yaml").exists());
        assert!(dir.join("providers/aws-provider-config.yaml").exists());
        assert!(dir.join("SECRETS.md").exists());
        assert!(written.len() >= 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_parent_path_escape() {
        let dir = std::env::temp_dir().join(format!("hops-gitops-bad-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let err = write_gitops_files(
            &dir,
            &[GitopsFile {
                rel_path: "../escape.yaml".into(),
                yaml: "x: 1\n".into(),
            }],
        );
        assert!(err.is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
