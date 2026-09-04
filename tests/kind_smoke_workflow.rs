//! Structural checks for the shipped Kind smoke workflow.

use std::fs;
use std::path::PathBuf;

fn workflow_text() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/on-pr-kind-smoke.yaml");
    assert!(path.is_file(), "expected Kind smoke at {}", path.display());
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn path_install_waits_for_canonical_oci_package_name() {
    let text = workflow_text();
    assert!(
        text.contains("CONFIGURATION=hops-ops-config-smoke"),
        "Kind smoke must wait for the same <org>-<package> name installed by the CLI"
    );
    assert!(
        !text
            .lines()
            .any(|line| line.trim() == "CONFIGURATION=config-smoke"),
        "Kind smoke must not wait for the package's short internal metadata name"
    );
}

#[test]
fn kind_backend_smoke_is_opt_in() {
    let text = workflow_text();
    for required in [
        "workflow_dispatch:",
        "contains(github.event.pull_request.labels.*.name, 'test-kind')",
    ] {
        assert!(
            text.contains(required),
            "Kind smoke must remain opt-in through manual dispatch or the test-kind label"
        );
    }
}
