#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const FAKE_KUBECTL: &str = r#"#!/bin/sh
case "$*" in
  *'get pods -n feature -o json')
    printf '%s\n' '{"items":[{"metadata":{"name":"app-1"},"status":{"phase":"Running","containerStatuses":[{"ready":true}]}}]}'
    ;;
  *'get httproute -n feature -o json')
    printf '%s\n' '{"items":[]}'
    ;;
  *)
    printf 'unexpected kubectl arguments: %s\n' "$*" >&2
    exit 1
    ;;
esac
"#;

#[test]
fn status_observes_without_persisting_or_healing_local_state() {
    let root = std::env::temp_dir().join(format!(
        "hops-local-status-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let home = root.join("home");
    let state = home.join(".hops/local");
    let envs = state.join("envs");
    let bin = root.join("bin");
    fs::create_dir_all(&envs).unwrap();
    fs::create_dir_all(&bin).unwrap();

    let record = envs.join("feature.json");
    let record_contents = r#"{
  "name": "feature",
  "namespace": "feature",
  "envPath": "/project/.gitops/local/environment.yaml",
  "clusterName": "project-dev",
  "kubeContext": "kind-project-dev"
}"#;
    fs::write(&record, record_contents).unwrap();
    let kubectl = bin.join("kubectl");
    fs::write(&kubectl, FAKE_KUBECTL).unwrap();
    let mut permissions = fs::metadata(&kubectl).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&kubectl, permissions).unwrap();

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_hops-cli"))
        .args([
            "local",
            "status",
            "--name",
            "feature",
            "--cluster-provider",
            "kind",
            "--docker-provider",
            "dory",
            "--cluster-name",
            "project-dev",
            "--context",
            "kind-project-dev",
        ])
        .env("HOME", &home)
        .env("PATH", path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("service access: disabled"), "{stdout}");
    assert!(stdout.contains("app-1: Running 1/1  [ok]"), "{stdout}");
    assert!(
        stdout.contains("ingress:  (no HTTPRoute hostnames)"),
        "{stdout}"
    );
    assert_eq!(fs::read_to_string(&record).unwrap(), record_contents);
    assert!(!state.join("providers.json").exists());
    assert!(!state.join("backend").exists());
    assert!(!state.join("runtime").exists());

    fs::remove_dir_all(root).unwrap();
}
