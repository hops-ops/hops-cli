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
fn local_help_lists_up_init_env_tui() {
    let fixture = Fixture::new();
    let output = Fixture::output(fixture.command().args(["local", "--help"]));
    assert!(output.status.success(), "{:?}", Fixture::stdout_stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    for needle in ["up", "init", "env", "tui", "gitops"] {
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
    assert!(output.status.success(), "{:?}", Fixture::stdout_stderr(&output));
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
    assert!(platform.status.success(), "{:?}", Fixture::stdout_stderr(&platform));
    let yaml = fs::read_to_string(dest.join(".gitops/local/platform.yaml")).unwrap();
    assert!(yaml.contains("scope: cluster"));
    assert!(dest.join(".gitops/local/platform/minio/Chart.yaml").is_file());
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
    fs::write(fixture.root.join(".gitops/local/environment.yaml"), ENV_YAML).unwrap();
    let output = Fixture::output(fixture.command().args(["local", "env", "discover"]));
    assert!(output.status.success(), "{:?}", Fixture::stdout_stderr(&output));
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
fn tui_once_lists_catalog() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join(".gitops/local/environment.yaml"), ENV_YAML).unwrap();
    assert!(Fixture::output(fixture.command().args(["local", "env", "discover"]))
        .status
        .success());
    let output = Fixture::output(fixture.command().args(["local", "tui", "--once"]));
    assert!(output.status.success(), "{:?}", Fixture::stdout_stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("demo") || stdout.contains("Environments"));
}

#[test]
fn up_from_leaf_yaml_does_not_create_second_cluster() {
    let fixture = Fixture::new();
    let first = Fixture::output(fixture.command().args(["local", "up", "--once", "--dry-run"]));
    assert!(first.status.success(), "{:?}", Fixture::stdout_stderr(&first));
    fs::write(fixture.root.join(".gitops/local/cluster.yaml"), LEAF_YAML).unwrap();
    let second = Fixture::output(fixture.command().args(["local", "up", "--once", "--dry-run"]));
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
    assert_eq!(creates, 0, "dry-run must not kind create: {}", fixture.log());
}

#[test]
fn cluster_name_escape_hatch_warns() {
    let fixture = Fixture::new();
    let output = Fixture::output(
        fixture
            .command()
            .args(["local", "up", "--once", "--dry-run", "--cluster-name", "hops"]),
    );
    let (_out, err) = Fixture::stdout_stderr(&output);
    assert!(output.status.success(), "{err}");
    assert!(err.contains("escape hatch"), "{err}");
}
