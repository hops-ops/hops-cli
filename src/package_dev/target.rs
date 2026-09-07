//! Non-secret target bindings. Loading and validation are read-only.
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteTarget {
    pub api_version: String,
    pub kind: String,
    pub metadata: Identity,
    pub spec: TargetSpec,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TargetSpec {
    /// kube-system namespace UID, not a workstation's kube-context name.
    pub cluster_uid: String,
    #[serde(default)]
    pub allow_development: bool,
    pub allowed_package_prefixes: Vec<String>,
    pub registry: Registry,
    pub ownership: Ownership,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Registry {
    pub namespace: String,
    pub service: String,
    pub port: u16,
    /// HTTPS authority only, no userinfo, path, query, or credentials.
    pub pull_endpoint: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum Ownership {
    /// Direct package writes are allowed only after separate live ownership
    /// checks prove the selected objects explicitly opt in to this target.
    Api {},
    Gitops {
        repository: String,
        #[serde(rename = "baseBranch")]
        base_branch: String,
        #[serde(rename = "writePolicy", default)]
        write_policy: WritePolicy,
        #[serde(rename = "packageDirectory")]
        package_directory: String,
        #[serde(rename = "imageConfigDirectory")]
        image_config_directory: String,
        argo: Argo,
    },
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum WritePolicy {
    #[default]
    Worktree,
    Direct,
    PrMerge,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Argo {
    pub namespace: String,
    pub application: String,
}

fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// Reject traversal and symlinks in every existing component, including output
/// paths that do not yet exist. The caller may bind the root differently per laptop.
pub fn bounded_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let root = fs::canonicalize(root).map_err(|_| "target root is unavailable")?;
    if relative.is_empty()
        || relative.contains('\\')
        || relative
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        || !Path::new(relative)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err("target path must be a normalized relative path".into());
    }
    let mut path = root;
    for component in Path::new(relative).components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("target paths must not contain symlinks".into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("cannot inspect target path".into()),
        }
    }
    Ok(path)
}

impl RemoteTarget {
    pub fn load(root: &Path, selected: &str) -> Result<Self> {
        if !name(selected) {
            return Err("invalid remote target name".into());
        }
        let path = bounded_path(root, &format!(".hops/remotes/{selected}.yaml"))?;
        let metadata = fs::metadata(&path).map_err(|_| "remote target file is missing")?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            return Err("remote target must be a regular file at most 64 KiB".into());
        }
        let bytes = fs::read(path).map_err(|_| "cannot read remote target")?;
        // serde errors can echo unknown fields/values, so never return them.
        let target: Self = serde_yaml::from_slice(&bytes).map_err(|_| {
            "invalid remote target schema; credentials and unknown fields are forbidden"
        })?;
        target.validate(selected)?;
        if let Ownership::Gitops {
            repository,
            base_branch,
            package_directory,
            image_config_directory,
            ..
        } = &target.spec.ownership
        {
            let packages = bounded_path(root, package_directory)?;
            let imageconfigs = bounded_path(root, image_config_directory)?;
            if packages.starts_with(&imageconfigs) || imageconfigs.starts_with(&packages) {
                return Err("package and ImageConfig directories must not overlap".into());
            }
            let actual_root = git(root, &["rev-parse", "--show-toplevel"])?;
            if fs::canonicalize(actual_root.trim())? != fs::canonicalize(root)? {
                return Err("--gitops must select the repository root".into());
            }
            if canonical_repository(&git(root, &["remote", "get-url", "origin"])?)?
                != canonical_repository(repository)?
            {
                return Err("target Git repository does not match checkout origin".into());
            }
            if git(root, &["branch", "--show-current"])?.trim() != base_branch {
                return Err("target Git base branch does not match checkout".into());
            }
            git(
                root,
                &[
                    "ls-files",
                    "--error-unmatch",
                    "--",
                    &format!(".hops/remotes/{selected}.yaml"),
                ],
            )?;
            if !git(
                root,
                &[
                    "status",
                    "--porcelain",
                    "--",
                    &format!(".hops/remotes/{selected}.yaml"),
                ],
            )?
            .is_empty()
            {
                return Err("GitOps target declaration must be committed and unchanged".into());
            }
        }
        // A second declaration for the same cluster is ambiguous, even if names
        // or kube contexts differ. Never read symlinked/unknown target files.
        let dir = bounded_path(root, ".hops/remotes")?;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let filename = entry.file_name();
            let Some(filename) = filename.to_str() else {
                return Err("invalid target filename".into());
            };
            if !filename.ends_with(".yaml") || filename == format!("{selected}.yaml") {
                continue;
            }
            let other_name = filename.trim_end_matches(".yaml");
            if !name(other_name) {
                return Err("invalid target filename".into());
            }
            let path = bounded_path(root, &format!(".hops/remotes/{filename}"))?;
            let metadata = fs::metadata(&path)?;
            if !metadata.is_file() || metadata.len() > 64 * 1024 {
                return Err("remote target exceeds 64 KiB".into());
            }
            let other: Self = serde_yaml::from_slice(&fs::read(path)?)
                .map_err(|_| "invalid sibling remote target schema")?;
            other.validate(other_name)?;
            if other.spec.cluster_uid == target.spec.cluster_uid {
                return Err(
                    "duplicate remote cluster identity; use one target per control plane".into(),
                );
            }
        }
        Ok(target)
    }

    fn validate(&self, selected: &str) -> Result<()> {
        if self.api_version != "hops.remote/v1alpha1"
            || self.kind != "RemoteTarget"
            || self.metadata.name != selected
        {
            return Err("remote target API, kind, or identity mismatch".into());
        }
        if !self.spec.allow_development {
            return Err("target does not permit development activation".into());
        }
        if uuid::Uuid::parse_str(&self.spec.cluster_uid)
            .ok()
            .is_none_or(|uid| uid.to_string() != self.spec.cluster_uid)
        {
            return Err("target requires the expected kube-system namespace UID".into());
        }
        let mut unique = HashSet::new();
        if self.spec.allowed_package_prefixes.is_empty() {
            return Err("target has no allowed package prefixes".into());
        }
        for prefix in &self.spec.allowed_package_prefixes {
            let repo = prefix
                .strip_suffix('/')
                .ok_or("allowed package prefixes must end at a namespace slash")?;
            if repo.split('/').count() < 2
                || !repo
                    .split('/')
                    .next()
                    .is_some_and(|host| host.contains('.'))
            {
                return Err(
                    "allowed package prefixes require a registry and organization namespace".into(),
                );
            }
            if !unique.insert(prefix) {
                return Err("duplicate allowed package prefix".into());
            }
            super::registry::validate_image_name(repo)?;
        }
        self.spec.registry.validate()?;
        if let Ownership::Gitops {
            repository,
            base_branch,
            package_directory,
            image_config_directory,
            argo,
            ..
        } = &self.spec.ownership
        {
            canonical_repository(repository)?;
            if base_branch.is_empty()
                || base_branch.starts_with('-')
                || base_branch.contains("..")
                || base_branch.ends_with('.')
                || base_branch.ends_with(".lock")
                || base_branch
                    .split('/')
                    .any(|part| part.is_empty() || part.starts_with('.'))
                || !base_branch
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c))
                || package_directory.split('/').any(|part| part == ".git")
                || image_config_directory.split('/').any(|part| part == ".git")
                || !name(&argo.namespace)
                || !name(&argo.application)
            {
                return Err("invalid GitOps ownership binding".into());
            }
        }
        Ok(())
    }

    pub fn verify_cluster_uid(&self, observed: &str) -> Result<()> {
        if observed != self.spec.cluster_uid {
            return Err("selected kube context points to a different cluster identity".into());
        }
        Ok(())
    }
}

impl Registry {
    fn validate(&self) -> Result<()> {
        let authority = self
            .pull_endpoint
            .strip_prefix("https://")
            .ok_or("registry pull endpoint must use HTTPS")?;
        let (host, port) = authority
            .split_once(':')
            .map(|(host, port)| (host, Some(port)))
            .unwrap_or((authority, None));
        if !name(&self.namespace)
            || !name(&self.service)
            || self.port == 0
            || host.is_empty()
            || !host.split('.').all(name)
            || port.is_some_and(|port| port.parse::<u16>().ok().filter(|p| *p != 0).is_none())
        {
            return Err("invalid registry binding; expected internal HTTPS authority without credentials or paths".into());
        }
        Ok(())
    }

    pub fn verify_service(&self, service: &serde_json::Value) -> Result<()> {
        let metadata = &service["metadata"];
        if metadata["name"] != self.service
            || metadata["namespace"] != self.namespace
            || metadata["labels"]["hops.ops.com.ai/registry-mode"] != "push"
            || metadata["labels"]["hops.ops.com.ai/registry-access"] != "write"
            || service["spec"]["type"] != "ClusterIP"
            || service["spec"]["externalIPs"]
                .as_array()
                .is_some_and(|ips| !ips.is_empty())
            || service["spec"]["selector"]
                .as_object()
                .is_none_or(|s| s.is_empty())
            || !service["spec"]["ports"]
                .as_array()
                .is_some_and(|ports| ports.iter().any(|p| p["port"] == self.port))
        {
            return Err("registry Service binding is not a private push-enabled upload Service; caches are not write targets".into());
        }
        Ok(())
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|_| "cannot inspect Git target binding")?;
    if !output.status.success() {
        return Err("cannot verify Git target binding".into());
    }
    String::from_utf8(output.stdout).map_err(|_| "invalid Git target binding".into())
}

fn canonical_repository(value: &str) -> Result<String> {
    let value = value.trim();
    let (host, path) = if let Some(rest) = value.strip_prefix("https://") {
        rest.split_once('/')
            .ok_or("invalid Git repository identity")?
    } else if let Some(rest) = value.strip_prefix("git@") {
        rest.split_once(':')
            .ok_or("invalid Git repository identity")?
    } else {
        return Err("Git repository must use credential-free HTTPS or git@host SSH".into());
    };
    if !host.split('.').all(name) {
        return Err("invalid Git repository host".into());
    }
    let path = path.strip_suffix(".git").unwrap_or(path);
    super::registry::validate_repository(path)?;
    Ok(format!("{host}/{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    const TARGET: &str = r#"
apiVersion: hops.remote/v1alpha1
kind: RemoteTarget
metadata:
  name: development
spec:
  clusterUid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee
  allowDevelopment: true
  allowedPackagePrefixes: ["ghcr.io/hops-ops/"]
  registry:
    namespace: crossplane-dev
    service: registry-upload
    port: 5001
    pullEndpoint: https://registry.crossplane-dev.svc.cluster.local:5000
  ownership:
    mode: api
"#;

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!("hops-target-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join(".hops/remotes")).unwrap();
        fs::write(root.join(".hops/remotes/development.yaml"), TARGET).unwrap();
        root
    }

    #[test]
    fn api_target_is_portable_without_git_or_local_backend() {
        for _ in 0..2 {
            let root = fixture();
            let target = RemoteTarget::load(&root, "development").unwrap();
            target
                .verify_cluster_uid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
                .unwrap();
            assert!(target.verify_cluster_uid("wrong-cluster").is_err());
            assert!(matches!(target.spec.ownership, Ownership::Api {}));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn targets_fail_closed_without_echoing_input_or_credentials() {
        let root = fixture();
        for contents in [
            TARGET.replace("allowDevelopment: true", "allowDevelopment: false"),
            TARGET.replace("mode: api", "mode: api\n    token: synthetic-secret-canary"),
            TARGET.replace(
                "https://registry.",
                "https://user:synthetic-secret-canary@registry.",
            ),
            TARGET.replace("name: development", "name: other"),
            TARGET.replace("ghcr.io/hops-ops/", "ghcr.io/"),
        ] {
            fs::write(root.join(".hops/remotes/development.yaml"), contents).unwrap();
            let result = RemoteTarget::load(&root, "development");
            assert!(result.is_err());
            assert!(!result
                .unwrap_err()
                .to_string()
                .contains("synthetic-secret-canary"));
        }
        assert!(RemoteTarget::load(&root, "../development").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_cluster_identity_and_escaping_paths_are_rejected() {
        let root = fixture();
        fs::write(
            root.join(".hops/remotes/other.yaml"),
            TARGET.replace("name: development", "name: other"),
        )
        .unwrap();
        assert!(RemoteTarget::load(&root, "development")
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
        for relative in [
            "../escape",
            "/tmp/escape",
            "a/../escape",
            "./escape",
            "a//b",
        ] {
            assert!(bounded_path(&root, relative).is_err());
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(std::env::temp_dir(), root.join("escape")).unwrap();
            assert!(bounded_path(&root, "escape/new-file").is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn service_caches_and_public_write_bindings_are_rejected() {
        let root = fixture();
        let target = RemoteTarget::load(&root, "development").unwrap();
        let mut service = serde_json::json!({
            "metadata": {"name": "registry-upload", "namespace": "crossplane-dev", "labels": {
                "hops.ops.com.ai/registry-mode": "push", "hops.ops.com.ai/registry-access": "write"
            }},
            "spec": {"type": "ClusterIP", "selector": {"app": "registry"}, "ports": [{"port": 5001}]}
        });
        target.spec.registry.verify_service(&service).unwrap();
        service["metadata"]["labels"]["hops.ops.com.ai/registry-mode"] = "cache".into();
        assert!(target.spec.registry.verify_service(&service).is_err());
        service["metadata"]["labels"]["hops.ops.com.ai/registry-mode"] = "push".into();
        service["spec"]["type"] = "LoadBalancer".into();
        assert!(target.spec.registry.verify_service(&service).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_identity_is_transport_independent_and_never_accepts_credentials() {
        assert_eq!(
            canonical_repository("https://github.com/hops-ops/example.git").unwrap(),
            canonical_repository("git@github.com:hops-ops/example.git").unwrap()
        );
        assert!(canonical_repository("https://token@github.com/hops-ops/example").is_err());
        assert!(canonical_repository("https://github.com/../example").is_err());
    }

    #[test]
    fn committed_gitops_targets_bind_repository_branch_and_safe_directories() {
        for remote_url in [
            "https://github.com/hops-ops/example.git",
            "git@github.com:hops-ops/example.git",
        ] {
            let root = fixture();
            let contents = TARGET.replace("mode: api", "mode: gitops\n    repository: https://github.com/hops-ops/example.git\n    baseBranch: main\n    packageDirectory: .gitops/packages\n    imageConfigDirectory: .gitops/imageconfigs\n    argo:\n      namespace: argocd\n      application: packages");
            fs::write(root.join(".hops/remotes/development.yaml"), &contents).unwrap();
            git(&root, &["init", "--initial-branch=main"]).unwrap();
            git(&root, &["remote", "add", "origin", remote_url]).unwrap();
            git(&root, &["add", "--", ".hops/remotes/development.yaml"]).unwrap();
            git(
                &root,
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "commit",
                    "-m",
                    "fixture",
                ],
            )
            .unwrap();
            let before = git(&root, &["status", "--porcelain"]).unwrap();
            let target = RemoteTarget::load(&root, "development").unwrap();
            assert!(matches!(
                target.spec.ownership,
                Ownership::Gitops {
                    write_policy: WritePolicy::Worktree,
                    ..
                }
            ));
            assert_eq!(git(&root, &["status", "--porcelain"]).unwrap(), before);
            fs::write(
                root.join(".hops/remotes/development.yaml"),
                format!("{contents}\n# changed\n"),
            )
            .unwrap();
            assert!(RemoteTarget::load(&root, "development")
                .unwrap_err()
                .to_string()
                .contains("committed"));
            fs::write(root.join(".hops/remotes/development.yaml"), &contents).unwrap();
            git(&root, &["switch", "-c", "wrong-branch"]).unwrap();
            assert!(RemoteTarget::load(&root, "development")
                .unwrap_err()
                .to_string()
                .contains("branch"));
            git(&root, &["switch", "main"]).unwrap();
            git(
                &root,
                &[
                    "remote",
                    "set-url",
                    "origin",
                    "https://github.com/other/repo.git",
                ],
            )
            .unwrap();
            assert!(RemoteTarget::load(&root, "development")
                .unwrap_err()
                .to_string()
                .contains("repository"));
            fs::remove_dir_all(root).unwrap();
        }
    }
}
