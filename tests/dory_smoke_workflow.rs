//! Structural checks for the shipped dory macOS smoke workflow.
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
        "must ensure ~/.dory/engine.sock before hops start"
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
        text.contains("labeled") && text.contains("test-dory"),
        "PR smoke must start only when opted in with the test-dory label"
    );
    assert!(
        text.contains("github.event_name == 'workflow_dispatch'")
            && text.contains("github.event.pull_request.labels.*.name"),
        "manual dispatch must remain available while PR runs are label-gated"
    );
    // Nested-virt pin for Colima-backed engine.sock on public GHA.
    assert!(
        text.lines()
            .any(|l| l.trim() == "runs-on: macos-15-intel"),
        "must pin macos-15-intel for nested virt (not bare macos-latest alone)"
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
    assert!(
        text.contains("kube-dns") || text.contains("coredns"),
        "must wait for CoreDNS/kube-dns before registry round-trip"
    );
    assert!(
        text.contains("--timeout=420s"),
        "must use a long Ready/Available timeout for nested-virt lag"
    );
    assert!(
        text.contains("rollout restart"),
        "stop/start resume must rollout-restart stalled deployments"
    );
    assert!(
        text.contains("Debug dump on failure") && text.contains("engine.sock"),
        "failure dump must capture dory/engine diagnostics"
    );
    assert!(
        text.contains("describe pod smoke-svc-name")
            || text.contains("describe pod smoke-svc-name smoke-localhost"),
        "failure dump must describe smoke pods in default ns"
    );
    assert!(
        text.to_lowercase().contains("localhost:30500"),
        "must exercise localhost:30500 registry path"
    );
}

#[test]
fn dory_smoke_workflow_public_gha_engine_fallback() {
    let text = workflow_text();
    // Public GHA cannot run dory-hv; workflow must document and implement a
    // Colima-backed engine.sock so hops --backend dory remains testable.
    assert!(
        text.contains("colima start") && text.contains("engine.sock"),
        "must bootstrap Colima docker as engine.sock when native sock is absent"
    );
    assert!(
        text.to_lowercase().contains("surrogate")
            || text.contains("colima-surrogate")
            || text.contains("Colima-backed"),
        "must document Colima engine.sock surrogate for public GHA"
    );
    assert!(
        text.contains("xattr") || text.contains("codesign"),
        "must clear quarantine/sign Debug Dory.app before open"
    );
}
