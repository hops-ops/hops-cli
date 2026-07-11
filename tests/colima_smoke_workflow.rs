//! Structural checks for the shipped colima macOS smoke workflow.
//!
//! These tests drive the real file under `.github/workflows/` so a missing
//! or regressed smoke contract fails `cargo test` without needing a macOS GHA run.

use std::fs;
use std::path::PathBuf;

fn workflow_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/on-pr-colima-smoke.yaml")
}

fn workflow_text() -> String {
    let path = workflow_path();
    assert!(
        path.is_file(),
        "expected colima smoke workflow at {}",
        path.display()
    );
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn colima_smoke_workflow_exists_and_pins_nested_virt_runner() {
    let text = workflow_text();
    assert!(
        text.contains("runs-on: macos-15-intel"),
        "must pin nested-virt-capable Intel runner"
    );
    assert!(
        !text
            .lines()
            .any(|l| l.trim() == "runs-on: macos-latest"),
        "must not use bare macos-latest as sole runs-on"
    );
    assert!(
        text.to_lowercase().contains("nested") && text.to_lowercase().contains("virt"),
        "must comment on runner/virt constraints"
    );
}

#[test]
fn colima_smoke_workflow_kind_parity_sequence() {
    let text = workflow_text();
    for needle in [
        "cargo build",
        "start --backend colima",
        "local doctor",
        "localhost:30500",
        "registry.crossplane-system.svc.cluster.local:5000",
        "--context colima",
        "local stop",
        "local destroy",
    ] {
        assert!(
            text.contains(needle),
            "colima smoke missing kind-parity fragment: {needle}"
        );
    }
    // stop/start resume: start without --backend after stop
    assert!(
        text.contains("local start\n") || text.lines().any(|l| l.trim() == "./target/debug/hops-cli local start"),
        "must start again without --backend after stop"
    );
    for tool in ["colima", "docker", "kubectl", "helm"] {
        assert!(
            text.contains(tool),
            "prereq install must mention {tool}"
        );
    }
}

#[test]
fn colima_smoke_workflow_sizes_vm_for_gha_intel_runner() {
    let text = workflow_text();
    // Defaults (8/16/60) exceed macos-15-intel; CI must pass explicit smaller sizes.
    assert!(
        text.contains("--cpus") && text.contains("--memory") && text.contains("--disk"),
        "must pass --cpus/--memory/--disk so the VZ VM fits the runner"
    );
    // 8Gi left CoreDNS thrashing; smoke needs headroom above that floor.
    assert!(
        text.contains("--memory 10") || text.contains("--memory 11") || text.contains("--memory 12"),
        "must allocate at least 10Gi to the colima VM on GHA intel runners"
    );
    assert!(
        text.contains("ha.stderr.log"),
        "failure dump must include lima hostagent stderr for nested-virt debug"
    );
}
