#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const VALID_DEFINITION: &str = r#"apiVersion: hops.local/v1alpha1
kind: Cluster
metadata:
  name: project-dev
spec:
  clusterProvider: kind
  dockerProvider: dory
  mountRoot: ../..
  manifests:
    path: .gitops/local/cluster
"#;

const DEFINITION_PATH: &str = ".gitops/local/cluster.yaml";

const ENVIRONMENT_DEFINITION: &str = r#"apiVersion: hops.local/v1alpha1
kind: Environment
metadata:
  name: local
spec:
  clusterRef:
    name: project-dev
  root: .
  deploys:
    - path: apps/gateway
"#;

const FAKE_TOOL: &str = r#"#!/bin/sh
tool=${0##*/}
printf '%s %s\n' "$tool" "$*" >> "$HOPS_TEST_COMMAND_LOG"

case "$tool" in
  kind)
    if test "$1" = "--version" || test "$1" = "version"; then
      echo "kind v0.32.0 go1.24 darwin/arm64"
      exit 0
    fi
    if test "$1" = "get" && test "$2" = "clusters"; then
      if test -f "$HOPS_TEST_CLUSTER_EXISTS"; then echo project-dev; fi
      exit 0
    fi
    if test "$1" = "create" && test "$2" = "cluster"; then
      while IFS= read -r line; do
        printf 'kind-config %s\n' "$line" >> "$HOPS_TEST_COMMAND_LOG"
      done
      touch "$HOPS_TEST_CLUSTER_EXISTS"
      exit 0
    fi
    exit 0
    ;;
  docker)
    if test "$1" = "info"; then echo "27.0.0"; exit 0; fi
    if test "$1" = "ps"; then exit 0; fi
    if test "$1" = "pull"; then exit 0; fi
    if test "$1" = "volume"; then exit 0; fi
    if test "$1" = "inspect"; then
      case "$*" in
        *'{{json .Mounts}}'*)
          if test -n "$HOPS_TEST_MOUNTS"; then
            printf '%s\n' "$HOPS_TEST_MOUNTS"
          else
            printf '[{"Source":"%s","Destination":"%s","RW":true}]\n' \
              "$HOPS_TEST_EXPECTED_MOUNT" "$HOPS_TEST_EXPECTED_MOUNT"
          fi
          exit 0
          ;;
        *'.State.Running'*)
          if test -f "$HOPS_TEST_CLUSTER_EXISTS"; then echo true; exit 0; fi
          exit 1
          ;;
        *)
          if test -f "$HOPS_TEST_CLUSTER_EXISTS"; then echo fake-id; exit 0; fi
          exit 1
          ;;
      esac
    fi
    if test "$1" = "exec"; then cat >/dev/null; exit 0; fi
    if test "$1" = "start" || test "$1" = "stop"; then exit 0; fi
    exit 0
    ;;
  kubectl)
    if test "$1" = "config" && test "$2" = "get-contexts"; then
      echo kind-project-dev
      exit 0
    fi
    case "$*" in
      *availableReplicas*) echo 1 ;;
      *status.conditions*) echo True ;;
      *'get svc registry '*'spec.clusterIP'*) echo 10.96.0.50 ;;
    esac
    exit 0
    ;;
esac
"#;

struct Fixture {
    root: PathBuf,
    bin: PathBuf,
    command_log: PathBuf,
    cluster_exists: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "hops-local-cluster-contract-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(root.join(".gitops/local/cluster")).unwrap();
        fs::create_dir_all(root.join("apps/gateway")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        let bin = root.join("fake-bin");
        fs::create_dir_all(&bin).unwrap();
        for tool in ["kind", "docker", "kubectl"] {
            write_executable(&bin.join(tool), FAKE_TOOL);
        }
        fs::write(root.join(DEFINITION_PATH), VALID_DEFINITION).unwrap();
        let root = root.canonicalize().unwrap();
        Self {
            bin: root.join("fake-bin"),
            command_log: root.join("commands.log"),
            cluster_exists: root.join("cluster-exists"),
            root,
        }
    }

    fn write_definition(&self, yaml: &str) {
        fs::write(self.root.join(DEFINITION_PATH), yaml).unwrap();
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
            .args(["local", "gitops", "cluster", DEFINITION_PATH, "--once"])
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("DOCKER_HOST", "unix:///contract-test.sock")
            .env("HOPS_KIND_REGISTRY_HOST_PORT", "39001")
            .env("HOPS_TEST_COMMAND_LOG", &self.command_log)
            .env("HOPS_TEST_CLUSTER_EXISTS", &self.cluster_exists)
            .env("HOPS_TEST_EXPECTED_MOUNT", &self.root)
            .env_remove("HOPS_KIND_EXTRA_MOUNT");
        command
    }

    fn run(&self) -> Output {
        self.command().output().unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.command_log).unwrap_or_default()
    }

    fn clear_log(&self) {
        match fs::remove_file(&self.command_log) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("clear command log: {error}"),
        }
    }

    fn assert_no_mutation(&self) {
        assert!(
            self.log().is_empty(),
            "external command inventory was not empty"
        );
        assert!(!self.cluster_exists.exists(), "cluster marker was created");
        assert!(
            !self.root.join("home/.hops/local").exists(),
            "local provider state was written"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn parses_cluster_only() {
    let fixture = Fixture::new();
    let first = fixture.run();
    assert!(first.status.success(), "{}", output_text(&first));
    let first_log = fixture.log();
    assert!(first_log.contains("kind create cluster --name project-dev --config -"));
    assert!(first_log.contains(&format!("hostPath: \"{}\"", fixture.root.display())));
    let text = output_text(&first);
    assert!(text.contains("Cluster 'project-dev' selected"), "{text}");
    let provider_state = fs::read_to_string(fixture.root.join("home/.hops/local/providers.json"))
        .expect("successful up persists provider identity");
    assert!(provider_state.contains(r#""clusterProvider": "kind""#));
    assert!(provider_state.contains(r#""dockerProvider": "dory""#));
    assert!(provider_state.contains(r#""clusterName": "project-dev""#));

    fixture.clear_log();
    let second = fixture.run();
    assert!(second.status.success(), "{}", output_text(&second));
    let second_log = fixture.log();
    assert!(!second_log.contains("kind create cluster"));
    assert!(!second_log.contains("kind delete cluster"));
    assert!(!second_log.contains("docker start"));
}

#[test]
fn rejects_zero_or_multiple_clusters() {
    let fixture = Fixture::new();
    fixture.write_definition("\n");
    let zero = fixture.run();
    assert!(!zero.status.success());
    assert!(output_text(&zero).contains("exactly one"));
    fixture.assert_no_mutation();

    fixture.write_definition(&format!(
        "{}\n---\n{}\n",
        VALID_DEFINITION, VALID_DEFINITION
    ));
    let multiple = fixture.run();
    assert!(!multiple.status.success());
    assert!(output_text(&multiple).contains("found 2"));
    fixture.assert_no_mutation();
}

#[test]
fn rejects_embedded_environment_before_mutation() {
    let fixture = Fixture::new();
    fixture.write_definition(&format!(
        "{}\n---\n{}",
        VALID_DEFINITION, ENVIRONMENT_DEFINITION
    ));
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(output_text(&output).contains("must not be committed"));
    fixture.assert_no_mutation();
}

#[test]
fn rejects_unknown_fields_and_escaping_paths_before_mutation() {
    let fixture = Fixture::new();
    let unknown = VALID_DEFINITION.replacen(
        "  mountRoot: ../..",
        "  mountRoot: ../..\n  unexpectedField: true",
        1,
    );
    fixture.write_definition(&unknown);
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(output_text(&output).contains("unknown field"));
    fixture.assert_no_mutation();

    let escape = VALID_DEFINITION.replacen("mountRoot: ../..", "mountRoot: /tmp/outside", 1);
    fixture.write_definition(&escape);
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(output_text(&output).contains("must be relative"));
    fixture.assert_no_mutation();
}

#[test]
fn provider_mount_matrix() {
    let fixture = Fixture::new();
    let matching = fixture
        .command()
        .args([
            "--cluster-provider",
            "kind",
            "--docker-provider",
            "dory",
            "--cluster-name",
            "project-dev",
            "--context",
            "kind-project-dev",
        ])
        .output()
        .unwrap();
    assert!(matching.status.success(), "{}", output_text(&matching));

    fixture.clear_log();
    let conflicting = fixture
        .command()
        .args(["--docker-provider", "colima"])
        .output()
        .unwrap();
    assert!(!conflicting.status.success());
    assert!(output_text(&conflicting).contains("conflicts"));
    assert!(fixture.log().is_empty());

    #[cfg(target_os = "macos")]
    {
        let legacy = fixture
            .command()
            .args(["--backend", "kind"])
            .output()
            .unwrap();
        assert!(legacy.status.success(), "{}", output_text(&legacy));
        assert!(output_text(&legacy).contains("--backend is deprecated"));
    }
}

#[test]
fn mount_drift_is_non_destructive() {
    let fixture = Fixture::new();
    fs::write(&fixture.cluster_exists, "existing").unwrap();
    let output = fixture
        .command()
        .env(
            "HOPS_TEST_MOUNTS",
            r#"[{"Source":"/different","Destination":"/different","RW":true}]"#,
        )
        .output()
        .unwrap();
    let text = output_text(&output);
    assert!(!output.status.success());
    assert!(text.contains("different or missing mountRoot"), "{text}");
    assert!(text.contains("No resources were deleted"), "{text}");
    let log = fixture.log();
    assert!(!log.contains("kind create cluster"));
    assert!(!log.contains("kind delete cluster"));
    assert!(!log.contains("docker start"));
}

#[test]
fn cluster_down_stops_the_declared_node_without_destroying_it() {
    let fixture = Fixture::new();
    fs::write(&fixture.cluster_exists, "existing").unwrap();

    let output = fixture.command().arg("--down").output().unwrap();

    assert!(output.status.success(), "{}", output_text(&output));
    let log = fixture.log();
    assert!(
        log.contains("docker stop project-dev-control-plane"),
        "{log}"
    );
    assert!(!log.contains("kind delete cluster"), "{log}");
    assert!(!log.contains("docker volume rm"), "{log}");
}
