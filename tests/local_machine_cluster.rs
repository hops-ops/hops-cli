#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

const FAKE_TOOL: &str = r#"#!/bin/sh
tool=${0##*/}
printf '%s %s\n' "$tool" "$*" >> "$HOPS_TEST_COMMAND_LOG"

case "$tool" in
  kind)
    if test "$1" = "get" && test "$2" = "clusters"; then
      if test -f "$HOPS_TEST_KIND_CLUSTERS"; then cat "$HOPS_TEST_KIND_CLUSTERS"; fi
      exit 0
    fi
    if test "$1" = "create" && test "$2" = "cluster"; then
      name=hops
      while test "$#" -gt 0; do
        if test "$1" = "--name"; then
          shift
          name=$1
        fi
        shift
      done
      echo "$name" >> "$HOPS_TEST_KIND_CLUSTERS"
      exit 0
    fi
    exit 0
    ;;
  docker)
    if test "$1" = "info"; then echo "27.0.0"; exit 0; fi
    if test "$1" = "ps"; then exit 0; fi
    if test "$1" = "inspect"; then
      if test -f "$HOPS_TEST_KIND_CLUSTERS"; then echo true; exit 0; fi
      exit 1
    fi
    exit 0
    ;;
  helm|kubectl)
    exit 0
    ;;
esac
"#;

const CLUSTER_YAML: &str = r#"apiVersion: hops.local/v1alpha1
kind: Cluster
metadata:
  name: hops
spec:
  clusterProvider: kind
  dockerProvider: dory
  mountRoot: ../..
  manifests:
    path: .gitops/local/cluster
"#;

const LEAF_YAML: &str = r#"apiVersion: hops.local/v1alpha1
kind: Cluster
metadata:
  name: harmony
spec:
  clusterProvider: kind
  dockerProvider: dory
  mountRoot: ../..
  manifests:
    path: .gitops/local/cluster
"#;

const ENV_YAML: &str = r#"apiVersion: hops.local/v1alpha1
kind: Environment
metadata:
  name: demo
spec:
  clusterRef:
    name: hops
  root: .
  deploys: []
"#;

struct Fixture {
    root: PathBuf,
    bin: PathBuf,
    command_log: PathBuf,
    kind_clusters: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "hops-machine-cluster-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(root.join(".gitops/local/cluster")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::write(root.join(".gitops/local/cluster.yaml"), CLUSTER_YAML).unwrap();
        let bin = root.join("fake-bin");
        fs::create_dir_all(&bin).unwrap();
        for tool in ["kind", "docker", "helm", "kubectl"] {
            let path = bin.join(tool);
            fs::write(&path, FAKE_TOOL).unwrap();
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
        let root = root.canonicalize().unwrap();
        Self {
            bin: root.join("fake-bin"),
            command_log: root.join("commands.log"),
            kind_clusters: root.join("kind-clusters"),
            root,
        }
    }

    fn command(&self) -> Command {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_hops-cli"));
        command
            .current_dir(&self.root)
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("DOCKER_HOST", "unix:///contract-test.sock")
            .env("HOPS_KIND_REGISTRY_HOST_PORT", "39011")
            .env("HOPS_TEST_COMMAND_LOG", &self.command_log)
            .env("HOPS_TEST_KIND_CLUSTERS", &self.kind_clusters)
            .env_remove("HOPS_KIND_EXTRA_MOUNT");
        command
    }

    fn output(cmd: &mut Command) -> Output {
        cmd.output().expect("run hops-cli")
    }

    fn stdout_stderr(output: &Output) -> (String, String) {
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.command_log).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn local_help_lists_up_init_env_envs() {
    let fixture = Fixture::new();
    let output = Fixture::output(fixture.command().args(["local", "--help"]));
    assert!(
        output.status.success(),
        "{:?}",
        Fixture::stdout_stderr(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for needle in ["up", "init", "env", "envs", "configure", "fwd", "gitops"] {
        assert!(stdout.contains(needle), "missing {needle} in {stdout}");
    }
}

#[test]
fn init_cluster_writes_committed_files_and_not_catalog() {
    let fixture = Fixture::new();
    let dest = fixture.root.join("home/meta");
    let output = Fixture::output(
        fixture
            .command()
            .args(["local", "init", "cluster", "--path"])
            .arg(&dest),
    );
    assert!(
        output.status.success(),
        "{:?}",
        Fixture::stdout_stderr(&output)
    );
    assert!(dest.join(".gitops/local/cluster.yaml").is_file());
    assert!(dest.join(".gitops/local/cluster").is_dir());
    let yaml = fs::read_to_string(dest.join(".gitops/local/cluster.yaml")).unwrap();
    assert!(yaml.contains("name: hops"));
    assert!(!fixture.root.join("home/.hops/local/catalog").exists());
}

#[test]
fn init_platform_and_environment_write_scope() {
    let fixture = Fixture::new();
    let dest = fixture.root.join("home/app");
    let platform = Fixture::output(
        fixture
            .command()
            .args(["local", "init", "platform", "--path"])
            .arg(&dest),
    );
    assert!(
        platform.status.success(),
        "{:?}",
        Fixture::stdout_stderr(&platform)
    );
    let yaml = fs::read_to_string(dest.join(".gitops/local/platform.yaml")).unwrap();
    assert!(yaml.contains("scope: cluster"));
    assert!(dest
        .join(".gitops/local/platform/minio/Chart.yaml")
        .is_file());
    let env = Fixture::output(
        fixture
            .command()
            .args(["local", "init", "environment", "--path"])
            .arg(&dest),
    );
    assert!(env.status.success(), "{:?}", Fixture::stdout_stderr(&env));
    let env_yaml = fs::read_to_string(dest.join(".gitops/local/environment.yaml")).unwrap();
    assert!(env_yaml.contains("name: hops"));
}

#[test]
fn env_discover_catalogues_disabled_and_refuses_home() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join(".gitops/local/environment.yaml"),
        ENV_YAML,
    )
    .unwrap();
    let output = Fixture::output(fixture.command().args(["local", "env", "discover"]));
    assert!(
        output.status.success(),
        "{:?}",
        Fixture::stdout_stderr(&output)
    );
    let catalog = fixture.root.join("home/.hops/local/catalog");
    let json = fs::read_dir(&catalog)
        .unwrap()
        .find_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension().and_then(|ext| ext.to_str()) == Some("json")).then_some(path)
        })
        .unwrap();
    let body = fs::read_to_string(json).unwrap();
    assert!(body.contains("\"enabled\": false"));
    let home = Fixture::output(
        fixture
            .command()
            .args(["local", "env", "discover"])
            .arg(fixture.root.join("home")),
    );
    assert!(!home.status.success());
    let stderr = String::from_utf8_lossy(&home.stderr);
    assert!(
        stderr.contains("refusing to crawl $HOME") || stderr.contains("HOME"),
        "{stderr}"
    );
}

#[test]
fn envs_once_lists_catalog() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join(".gitops/local/environment.yaml"),
        ENV_YAML,
    )
    .unwrap();
    assert!(
        Fixture::output(fixture.command().args(["local", "env", "discover"]))
            .status
            .success()
    );
    let output = Fixture::output(fixture.command().args(["local", "envs", "--once"]));
    assert!(
        output.status.success(),
        "{:?}",
        Fixture::stdout_stderr(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.trim().is_empty(), "{stdout}");
}

#[test]
fn disable_after_cluster_reset_clears_selection_without_inferred_deletion() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join(".gitops/local/environment.yaml"),
        ENV_YAML,
    )
    .unwrap();
    assert!(
        Fixture::output(fixture.command().args(["local", "env", "discover"]))
            .status
            .success()
    );
    let catalog = fixture.root.join("home/.hops/local/catalog");
    let entry_path = fs::read_dir(&catalog)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut entry: serde_json::Value =
        serde_json::from_slice(&fs::read(&entry_path).unwrap()).unwrap();
    entry["enabled"] = true.into();
    let runtime_name = entry["runtimeName"].as_str().unwrap().to_string();
    fs::write(&entry_path, serde_json::to_vec_pretty(&entry).unwrap()).unwrap();

    let output =
        Fixture::output(
            fixture
                .command()
                .args(["local", "env", "disable", &runtime_name]),
        );
    assert!(
        output.status.success(),
        "{:?}",
        Fixture::stdout_stderr(&output)
    );
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&entry_path).unwrap()).unwrap();
    assert_eq!(saved["enabled"], false);
    assert!(!fixture.log().contains("kubectl --context"));
}

#[test]
fn disable_uses_the_context_in_its_exact_ownership_snapshot() {
    let fixture = Fixture::new();
    let state = fixture.root.join("home/.hops/local");
    fs::create_dir_all(state.join("catalog")).unwrap();
    fs::create_dir_all(state.join("envs")).unwrap();
    fs::create_dir_all(state.join("clusters/hops/environments")).unwrap();
    fs::write(
        state.join("catalog/demo.json"),
        serde_json::to_vec(&serde_json::json!({
            "id": "demo", "name": "demo", "runtimeName": "demo",
            "source": fixture.root.join(".gitops/local/environment.yaml"),
            "enabled": true
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        state.join("envs/demo.json"),
        serde_json::to_vec(&serde_json::json!({
            "name": "demo", "namespace": "demo", "envPath": "/project/environment.yaml",
            "clusterName": "hops", "kubeContext": "kind-hops"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        state.join("clusters/hops/environments/demo.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1, "clusterName": "hops", "kubeContext": "kind-hops",
            "name": "demo", "namespace": "demo", "sourcePath": "/project/environment.yaml",
            "root": "/project", "namespaceExclusive": false,
            "deploys": [{
                "sourceRoot": "/project", "sourcePath": "/project/app/.gitops/local",
                "appName": "app", "objects": [{
                    "apiVersion": "v1", "kind": "ConfigMap", "namespace": "demo", "name": "owned"
                }]
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Fixture::output(
        fixture
            .command()
            .env("HOPS_KUBE_CONTEXT", "kind-harmony")
            .args(["local", "env", "disable", "demo"]),
    );
    assert!(
        output.status.success(),
        "{:?}",
        Fixture::stdout_stderr(&output)
    );
    let log = fixture.log();
    assert!(
        log.contains("kubectl --context kind-hops delete configmap owned --namespace demo"),
        "{log}"
    );
    assert!(!log.contains("kind-harmony delete"), "{log}");
    assert!(!state.join("clusters/hops/environments/demo.json").exists());
    assert!(!state.join("envs/demo.json").exists());
    let catalog: serde_json::Value =
        serde_json::from_slice(&fs::read(state.join("catalog/demo.json")).unwrap()).unwrap();
    assert_eq!(catalog["enabled"], false);
}

#[test]
fn up_from_leaf_yaml_does_not_create_second_cluster() {
    let fixture = Fixture::new();
    let first = Fixture::output(
        fixture
            .command()
            .args(["local", "up", "--once", "--dry-run"]),
    );
    assert!(
        first.status.success(),
        "{:?}",
        Fixture::stdout_stderr(&first)
    );
    fs::write(fixture.root.join(".gitops/local/cluster.yaml"), LEAF_YAML).unwrap();
    let second = Fixture::output(
        fixture
            .command()
            .args(["local", "up", "--once", "--dry-run"]),
    );
    let (_out, err) = Fixture::stdout_stderr(&second);
    assert!(second.status.success(), "{err}");
    assert!(
        err.contains("harmony") && err.contains("hops"),
        "expected leaf-name warning, got {err}"
    );
    let creates = fixture
        .log()
        .lines()
        .filter(|line| line.contains("kind create"))
        .count();
    assert_eq!(
        creates,
        0,
        "dry-run must not kind create: {}",
        fixture.log()
    );
    assert!(
        fixture
            .log()
            .lines()
            .any(|line| line.starts_with("kubectl --context kind-hops ")),
        "Cluster dry-run must target its selected context: {}",
        fixture.log()
    );
}

#[test]
fn env_discover_keeps_worktrees_distinct() {
    let fixture = Fixture::new();
    let main = fixture.root.join(".gitops/local/environment.yaml");
    let wt = fixture
        .root
        .join(".worktrees/feature-auth/.gitops/local/environment.yaml");
    fs::create_dir_all(wt.parent().unwrap()).unwrap();
    fs::write(&main, ENV_YAML).unwrap();
    fs::write(&wt, ENV_YAML).unwrap();
    let output = Fixture::output(fixture.command().args(["local", "env", "discover"]));
    assert!(
        output.status.success(),
        "{:?}",
        Fixture::stdout_stderr(&output)
    );
    let catalog = fixture.root.join("home/.hops/local/catalog");
    let files: Vec<_> = fs::read_dir(&catalog)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension().and_then(|ext| ext.to_str()) == Some("json")).then_some(path)
        })
        .collect();
    assert_eq!(
        files.len(),
        2,
        "worktree and main must not share a catalog file"
    );
    let list = Fixture::output(fixture.command().args(["local", "env", "list"]));
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("feature-auth"), "{stdout}");
}

#[test]
fn up_materializes_cli_template_and_skips_shared_overlay() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join(".gitops/local/cluster/shared")).unwrap();
    fs::write(
        fixture.root.join(".gitops/local/cluster/extra.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: extra\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join(".gitops/local/cluster/shared/minio.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: minio-should-not-land\n",
    )
    .unwrap();
    let output = Fixture::output(
        fixture
            .command()
            .args(["local", "up", "--once", "--dry-run"]),
    );
    assert!(
        output.status.success(),
        "{:?}",
        Fixture::stdout_stderr(&output)
    );
    let profile = fixture.root.join("home/.gitops/local/cluster");
    assert!(
        profile.join("providers/helm.yaml").is_file(),
        "CLI template providers must land in $HOME/.gitops/local/cluster"
    );
    let helm = fs::read_to_string(profile.join("providers/helm.yaml")).unwrap();
    assert!(helm.contains("provider-helm:v1.3.0"), "{helm}");
    assert!(profile.join("extra.yaml").is_file());
    assert!(!profile.join("shared/minio.yaml").exists());
    let yaml = fs::read_to_string(fixture.root.join("home/.gitops/local/cluster.yaml")).unwrap();
    assert!(yaml.contains("name: hops"), "{yaml}");
    assert!(yaml.contains("mountRoot: $HOME"), "{yaml}");
}

#[test]
fn env_discover_finds_cluster_scoped_extra_yaml() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join(".gitops/local/environment.yaml"),
        ENV_YAML,
    )
    .unwrap();
    fs::write(
        fixture.root.join(".gitops/local/harmony-system.yaml"),
        r#"apiVersion: hops.local/v1alpha1
kind: Environment
metadata:
  name: harmony-system
spec:
  scope: cluster
  clusterRef:
    name: hops
  namespace: harmony-system
  root: .
  deploys: []
"#,
    )
    .unwrap();
    let output = Fixture::output(fixture.command().args(["local", "env", "discover"]));
    assert!(
        output.status.success(),
        "{:?}",
        Fixture::stdout_stderr(&output)
    );
    let list = Fixture::output(fixture.command().args(["local", "env", "list"]));
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("harmony-system"), "{stdout}");
    assert!(
        stdout.contains("demo") || stdout.contains("off"),
        "{stdout}"
    );
}

#[test]
fn cluster_name_escape_hatch_warns() {
    let fixture = Fixture::new();
    let output = Fixture::output(fixture.command().args([
        "local",
        "up",
        "--once",
        "--dry-run",
        "--cluster-name",
        "hops",
    ]));
    let (_out, err) = Fixture::stdout_stderr(&output);
    assert!(output.status.success(), "{err}");
    assert!(err.contains("escape hatch"), "{err}");
}

#[test]
fn status_json_reports_effective_cluster_and_workspace_health() {
    let fixture = Fixture::new();
    let state = fixture.root.join("home/.hops/local");
    fs::create_dir_all(state.join("envs")).unwrap();
    let output = Fixture::output(
        fixture
            .command()
            .args(["local", "status", "--all", "--json"]),
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["cluster"]["state"], "absent_record");
    fs::write(state.join("cluster.json"), r#"{"name":"hops","kubeContext":"kind-hops","source":"/fixture/cluster.yaml","hostPath":"/fixture","localDomain":"gitkb.localhost"}"#).unwrap();
    fs::write(state.join("envs/demo.json"), r#"{"name":"demo","namespace":"demo","envPath":"/fixture/env.yaml","projectRoot":"/fixture","clusterName":"hops","kubeContext":"kind-hops"}"#).unwrap();
    for (mode, expected) in [
        ("ready", "ready"),
        ("degraded", "degraded"),
        ("down", "unreachable"),
        ("forbidden", "forbidden"),
        ("unavailable", "unavailable"),
        ("empty", "not_found"),
        ("missing", "missing_context"),
    ] {
        let tool = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOPS_TEST_COMMAND_LOG"
case "$*" in
  *get-contexts*) test '{mode}' = missing || echo kind-hops; exit 0;;
esac
test '{mode}' = down && exit 1
if test '{mode}' = forbidden; then echo 'Error from server (Forbidden): private details' >&2; exit 1; fi
if test '{mode}' = unavailable; then echo "error: the server doesn't have a resource type" >&2; exit 1; fi
if test '{mode}' = empty; then echo '{{"items":[]}}'; exit 0; fi
if test '{mode}' = degraded; then status=False; else status=True; fi
printf '{{"items":[{{"status":{{"conditions":[{{"type":"Ready","status":"%s"}},{{"type":"Healthy","status":"True"}},{{"type":"Installed","status":"True"}}]}}}}]}}' "$status"
"#
        );
        fs::write(fixture.bin.join("kubectl"), tool).unwrap();
        let output = Fixture::output(
            fixture
                .command()
                .args(["local", "status", "--all", "--json"])
                .env("HOPS_KUBE_CONTEXT", "must-not-use"),
        );
        assert!(
            output.status.success(),
            "{:?}",
            Fixture::stdout_stderr(&output)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["cluster"]["state"], expected);
        assert_eq!(value["workspaces"][0]["health"]["state"], expected);
    }
    let log = fixture.log();
    assert!(!log.contains("must-not-use"));
    assert!(!log.contains("apply") && !log.contains("delete"));
}

#[test]
fn catalog_json_is_sorted_read_only_and_redacts_stored_errors() {
    let fixture = Fixture::new();
    let catalog = fixture.root.join("home/.hops/local/catalog");
    fs::create_dir_all(&catalog).unwrap();
    for (id, enabled) in [("b", true), ("a", false)] {
        fs::write(
            catalog.join(format!("{id}.json")),
            serde_json::json!({
                "id": id, "name": "same-template", "runtimeName": id,
                "source": format!("/fixture/{id}/env.yaml"), "enabled": enabled,
                "lastError": "token=SECRET"
            })
            .to_string(),
        )
        .unwrap();
    }
    let output = Fixture::output(fixture.command().args(["local", "env", "list", "--json"]));
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["entries"][0]["id"], "a");
    assert_eq!(value["entries"][0]["enabled"], false);
    assert_eq!(value["entries"][1]["enabled"], true);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET"));
    assert!(fixture.log().is_empty());
}

#[test]
fn status_json_rejects_unknown_workspace_before_queries() {
    let fixture = Fixture::new();
    for check in [false, true] {
        let mut command = fixture.command();
        command.args(["local", "status", "--name", "unknown", "--json"]);
        if check {
            command.arg("--check");
        }
        let output = Fixture::output(&mut command);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("Workspace `unknown` is not registered."));
        assert!(fixture.log().is_empty());
    }
}
