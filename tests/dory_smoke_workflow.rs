//! Structural checks for the shipped dory macOS smoke workflow.
//!
//! Self-hosted stock Dory, env-only session (no desktop context mutation).

use std::fs;
use std::path::PathBuf;

fn workflow_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/on-pr-dory-smoke.yaml")
}

fn workflow_text() -> String {
    let path = workflow_path();
    assert!(
        path.is_file(),
        "expected dory smoke workflow at {}",
        path.display()
    );
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn dory_smoke_workflow_self_hosted_stock_contract() {
    let text = workflow_text();

    assert!(
        !text.contains("patrickleet/dory"),
        "must not clone a hops fork of dory"
    );
    assert!(
        !text.contains("xcodebuild") && !text.contains("build-dory-ffi"),
        "must not build Dory from source"
    );
    assert!(
        !text.contains("colima start")
            && !text.contains("brew install colima")
            && !text.contains("colima-surrogate"),
        "must not use Colima as a dory.sock surrogate"
    );
    assert!(
        text.contains("dory.sock"),
        "must require real ~/.dory/dory.sock"
    );
    assert!(
        text.contains("refusing Colima-backed") || text.contains("Colima-backed dory.sock"),
        "must refuse a Colima-symlinked dory.sock"
    );
    assert!(
        text.contains("self-hosted") && text.contains("hops-dory"),
        "must target self-hosted runner labeled hops-dory"
    );
    assert!(
        !text.lines().any(|l| {
            let t = l.trim();
            t == "runs-on: macos-15"
                || t == "runs-on: macos-latest"
                || t == "runs-on: macos-15-intel"
        }),
        "must not use GitHub-hosted macOS"
    );
    assert!(
        text.contains("labeled") && text.contains("test-dory"),
        "PR smoke must be opt-in via test-dory label"
    );
}

#[test]
fn dory_smoke_workflow_env_only_no_desktop_mutation() {
    let text = workflow_text();

    // Session entirely via env vars.
    assert!(
        text.contains("HOPS_DORY_DESKTOP") && text.contains("\"0\""),
        "must set HOPS_DORY_DESKTOP=0 so hops does not rewrite desktop defaults"
    );
    assert!(
        text.contains("DOCKER_HOST") && text.contains("dory.sock"),
        "must drive docker via DOCKER_HOST → dory.sock"
    );
    assert!(
        text.contains("KUBECONFIG") && text.contains("RUNNER_TEMP"),
        "must use a job-private KUBECONFIG under RUNNER_TEMP"
    );

    // Never switch/restore machine defaults.
    assert!(
        !text.contains("kubectl config use-context"),
        "must not kubectl config use-context"
    );
    assert!(
        !text.contains("docker context use"),
        "must not docker context use"
    );
    assert!(
        !text.contains("Restore desktop contexts") && !text.contains("Snapshot desktop"),
        "must not snapshot/restore contexts — env-only session needs no restore"
    );

    // Do not destroy product plane.
    assert!(
        !text.contains("hops-cli local destroy"),
        "must not hops local destroy"
    );
    assert!(
        !text.contains("docker rm -f dory-k8s"),
        "must not docker rm dory-k8s"
    );
    assert!(
        !text.contains("dory engine sleep") && !text.contains("pkill -f"),
        "must not sleep engine or kill Dory"
    );
    assert!(
        !text.contains("hops-cli local stop"),
        "must not hops local stop"
    );

    assert!(
        text.contains("CARGO_BUILD_JOBS") && text.contains("nice"),
        "must limit cargo parallelism and nice the build"
    );
}

#[test]
fn dory_smoke_workflow_hops_integration_core() {
    let text = workflow_text();
    for needle in [
        "cargo build",
        "start --backend dory",
        "local doctor",
        "registry.crossplane-system.svc.cluster.local:5000",
        "30500",
        "smoke-svc-name",
        "smoke-nodeport",
    ] {
        assert!(
            text.contains(needle),
            "dory smoke missing hops integration fragment: {needle}"
        );
    }
    assert!(
        text.contains("NODE_IP") || text.contains("NetworkSettings"),
        "must push via engine-plane dory-k8s IP"
    );
    // Single-platform pull/build avoids multiplatform push warnings.
    assert!(
        text.contains("docker pull --platform") || text.contains("--platform \"$PLATFORM\""),
        "registry smoke must pull busybox with an explicit --platform"
    );
    assert!(
        text.contains("multiplatform") || text.contains("single-platform"),
        "must document why single-platform materialization is used"
    );
}

#[test]
fn dory_smoke_workflow_path_based_config_install() {
    let text = workflow_text();
    assert!(
        text.contains("tests/fixtures/config-smoke"),
        "must path-install the in-repo config-smoke fixture"
    );
    assert!(
        text.contains("config install --path") || text.contains("config install --path "),
        "must use hops config install --path (no clone)"
    );
    assert!(
        !text.contains("config install --repo"),
        "must not clone a remote config repo for the smoke"
    );
    assert!(
        text.contains("local/ci-xr.yaml") || text.contains("ci-xr.yaml"),
        "must apply the fixture XR"
    );
    assert!(
        text.contains("hops-ci") && text.contains("configmap"),
        "must verify ConfigMap in hops-ci namespace"
    );
    assert!(
        text.contains("command -v up") || text.contains("up CLI"),
        "must require Upbound up CLI for path builds"
    );
}
