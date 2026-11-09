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
    - path: apps/gateway/.gitops/local
      type: helm
"#;

const FAKE_TOOL: &str = r#"#!/bin/sh
tool=${0##*/}
printf '%s %s\n' "$tool" "$*" >> "$HOPS_TEST_COMMAND_LOG"

case "$tool" in
  mutagen) exit 1 ;;
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
    if test "$1" = "exec"; then
      case "$*" in *'test -d'*) if test -n "$HOPS_TEST_SOURCE_PROBE"; then exit 125; fi ;; esac
      cat >/dev/null; exit 0
    fi
    if test "$1" = "start" || test "$1" = "stop"; then exit 0; fi
    exit 0
    ;;
  helm)
    if test "$1" = "template"; then
      if test "$HOPS_TEST_SYNC_FIXTURE" = 1; then
        cat <<'YAML'
apiVersion: apps/v1
kind: Deployment
metadata: {name: source-app}
spec:
  template:
    spec:
      containers:
        - name: app
          volumeMounts: [{name: source, mountPath: /workspace}]
      volumes: [{name: source, emptyDir: {}}]
YAML
        exit 0
      fi
      values_file=
      while test "$#" -gt 0; do
        if test "$1" = "--values"; then
          shift
          values_file=$1
          break
        fi
        shift
      done
      if test -n "$values_file"; then
        sed 's/^/helm-values /' "$values_file" >> "$HOPS_TEST_COMMAND_LOG"
      fi
      cat <<'YAML'
apiVersion: v1
kind: ConfigMap
metadata:
  name: rendered
YAML
    fi
    exit 0
    ;;
  kubectl)
    if test -n "$HOPS_TEST_SOURCE_PROBE"; then
      case "$*" in
        *'create -f -')
          cat > "$HOPS_TEST_COMMAND_LOG.probe"
          sed 's/^/probe-yaml /' "$HOPS_TEST_COMMAND_LOG.probe" >> "$HOPS_TEST_COMMAND_LOG"
          rm -f "$HOPS_TEST_COMMAND_LOG.polls"
          exit 0 ;;
        *'get pod hops-path-probe-'*'jsonpath={.status.phase}')
          count=0
          test ! -f "$HOPS_TEST_COMMAND_LOG.polls" || count=$(cat "$HOPS_TEST_COMMAND_LOG.polls")
          count=$((count + 1)); echo "$count" > "$HOPS_TEST_COMMAND_LOG.polls"
          if test "$HOPS_TEST_SOURCE_PROBE" = unknown; then echo Failed; else echo Running; fi
          exit 0 ;;
        *'describe pod hops-path-probe-'*) exit 0 ;;
        *'logs hops-path-probe-'*)
          case "$HOPS_TEST_SOURCE_PROBE" in
            missing) echo HOPS_PATH_MISSING ;;
            visible) if test "$(cat "$HOPS_TEST_COMMAND_LOG.polls")" -gt 1; then echo HOPS_PATH_VISIBLE; fi ;;
          esac
          exit 0 ;;
        *'delete pod hops-path-probe-'*) exit 0 ;;
      esac
    fi
    if test "$HOPS_TEST_SYNC_FIXTURE" = 1; then
      case "$*" in
        *'get httproute -n feature -o json') echo '{"items":[]}'; exit 0 ;;
        *'get configmap hops-lgi-'*) echo NotFound >&2; exit 1 ;;
        *'get namespace feature'*)
          printf '%s\n' '{"metadata":{"name":"feature","uid":"fixture-uid","labels":{"app.kubernetes.io/managed-by":"hops-local-gitops","hops.ops.com.ai/local-env":"feature"}}}'
          exit 0 ;;
        *'get pods -n feature -l hops.ops.com.ai/local-env=feature -o json')
          printf '%s\n' '{"items":[{"metadata":{"name":"source-app-1","labels":{"hops.ops.com.ai/local-app":"gateway"}},"status":{"phase":"Running"},"spec":{"containers":[{"name":"app","volumeMounts":[{"name":"source","mountPath":"/workspace"}]}],"volumes":[{"name":"source","emptyDir":{}}]}}]}'
          exit 0 ;;
        *'-- tar xf - -C /workspace') tar xf - -C "$HOPS_TEST_POD_DIR"; exit $? ;;
        *'touch /workspace/.hops-synced'*) touch "$HOPS_TEST_POD_DIR/.hops-synced"; exit 0 ;;
        *'test -f /workspace/.hops-synced'*) test -f "$HOPS_TEST_POD_DIR/.hops-synced"; exit $? ;;
        *'jsonpath={range .items[*]}'*) echo source-app-1; exit 0 ;;
        *'jsonpath='*'emptyDir}'*) echo '{}'; exit 0 ;;
      esac
    fi
    if test "$1" = "config" && test "$2" = "get-contexts"; then
      printf 'colima\nkind-project-dev\n'
      exit 0
    fi
    case "$*" in
      *'get provider.pkg.crossplane.io,configuration.pkg.crossplane.io,function.pkg.crossplane.io -o json')
        printf '%s\n' '{"items":[]}'
        ;;
      *'get nodes -o json')
        printf '%s\n' '{"items":[{"metadata":{"name":"project-dev-control-plane"}}]}'
        ;;
      *'get deployment crossplane-rbac-manager '*'-o json')
        printf '%s\n' '{"spec":{"template":{"spec":{"containers":[{"name":"crossplane","resources":{}}]}}}}'
        ;;
      *'get deployment crossplane '*'-o json')
        printf '%s\n' '{"spec":{"template":{"spec":{"containers":[{"name":"crossplane","resources":{}}]}}}}'
        ;;
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
        fs::create_dir_all(root.join("apps/gateway/.gitops/local/templates")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::write(
            root.join("apps/gateway/.gitops/local/Chart.yaml"),
            "apiVersion: v2\nname: gateway\nversion: 0.1.0\n",
        )
        .unwrap();
        let bin = root.join("fake-bin");
        fs::create_dir_all(&bin).unwrap();
        for tool in ["kind", "docker", "helm", "kubectl", "mutagen"] {
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

    fn base_command(&self) -> Command {
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
            .env("HOPS_KIND_REGISTRY_HOST_PORT", "39001")
            .env("HOPS_TEST_COMMAND_LOG", &self.command_log)
            .env("HOPS_TEST_CLUSTER_EXISTS", &self.cluster_exists)
            .env("HOPS_TEST_EXPECTED_MOUNT", &self.root)
            .env_remove("HOPS_KIND_EXTRA_MOUNT");
        command
    }

    fn command(&self) -> Command {
        let mut command = self.base_command();
        command.args(["local", "gitops", "cluster", DEFINITION_PATH, "--once"]);
        command
    }

    fn environment_command(&self) -> Command {
        let mut command = self.base_command();
        command.args([
            "local",
            "gitops",
            "environment",
            ".gitops/local/environment.yaml",
            "--once",
            "--dry-run",
        ]);
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
        let state = self
            .root
            .join("home/.hops/local/runtime/feature.delivery.json");
        if let Ok(text) = fs::read_to_string(state) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                for pid in value
                    .get("syncPids")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    if let Some(pid) = pid.as_u64() {
                        // SAFETY: this fixture's CLI started this process group
                        // and recorded its leader in the fixture-owned state.
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGTERM);
                        }
                    }
                }
            }
        }
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
    assert!(
        output_text(&output).contains("must be $HOME")
            || output_text(&output).contains("unable to canonicalize")
    );
    fixture.assert_no_mutation();
}

#[test]
fn injects_normalized_local_domain_for_runtime_environment() {
    let fixture = Fixture::new();
    let definition = VALID_DEFINITION.replacen(
        "  manifests:",
        "  localDomain: .gitkb.localhost\n  manifests:",
        1,
    );
    fixture.write_definition(&definition);
    fs::write(
        fixture.root.join(".gitops/local/environment.yaml"),
        ENVIRONMENT_DEFINITION,
    )
    .unwrap();
    fs::write(&fixture.cluster_exists, "existing").unwrap();

    let output = fixture
        .base_command()
        .args([
            "local",
            "gitops",
            "environment",
            ".gitops/local/environment.yaml",
            "--name",
            "feature-auth",
            "--once",
            "--dry-run",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", output_text(&output));
    let log = fixture.log();
    assert!(
        log.contains("helm-values localDomain: gitkb.localhost"),
        "{log}"
    );
    assert!(log.contains("helm-values   name: feature-auth"), "{log}");
}

#[test]
fn rejects_non_local_domain_before_mutation() {
    let fixture = Fixture::new();
    let definition = VALID_DEFINITION.replacen(
        "  manifests:",
        "  localDomain: example.com\n  manifests:",
        1,
    );
    fixture.write_definition(&definition);

    let output = fixture.run();

    assert!(!output.status.success());
    assert!(output_text(&output).contains("Cluster.spec.localDomain"));
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

    let output = fixture
        .command()
        .arg("--down")
        .env(
            "HOPS_TEST_MOUNTS",
            r#"[{"Source":"/moved","Destination":"/moved","RW":true}]"#,
        )
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", output_text(&output));
    let log = fixture.log();
    assert!(
        log.contains("docker stop project-dev-control-plane"),
        "{log}"
    );
    assert!(!log.contains("kind delete cluster"), "{log}");
    assert!(!log.contains("docker volume rm"), "{log}");
}

#[test]
fn down_and_dry_run_conflict_for_cluster_and_environment() {
    let fixture = Fixture::new();

    let cluster = fixture
        .command()
        .args(["--down", "--dry-run"])
        .output()
        .unwrap();
    assert!(!cluster.status.success());
    assert!(output_text(&cluster).contains("cannot be used with"));
    fixture.assert_no_mutation();

    let environment = fixture
        .base_command()
        .args([
            "local",
            "gitops",
            "environment",
            "--name",
            "local",
            "--down",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(!environment.status.success());
    assert!(output_text(&environment).contains("cannot be used with"));
    fixture.assert_no_mutation();
}

#[test]
fn environment_activates_its_declared_cluster_over_generic_state() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join(".gitops/local/environment.yaml"),
        ENVIRONMENT_DEFINITION,
    )
    .unwrap();
    fs::write(&fixture.cluster_exists, "existing").unwrap();
    let state = fixture.root.join("home/.hops/local");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("backend"), "colima\n").unwrap();
    fs::write(
        state.join("providers.json"),
        r#"{"clusterProvider":"colima","dockerProvider":"colima","clusterName":"wrong"}"#,
    )
    .unwrap();
    fs::create_dir_all(state.join("envs")).unwrap();
    fs::write(
        state.join("envs/local.json"),
        r#"{"name":"local","namespace":"local","envPath":"/old","clusterName":"wrong","kubeContext":"colima"}"#,
    )
    .unwrap();

    let output = fixture
        .environment_command()
        .env("HOPS_KUBE_CONTEXT", "colima")
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", output_text(&output));
    let log = fixture.log();
    assert!(
        log.lines().any(
            |line| line.starts_with("kubectl --context kind-project-dev ")
                && line.contains("get nodes -o json")
        ),
        "declared Cluster context was not used: {log}"
    );
    assert!(
        !log.lines()
            .any(|line| line.starts_with("kubectl --context colima ")
                && line.contains("get nodes -o json")),
        "generic context leaked into Environment reconcile: {log}"
    );
    let providers = fs::read_to_string(state.join("providers.json")).unwrap();
    assert!(providers.contains(r#""clusterProvider": "kind""#));
    assert!(providers.contains(r#""dockerProvider": "dory""#));
    assert!(providers.contains(r#""clusterName": "project-dev""#));
}

fn accepted_hostpath_fixture() -> (Fixture, PathBuf, String) {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join(".gitops/local/environment.yaml"),
        ENVIRONMENT_DEFINITION,
    )
    .unwrap();
    fs::write(&fixture.cluster_exists, "existing").unwrap();
    let state = fixture.root.join("home/.hops/local/envs");
    fs::create_dir_all(&state).unwrap();
    let record = state.join("feature.json");
    let contents = serde_json::json!({
        "name":"feature", "namespace":"feature", "envPath":fixture.root.join(".gitops/local/environment.yaml"),
        "projectRoot":fixture.root, "deliveryMode":"hostPath", "clusterName":"project-dev", "kubeContext":"kind-project-dev"
    }).to_string();
    fs::write(&record, &contents).unwrap();
    (fixture, record, contents)
}

#[test]
fn running_probe_waits_for_output_and_shared_roots_are_probed_once() {
    let (fixture, record, contents) = accepted_hostpath_fixture();
    let chart = fixture.root.join("apps/second/.gitops/local");
    fs::create_dir_all(&chart).unwrap();
    fs::write(
        chart.join("Chart.yaml"),
        "apiVersion: v2\nname: second\nversion: 0.1.0\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join(".gitops/local/environment.yaml"),
        format!(
            "{ENVIRONMENT_DEFINITION}    - path: apps/second/.gitops/local\n      type: helm\n"
        ),
    )
    .unwrap();
    let output = fixture
        .environment_command()
        .args(["--name", "feature", "--namespace", "feature"])
        .env("HOPS_TEST_SOURCE_PROBE", "visible")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains("source delivery hostPath"),
        "{}",
        output_text(&output)
    );
    assert_eq!(
        fs::read_to_string(fixture.command_log.with_extension("log.polls"))
            .unwrap()
            .trim(),
        "2"
    );
    let log = fixture.log();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("create -f -"))
            .count(),
        1
    );
    let name = log
        .lines()
        .find_map(|line| line.strip_prefix("probe-yaml   name: "))
        .unwrap();
    assert!(uuid::Uuid::parse_str(name.strip_prefix("hops-path-probe-").unwrap()).is_ok());
    assert!(log.contains("delete pod hops-path-probe-"));
    assert_eq!(fs::read_to_string(record).unwrap(), contents);
}

#[test]
fn inconclusive_probe_keeps_accepted_mounts_and_registration() {
    let (fixture, record, contents) = accepted_hostpath_fixture();
    let output = fixture
        .environment_command()
        .args(["--name", "feature", "--namespace", "feature"])
        .env("HOPS_TEST_SOURCE_PROBE", "unknown")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "an inconclusive probe must defer, not report successful sync: {}",
        output_text(&output)
    );
    assert!(output_text(&output).contains("leaving source delivery unchanged"));
    assert!(!fixture.log().contains("helm template"));
    assert!(!fixture.log().contains("apply -f"));
    assert!(fixture.log().contains("delete pod hops-path-probe-"));
    assert_eq!(fs::read_to_string(record).unwrap(), contents);
}

#[test]
fn confirmed_missing_path_does_not_replace_existing_hostpath() {
    let (fixture, record, contents) = accepted_hostpath_fixture();
    let output = fixture
        .environment_command()
        .args(["--name", "feature", "--namespace", "feature"])
        .env("HOPS_TEST_SOURCE_PROBE", "missing")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{}", output_text(&output));
    assert!(output_text(&output).contains("preserving accepted hostPath volumes"));
    assert!(!fixture.log().contains("helm template"));
    assert_eq!(fs::read_to_string(record).unwrap(), contents);
}

#[test]
fn initial_sync_delivers_files_before_recording_success() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join(".gitops/local/environment.yaml"),
        ENVIRONMENT_DEFINITION,
    )
    .unwrap();
    fs::write(&fixture.cluster_exists, "existing").unwrap();
    fs::create_dir_all(fixture.root.join("src")).unwrap();
    fs::write(
        fixture.root.join("src/sentinel.txt"),
        "source delivered through the real tar pipeline",
    )
    .unwrap();
    let pod = fixture.root.join("target/fake-pod");
    fs::create_dir_all(&pod).unwrap();
    let output = fixture
        .base_command()
        .args([
            "local",
            "gitops",
            "environment",
            ".gitops/local/environment.yaml",
            "--once",
            "--name",
            "feature",
            "--namespace",
            "feature",
        ])
        .env("HOPS_TEST_SOURCE_PROBE", "missing")
        .env("HOPS_TEST_SYNC_FIXTURE", "1")
        .env("HOPS_TEST_POD_DIR", &pod)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\nCOMMANDS:\n{}",
        output_text(&output),
        fixture.log()
    );
    assert!(
        pod.join("src/sentinel.txt").is_file(),
        "CLI reported sync success without delivering source"
    );
    assert_eq!(
        fs::read_to_string(pod.join("src/sentinel.txt")).unwrap(),
        "source delivered through the real tar pipeline"
    );
    assert!(pod.join(".hops-synced").is_file());
    let record: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture.root.join("home/.hops/local/envs/feature.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["deliveryMode"], "sync");
    let runtime: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(
            fixture
                .root
                .join("home/.hops/local/runtime/feature.delivery.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(!runtime["syncPids"].as_array().unwrap().is_empty());
}
