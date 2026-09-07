use serde::Deserialize;
use serde_yaml::Value;
use std::path::PathBuf;
use std::process::{Command, Output};

fn render(profile: &str, extra: &[&str]) -> Output {
    let chart = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("charts/oci-registry");
    Command::new("helm")
        .args(["template", "registry"])
        .arg(&chart)
        .args(["--namespace", "crossplane-dev", "-f"])
        .arg(chart.join("ci").join(format!("{profile}.yaml")))
        .args(extra)
        .output()
        .expect("helm is required for chart contract tests")
}

fn documents(profile: &str, extra: &[&str]) -> Vec<Value> {
    let output = render(profile, extra);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_yaml::Deserializer::from_slice(&output.stdout)
        .map(|doc| Value::deserialize(doc).unwrap())
        .filter(|doc| !doc.is_null())
        .collect()
}

fn resource<'a>(docs: &'a [Value], kind: &str, name: &str) -> &'a Value {
    docs.iter()
        .find(|d| d["kind"] == kind && d["metadata"]["name"] == name)
        .unwrap()
}

// OCI-AC-001.1, 002.1, 002.2, 003.1, 006.1: rendered contracts;
// runtime push/TLS/RBAC behavior requires the separate Kubernetes fixture.
#[test]
fn local_and_remote_share_private_read_write_contract() {
    for profile in ["local", "remote"] {
        let docs = documents(profile, &[]);
        let config = resource(&docs, "ConfigMap", "registry-config");
        for mode in ["write", "read"] {
            let text = config["data"][format!("{mode}.yml")].as_str().unwrap();
            let cfg: Value = serde_yaml::from_str(text).unwrap();
            assert!(cfg["proxy"].is_null());
            assert_eq!(cfg["storage"]["delete"]["enabled"], false);
            assert_eq!(cfg["storage"]["redirect"]["disable"], true);
            assert_eq!(
                cfg["storage"]["maintenance"]["uploadpurging"]["enabled"],
                false
            );
            assert_eq!(
                cfg["http"]["addr"],
                if mode == "write" {
                    "127.0.0.1:5001"
                } else {
                    ":5000"
                }
            );
            if mode == "read" {
                assert_eq!(cfg["storage"]["maintenance"]["readonly"]["enabled"], true);
                assert_eq!(cfg["http"]["tls"]["certificate"], "/certs/tls.crt");
            } else {
                assert!(cfg["storage"]["maintenance"]["readonly"].is_null());
                assert_eq!(cfg["http"]["relativeurls"], true);
            }
            if profile == "local" {
                assert!(cfg["storage"]["s3"].is_null());
                assert_eq!(
                    cfg["storage"]["filesystem"]["rootdirectory"],
                    "/var/lib/registry"
                );
            } else {
                assert_eq!(cfg["storage"]["s3"]["encrypt"], true);
                assert_eq!(cfg["storage"]["s3"]["secure"], true);
                assert!(cfg["storage"]["s3"]["accesskey"].is_null());
                assert!(cfg["storage"]["filesystem"].is_null());
            }
        }
        for doc in &docs {
            assert!(![
                "Ingress",
                "Gateway",
                "HTTPRoute",
                "OCIRegistry",
                "Configuration",
                "Provider",
                "CronJob"
            ]
            .contains(&doc["kind"].as_str().unwrap()));
            if doc["kind"] == "Service" {
                assert_eq!(doc["spec"]["type"], "ClusterIP");
                assert!(doc["spec"]["ports"][0]["nodePort"].is_null());
            }
        }
        let sts = resource(&docs, "StatefulSet", "registry");
        assert_eq!(sts["spec"]["replicas"], 1);
        assert_eq!(
            sts["spec"]["template"]["spec"]["containers"]
                .as_sequence()
                .unwrap()
                .len(),
            2
        );
        let rules = &resource(&docs, "Role", "registry-upload")["rules"];
        assert_eq!(rules[2]["resources"][0], "pods/portforward");
        assert_eq!(rules[2]["resourceNames"][0], "registry-0");
        assert_eq!(
            rules[2]["verbs"],
            serde_yaml::to_value(vec!["get", "create"]).unwrap()
        );
        let policy = resource(&docs, "NetworkPolicy", "registry");
        assert_eq!(policy["spec"]["ingress"][0]["ports"][0]["port"], 5000);
        assert_eq!(policy["spec"]["ingress"].as_sequence().unwrap().len(), 1);
    }
}

#[test]
fn pvc_survives_helm_argo_removal_and_existing_claim_is_not_adopted() {
    let docs = documents("local", &[]);
    let pvc = resource(&docs, "PersistentVolumeClaim", "registry-pvc");
    assert_eq!(
        pvc["metadata"]["annotations"]["helm.sh/resource-policy"],
        "keep"
    );
    assert_eq!(
        pvc["metadata"]["annotations"]["argocd.argoproj.io/sync-options"],
        "Prune=false,Delete=false"
    );
    let existing = documents(
        "local",
        &["--set", "storage.pvc.existingClaim=retained-data"],
    );
    assert!(!existing
        .iter()
        .any(|d| d["kind"] == "PersistentVolumeClaim"));
    let sts = resource(&existing, "StatefulSet", "registry");
    assert_eq!(
        sts["spec"]["template"]["spec"]["volumes"][2]["persistentVolumeClaim"]["claimName"],
        "retained-data"
    );
    assert!(!documents("remote", &[])
        .iter()
        .any(|d| d["kind"] == "PersistentVolumeClaim"));
}

#[test]
fn inputs_fail_closed_for_cache_mode_missing_trust_storage_and_access() {
    for (profile, flag, error) in [
        (
            "local",
            "proxy.remoteurl=https://registry-1.docker.io",
            "proxy",
        ),
        ("local", "storage.type=cache", "type"),
        ("local", "tls.existingSecret=", "existingSecret"),
        (
            "local",
            "storage.pvc.encryptionConfirmed=false",
            "encryptionConfirmed",
        ),
        ("local", "access.pullPeers=[]", "pullPeers"),
        ("remote", "storage.s3.bucket=", "storage.s3.bucket"),
        (
            "remote",
            "storage.s3.accesskey=synthetic-canary",
            "accesskey",
        ),
        ("local", "name=../../escape", "name"),
    ] {
        let output = render(profile, &["--set", flag]);
        assert!(!output.status.success(), "accepted unsafe input: {flag}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(error), "{flag}: {stderr}");
    }
}

#[test]
fn empty_upload_subjects_grant_nobody_and_renders_are_deterministic() {
    let docs = documents("local", &["--set-json", "access.uploadSubjects=[]"]);
    assert!(!docs
        .iter()
        .any(|d| d["kind"] == "Role" || d["kind"] == "RoleBinding"));
    assert_eq!(render("local", &[]).stdout, render("local", &[]).stdout);
}
