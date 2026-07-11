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
        text.contains("workflow_dispatch"),
        "spike remains dispatch-oriented until public GHA engine boot is proven"
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
}
