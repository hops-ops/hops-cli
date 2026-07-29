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
    // Pin a known-good commit of the hops integration work (not a moving branch
    // tip — the upstream branch may never merge). Bump intentionally with dory.
    assert!(
        text.contains("f8c61d2fd0fc4d528e5e0da36ffa09b9796b3871"),
        "must pin patrickleet/dory to a specific commit SHA"
    );
    assert!(
        !text
            .lines()
            .any(|l| l.trim() == "ref: feat/hops-local-integration"),
        "must not track the moving feat/hops-local-integration branch tip"
    );
    assert!(
        text.contains("xcodebuild") && text.contains("derivedDataPath"),
        "must build in-pipeline with deterministic derivedDataPath"
    );
    assert!(
        text.contains("dtolnay/rust-toolchain") && text.contains("brew install protobuf"),
        "must install the Rust and protoc prerequisites used by Dory's FFI builder"
    );
    let ffi_build = text
        .find("scripts/build-dory-ffi-xcframework.sh --if-needed")
        .expect("must materialize DoryFFI from a clean checkout");
    let app_build = text
        .find("xcodebuild -project Dory.xcodeproj")
        .expect("must build the Dory app");
    assert!(
        ffi_build < app_build,
        "must generate DoryFFI before SwiftPM resolves the Dory app"
    );
    assert!(
        text.contains("DoryFFI.xcframework/macos-arm64_x86_64/libdory_ffi.a")
            && text.contains("Sources/DoryCore/generated/dory_ffi.swift"),
        "must verify both generated Dory FFI artifacts before xcodebuild"
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
