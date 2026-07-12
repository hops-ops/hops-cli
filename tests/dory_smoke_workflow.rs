//! Structural checks for the shipped dory macOS smoke (spike) workflow.
//!
//! Drives the real `.github/workflows/on-pr-dory-smoke.yaml` so the clone+build
//! install contract and kind-parity smoke steps cannot silently regress.

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
fn dory_smoke_workflow_clone_build_install_contract() {
    let text = workflow_text();
    assert!(
        text.contains("patrickleet/dory"),
        "must clone patrickleet/dory"
    );
    assert!(
        text.contains("feat/hops-local-integration"),
        "must pin hops integration branch"
    );
    assert!(
        text.contains("xcodebuild") && text.contains("derivedDataPath"),
        "must build in-pipeline with deterministic derivedDataPath"
    );
    assert!(
        text.contains("GITHUB_PATH") && text.contains("scripts"),
        "must put scripts/dory on PATH"
    );
    assert!(
        text.contains("engine.sock"),
        "must wait for ~/.dory/engine.sock before hops start"
    );
    assert!(
        !text.contains("brew install --cask") && !text.contains("homebrew/cask"),
        "must not use brew cask as primary install"
    );
    assert!(
        text.contains("workflow_dispatch") && text.contains("pull_request:"),
        "must support pull_request (PR-branch runs) and workflow_dispatch"
    );
    assert!(
        text.lines().any(|l| l.trim() == "runs-on: macos-15"),
        "must pin explicit macos-15 (not bare macos-latest alone)"
    );
}

#[test]
fn dory_smoke_workflow_kind_parity_when_engine_boots() {
    let text = workflow_text();
    for needle in [
        "cargo build",
        "start --backend dory",
        "local doctor",
        "localhost:30500",
        "registry.crossplane-system.svc.cluster.local:5000",
        "--context dory",
        "local stop",
        "local destroy",
    ] {
        assert!(
            text.contains(needle),
            "dory smoke missing kind-parity fragment: {needle}"
        );
    }
    // stop/start resume: start without --backend after stop
    assert!(
        text.contains("local start\n")
            || text
                .lines()
                .any(|l| l.trim() == "./target/debug/hops-cli local start"),
        "must start again without --backend after stop"
    );
}

#[test]
fn dory_smoke_workflow_carries_colima_lessons() {
    let text = workflow_text();
    // Nested-virt settle before registry pods (CoreDNS/node Ready).
    assert!(
        text.contains("kube-dns") || text.contains("coredns"),
        "must wait for CoreDNS/kube-dns before registry round-trip"
    );
    // Smoke Ready timeout must outlast CNI lag under nested virt (colima: 420s).
    assert!(
        text.contains("--timeout=420s"),
        "must use a long Ready/Available timeout for nested-virt lag"
    );
    // Resume recovery when container IDs churn after stop/start.
    assert!(
        text.contains("rollout restart"),
        "stop/start resume must rollout-restart stalled deployments"
    );
    // Failure dump before destroy — smoke pods + dory state.
    assert!(
        text.contains("Debug dump on failure") && text.contains("engine.sock"),
        "failure dump must capture dory/engine diagnostics"
    );
    assert!(
        text.contains("describe pod smoke-svc-name")
            || text.contains("describe pod smoke-svc-name smoke-localhost"),
        "failure dump must describe smoke pods in default ns"
    );
    // Prefer localhost registry, not raw VM IP (macOS 15 privacy).
    assert!(
        text.to_lowercase().contains("localhost:30500"),
        "must exercise localhost:30500 registry path"
    );
}
