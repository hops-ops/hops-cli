//! CLI-owned local Cluster template.
//!
//! `hops local up` materializes this embed to `$HOME/.gitops/local/cluster`
//! (the path `Cluster.spec.mountRoot: $HOME` + `manifests.path:
//! .gitops/local/cluster` already resolves to). Project
//! `.gitops/local/cluster/` extras overlay on top. `shared/` is never copied
//! into Cluster manifests.

use super::definition::{
    CLUSTER_MANIFESTS_PATH, DEFAULT_CROSSPLANE_CHART, DEFAULT_CROSSPLANE_VERSION,
    DEFAULT_LOCAL_DOMAIN,
};
use super::machine::DEFAULT_MACHINE_CLUSTER_NAME;
use serde_yaml::Value;
use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const MANAGED_MARKER: &str = ".hops-managed";

macro_rules! cluster_file {
    ($path:literal) => {
        (
            $path,
            include_str!(concat!("../../../../templates/local/cluster/", $path)),
        )
    };
}

pub const FILES: &[(&str, &str)] = &[
    cluster_file!("README.md"),
    cluster_file!("SECRETS.md"),
    cluster_file!("configurations/auth-stack.yaml"),
    cluster_file!("configurations/gateway-api-stack.yaml"),
    cluster_file!("configurations/istio-stack.yaml"),
    cluster_file!("configurations/psql-stack.yaml"),
    cluster_file!("configurations/secret-stack.yaml"),
    cluster_file!("identity/features.yaml"),
    cluster_file!("identity/machine-users.yaml"),
    cluster_file!("identity/personas.yaml"),
    cluster_file!("identity/project.yaml"),
    cluster_file!("identity/secret-outputs.yaml"),
    cluster_file!("identity/smtp.yaml"),
    cluster_file!("providerconfigs/helm.yaml"),
    cluster_file!("providerconfigs/kubernetes.yaml"),
    cluster_file!("providerconfigs/zitadel.yaml"),
    cluster_file!("providers/00-namespaces.yaml"),
    cluster_file!("providers/helm-drc.yaml"),
    cluster_file!("providers/helm.yaml"),
    cluster_file!("providers/kubernetes-drc.yaml"),
    cluster_file!("providers/kubernetes.yaml"),
    cluster_file!("providers/zitadel.yaml"),
    cluster_file!("secrets/stack.yaml"),
    cluster_file!("secrets/vault-auth-delegator.yaml"),
    cluster_file!("stacks/auth.yaml"),
    cluster_file!("stacks/gateway-api.yaml"),
    cluster_file!("stacks/istio-gateway-defaults.yaml"),
    cluster_file!("stacks/istio.yaml"),
    cluster_file!("stacks/psql.yaml"),
];

/// Materialize the CLI template plus optional project overlay.
///
/// Returns the Cluster document path (`$HOME/.gitops/local/cluster.yaml`).
pub fn materialize(
    home: &Path,
    overlay_cluster_yaml: Option<&Path>,
    machine_name: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    let home = home.canonicalize().map_err(|error| {
        format!(
            "unable to canonicalize HOME for cluster template {}: {error}",
            home.display()
        )
    })?;
    let gitops_local = home.join(".gitops/local");
    let manifests = gitops_local.join("cluster");
    let yaml_path = gitops_local.join("cluster.yaml");
    prepare_manifests_dir(&manifests)?;
    write_embed(&manifests)?;
    if let Some(overlay) = overlay_cluster_yaml {
        if let Some(parent) = overlay.parent() {
            let extras = parent.join("cluster");
            if extras.is_dir() {
                overlay_manifests(&extras, &manifests)?;
            }
        }
    }
    fs::write(
        manifests.join(MANAGED_MARKER),
        "Materialized by hops local up from the CLI local-cluster template.\n\
         Project extras overlay from <repo>/.gitops/local/cluster/. Do not edit in place.\n",
    )?;
    let body = cluster_document_yaml(machine_name, overlay_cluster_yaml, &home)?;
    fs::write(&yaml_path, body)?;
    Ok(yaml_path)
}

fn prepare_manifests_dir(manifests: &Path) -> Result<(), Box<dyn Error>> {
    if manifests.exists() {
        let marker = manifests.join(MANAGED_MARKER);
        if !marker.is_file() {
            return Err(format!(
                "{} exists and is not hops-managed; move it aside before `hops local up` (expected {MANAGED_MARKER})",
                manifests.display()
            )
            .into());
        }
        fs::remove_dir_all(manifests).map_err(|error| {
            format!(
                "unable to refresh cluster template {}: {error}",
                manifests.display()
            )
        })?;
    }
    fs::create_dir_all(manifests)?;
    Ok(())
}

fn write_embed(manifests: &Path) -> Result<(), Box<dyn Error>> {
    for (relative, contents) in FILES {
        let dest = manifests.join(relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, contents)
            .map_err(|error| format!("write cluster template {}: {error}", dest.display()))?;
    }
    Ok(())
}

fn overlay_manifests(src: &Path, dest: &Path) -> Result<(), Box<dyn Error>> {
    overlay_walk(src, src, dest)
}

fn overlay_walk(root: &Path, dir: &Path, dest: &Path) -> Result<(), Box<dyn Error>> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in read {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        if should_skip_overlay(rel) {
            continue;
        }
        if path.is_dir() {
            overlay_walk(root, &path, dest)?;
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let target = dest.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&path, &target).map_err(|error| {
            format!(
                "overlay {} onto {}: {error}",
                path.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

pub fn should_skip_overlay(relative: &Path) -> bool {
    let mut components = relative.components();
    match components.next() {
        Some(Component::Normal(first)) if first == "shared" => true,
        Some(Component::Normal(first))
            if first == MANAGED_MARKER && components.next().is_none() =>
        {
            true
        }
        _ => false,
    }
}

fn cluster_document_yaml(
    machine_name: &str,
    overlay_cluster_yaml: Option<&Path>,
    home: &Path,
) -> Result<String, Box<dyn Error>> {
    let name = if machine_name.trim().is_empty() {
        DEFAULT_MACHINE_CLUSTER_NAME
    } else {
        machine_name
    };
    let overlay = overlay_cluster_yaml
        .map(load_overlay_spec)
        .transpose()?
        .unwrap_or_default();
    let domain = overlay
        .local_domain
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_LOCAL_DOMAIN);
    let mut body = format!(
        r#"apiVersion: hops.local/v1alpha1
kind: Cluster
metadata:
  name: {name}
spec:
  clusterProvider: kind
  dockerProvider: dory
  mountRoot: $HOME
  manifests:
    path: {manifests}
  controlPlane:
    crossplane:
      chart: {chart}
      version: "{version}"
  localDomain: {domain}
"#,
        manifests = CLUSTER_MANIFESTS_PATH,
        chart = DEFAULT_CROSSPLANE_CHART,
        version = DEFAULT_CROSSPLANE_VERSION,
    );
    if !overlay.browser_ingress_namespaces.is_empty() {
        body.push_str("  browserIngress:\n    namespaces:\n");
        for namespace in &overlay.browser_ingress_namespaces {
            body.push_str(&format!("      - {namespace}\n"));
        }
    }
    if let Some(secret) = overlay.secret_sync_path.as_ref() {
        if let Some(relative) = rewrite_secret_sync(overlay_cluster_yaml, secret, home) {
            body.push_str(&format!(
                "  secretSync:\n    path: {}\n",
                relative.display()
            ));
        }
    }
    Ok(body)
}

#[derive(Default)]
struct OverlaySpec {
    local_domain: Option<String>,
    browser_ingress_namespaces: Vec<String>,
    secret_sync_path: Option<PathBuf>,
}

fn load_overlay_spec(path: &Path) -> Result<OverlaySpec, Box<dyn Error>> {
    let raw = fs::read_to_string(path)
        .map_err(|error| format!("read overlay {}: {error}", path.display()))?;
    let value: Value = serde_yaml::from_str(&raw)
        .map_err(|error| format!("parse overlay {}: {error}", path.display()))?;
    let spec = value.get("spec").cloned().unwrap_or(Value::Null);
    let local_domain = spec
        .get("localDomain")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let browser_ingress_namespaces = spec
        .get("browserIngress")
        .and_then(|ingress| ingress.get("namespaces"))
        .and_then(Value::as_sequence)
        .map(|namespaces| {
            namespaces
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let secret_sync_path = spec
        .get("secretSync")
        .and_then(|secret| secret.get("path"))
        .and_then(Value::as_str)
        .map(PathBuf::from);
    Ok(OverlaySpec {
        local_domain,
        browser_ingress_namespaces,
        secret_sync_path,
    })
}

fn rewrite_secret_sync(overlay_yaml: Option<&Path>, secret: &Path, home: &Path) -> Option<PathBuf> {
    if secret.is_absolute() {
        return pathdiff(secret, home);
    }
    let overlay = overlay_yaml?;
    let checkout = overlay.ancestors().nth(3)?;
    let resolved = checkout.join(secret);
    pathdiff(&resolved, home)
}

fn pathdiff(path: &Path, base: &Path) -> Option<PathBuf> {
    let path = path
        .canonicalize()
        .ok()
        .unwrap_or_else(|| path.to_path_buf());
    let base = base
        .canonicalize()
        .ok()
        .unwrap_or_else(|| base.to_path_buf());
    path.strip_prefix(&base).ok().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "hops-cluster-template-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn embed_has_providers_and_no_shared() {
        assert!(FILES.iter().any(|(path, _)| *path == "providers/helm.yaml"));
        assert!(FILES
            .iter()
            .any(|(path, contents)| *path == "providers/helm.yaml"
                && contents.contains("provider-helm:v1.3.0")));
        assert!(FILES
            .iter()
            .any(|(path, _)| *path == "providerconfigs/kubernetes.yaml"));
        assert!(FILES.iter().any(|(path, _)| *path == "stacks/auth.yaml"));
        assert!(!FILES.iter().any(|(path, _)| path.starts_with("shared/")));
    }

    #[test]
    fn materialize_writes_profile_and_skips_shared_overlay() {
        let home = temp_home();
        let project = home.join("project");
        fs::create_dir_all(project.join(".gitops/local/cluster/shared")).unwrap();
        fs::create_dir_all(project.join(".gitops/local/cluster/extra")).unwrap();
        fs::write(
            project.join(".gitops/local/cluster.yaml"),
            r#"apiVersion: hops.local/v1alpha1
kind: Cluster
metadata:
  name: harmony
spec:
  clusterProvider: kind
  dockerProvider: dory
  mountRoot: $HOME
  localDomain: gitkb.localhost
  browserIngress:
    namespaces:
      - harmony-auth
  manifests:
    path: .gitops/local/cluster
"#,
        )
        .unwrap();
        fs::write(
            project.join(".gitops/local/cluster/extra/addon.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: extra\n",
        )
        .unwrap();
        fs::write(
            project.join(".gitops/local/cluster/shared/minio.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: minio-should-not-land\n",
        )
        .unwrap();

        let yaml = materialize(
            &home,
            Some(&project.join(".gitops/local/cluster.yaml")),
            "hops",
        )
        .unwrap();
        assert_eq!(yaml, home.join(".gitops/local/cluster.yaml"));
        let body = fs::read_to_string(&yaml).unwrap();
        assert!(body.contains("name: hops"), "{body}");
        assert!(!body.contains("name: harmony"), "{body}");
        assert!(body.contains("localDomain: gitkb.localhost"), "{body}");
        assert!(body.contains("harmony-auth"), "{body}");
        assert!(body.contains("mountRoot: $HOME"));
        let manifests = home.join(".gitops/local/cluster");
        assert!(manifests.join(MANAGED_MARKER).is_file());
        assert!(manifests.join("providers/helm.yaml").is_file());
        assert!(manifests.join("extra/addon.yaml").is_file());
        assert!(!manifests.join("shared/minio.yaml").exists());
        cleanup(&home);
    }

    #[test]
    fn refuses_unmanaged_existing_cluster_dir() {
        let home = temp_home();
        let manifests = home.join(".gitops/local/cluster");
        fs::create_dir_all(&manifests).unwrap();
        fs::write(manifests.join("stray.yaml"), "kind: ConfigMap\n").unwrap();
        let err = materialize(&home, None, "hops").unwrap_err();
        assert!(err.to_string().contains("not hops-managed"), "{err}");
        cleanup(&home);
    }

    #[test]
    fn skip_overlay_shared_and_marker() {
        assert!(should_skip_overlay(Path::new("shared/minio.yaml")));
        assert!(should_skip_overlay(Path::new("shared")));
        assert!(should_skip_overlay(Path::new(MANAGED_MARKER)));
        assert!(!should_skip_overlay(Path::new("extra/addon.yaml")));
        assert!(!should_skip_overlay(Path::new("providers/helm.yaml")));
    }
}
