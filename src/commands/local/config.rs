use super::{kubectl_apply_stdin, run_cmd, run_cmd_output};
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const REGISTRY_YAML: &str = include_str!("../../../bootstrap/registry/registry.yaml");

/// Host address for `docker push` (NodePort exposed by the in-cluster registry)
const REGISTRY_PUSH: &str = "localhost:30500";

/// Cluster-internal address used in Crossplane package references
const REGISTRY_PULL: &str = "registry.crossplane-system.svc.cluster.local:5000";

pub fn run(path: &str) -> Result<(), Box<dyn Error>> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", path).into());
    }

    ensure_registry()?;

    // Build the Crossplane package
    log::info!("Building Crossplane package in {}...", path);
    let status = Command::new("up")
        .args(["project", "build"])
        .current_dir(dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        return Err(format!("up project build exited with {}", status).into());
    }

    // Find .uppkg files in _output/
    let output_dir = dir.join("_output");
    let packages: Vec<_> = fs::read_dir(&output_dir)
        .map_err(|e| format!("Failed to read {}: {}", output_dir.display(), e))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().map_or(false, |ext| ext == "uppkg"))
        .collect();

    if packages.is_empty() {
        return Err(format!("No .uppkg files found in {}", output_dir.display()).into());
    }

    // Load each package into docker and collect image names
    let mut loaded = Vec::new();
    for pkg in &packages {
        let pkg_path = pkg.path();
        let pkg_str = pkg_path.to_string_lossy();
        log::info!("Loading {}...", pkg_str);

        let output = Command::new("docker")
            .args(["load", "-i", &*pkg_str])
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("docker load failed: {}", stderr).into());
        }

        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(img) = line.strip_prefix("Loaded image: ") {
                loaded.push(img.trim().to_string());
            }
        }
    }

    if loaded.is_empty() {
        return Err("No images were loaded from .uppkg files".into());
    }

    // Tag and push all images to the local registry.
    // Render function images are rebuilt via `docker build FROM` to fix the
    // empty rootfs.type in the OCI config (`up project build` bug).
    for img in &loaded {
        let push_ref = rewrite_registry(img, REGISTRY_PUSH);
        let (img_path, _) = split_ref(img);

        if img_path.ends_with("_render") {
            log::info!("Rebuilding {} (fix OCI config)...", push_ref);
            docker_build_from(img, &push_ref)?;
        } else {
            run_cmd("docker", &["tag", img, &push_ref])?;
        }
        log::info!("Pushing {}...", push_ref);
        run_cmd("docker", &["push", &push_ref])?;
    }

    // Install declared dependencies from upbound.yaml before the Configuration.
    // We use skipDependencyResolution on the Configuration because `up project
    // build` bakes the render function's OCI digest into the package metadata,
    // but our rootfs.type fix changes that digest. Manual dep installation
    // gives the same result without the unresolvable digest check.
    let upbound_path = dir.join("upbound.yaml");
    if upbound_path.exists() {
        let content = fs::read_to_string(&upbound_path)?;
        let deps = parse_dependencies(&content);
        for dep in &deps {
            let name = dep.package.rsplit('/').next().unwrap_or(&dep.package);
            let api_version = match dep.kind.as_str() {
                "Provider" => "pkg.crossplane.io/v1",
                "Function" => "pkg.crossplane.io/v1beta1",
                _ => continue,
            };
            let kind = &dep.kind;
            let package = &dep.package;
            log::info!("Installing dependency {} '{}'...", kind, name);
            kubectl_apply_stdin(&format!(
"apiVersion: {api_version}
kind: {kind}
metadata:
  name: {name}
spec:
  package: {package}
"
            ))?;
        }
    }

    // Apply Crossplane resources for the pushed images
    let arch = docker_arch();
    for img in &loaded {
        let pull_ref = rewrite_registry(img, REGISTRY_PULL);
        let (img_path, tag) = split_ref(img);

        if tag == "configuration" {
            let name = img_path.rsplit('/').next().unwrap_or(img_path);
            log::info!("Applying Configuration '{}'...", name);
            kubectl_apply_stdin(&format!(
"apiVersion: pkg.crossplane.io/v1
kind: Configuration
metadata:
  name: {name}
spec:
  package: {pull_ref}
  packagePullPolicy: Always
  skipDependencyResolution: true
"
            ))?;
        } else if tag == arch && img_path.ends_with("_render") {
            // Crossplane derives Function names as DNS labels from the package path:
            // strip registry, replace / with -, remove non-DNS chars (like _)
            let path = strip_registry(img_path);
            let name: String = path
                .replace('/', "-")
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            log::info!("Applying Function '{}'...", name);
            kubectl_apply_stdin(&format!(
"apiVersion: pkg.crossplane.io/v1beta1
kind: Function
metadata:
  name: {name}
spec:
  package: {pull_ref}
  packagePullPolicy: Always
"
            ))?;
        }
    }

    Ok(())
}

/// Ensure the in-cluster registry is deployed and available.
fn ensure_registry() -> Result<(), Box<dyn Error>> {
    let result = run_cmd_output(
        "kubectl",
        &[
            "get", "deployment", "registry",
            "-n", "crossplane-system",
            "-o", "jsonpath={.status.availableReplicas}",
        ],
    );

    if let Ok(replicas) = result {
        if replicas.trim() == "1" {
            return Ok(());
        }
    }

    log::info!("Deploying local package registry...");
    kubectl_apply_stdin(REGISTRY_YAML)?;

    // Wait for the registry pod to become ready
    for _ in 0..60 {
        let out = run_cmd_output(
            "kubectl",
            &[
                "get", "deployment", "registry",
                "-n", "crossplane-system",
                "-o", "jsonpath={.status.availableReplicas}",
            ],
        );
        if let Ok(r) = out {
            if r.trim() == "1" {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    Err("Timed out waiting for registry deployment".into())
}

/// A dependency declared in upbound.yaml's spec.dependsOn list.
struct Dependency {
    kind: String,
    package: String,
}

/// Parse the dependsOn list from an upbound.yaml file.
/// Extracts kind and package for each entry.
fn parse_dependencies(content: &str) -> Vec<Dependency> {
    let mut deps = Vec::new();
    let mut in_depends = false;
    let mut current_kind: Option<String> = None;
    let mut current_package: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed == "dependsOn:" {
            in_depends = true;
            continue;
        }
        if !in_depends {
            continue;
        }

        // A non-indented, non-empty line ends the dependsOn section.
        if !trimmed.is_empty() && !line.starts_with(' ') && !line.starts_with('\t') {
            break;
        }

        // New list item — flush the previous entry.
        if trimmed.starts_with("- ") {
            if let (Some(k), Some(p)) = (current_kind.take(), current_package.take()) {
                deps.push(Dependency { kind: k, package: p });
            }
            parse_dep_key(&trimmed[2..], &mut current_kind, &mut current_package);
            continue;
        }

        parse_dep_key(trimmed, &mut current_kind, &mut current_package);
    }

    // Flush last entry.
    if let (Some(k), Some(p)) = (current_kind, current_package) {
        deps.push(Dependency { kind: k, package: p });
    }

    deps
}

fn parse_dep_key(s: &str, kind: &mut Option<String>, package: &mut Option<String>) {
    if let Some(val) = s.strip_prefix("kind:") {
        *kind = Some(val.trim().to_string());
    } else if let Some(val) = s.strip_prefix("package:") {
        *package = Some(val.trim().trim_matches('\'').trim_matches('"').to_string());
    }
}

/// Replace the registry portion of an image reference.
/// "ghcr.io/hops-ops/helm-airflow:configuration" -> "<registry>/hops-ops/helm-airflow:configuration"
fn rewrite_registry(image: &str, registry: &str) -> String {
    let (path_with_reg, tag) = split_ref(image);
    let path = strip_registry(path_with_reg);
    format!("{}/{}:{}", registry, path, tag)
}

/// Strip the registry prefix from an image path.
fn strip_registry(path: &str) -> &str {
    if let Some(pos) = path.find('/') {
        let prefix = &path[..pos];
        if prefix.contains('.') || prefix.contains(':') {
            return &path[pos + 1..];
        }
    }
    path
}

/// Split "path:tag" into ("path", "tag").
fn split_ref(image: &str) -> (&str, &str) {
    image.rsplit_once(':').unwrap_or((image, "latest"))
}

/// Map Rust arch constant to Docker platform architecture name.
fn docker_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

/// Rebuild a Docker image with just `FROM <src>` to produce a valid OCI config.
/// This fixes images where rootfs.type is empty (a known issue with `up project build`
/// render function images).
fn docker_build_from(src: &str, tag: &str) -> Result<(), Box<dyn Error>> {
    let dockerfile = format!("FROM {}\n", src);
    let mut child = Command::new("docker")
        .args(["build", "-t", tag, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(dockerfile.as_bytes())?;
    }

    let status = child.wait()?;
    if !status.success() {
        return Err(format!("docker build exited with {}", status).into());
    }
    Ok(())
}
