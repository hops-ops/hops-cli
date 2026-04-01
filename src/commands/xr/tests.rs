use super::helpers::discovery::{has_tag, kms_key_external_name, role_name_from_arn, tag_value};
use super::helpers::manifest::{
    match_spec, prune_value_to_openapi_schema, render_manifest, sanitize_authored_manifest,
    sanitize_manifest_defaults, strip_external_name_fields, strip_runtime_k8s_fields,
};
use super::helpers::types::{AdoptArgs, ManifestSource, ReclaimSpec, NETWORK_TAG_KEY};
use super::orphan::{canonical_orphan_management_policies, orphan_xr_management_policies};
use clap::Parser;
use serde_json::Value as JsonValue;
use serde_yaml::Value;

fn test_spec(kind: &str) -> ReclaimSpec {
    ReclaimSpec {
        api_version: "aws.hops.ops.com.ai/v1alpha1".to_string(),
        kind: kind.to_string(),
        plural: kind.to_ascii_lowercase(),
        group: "aws.hops.ops.com.ai".to_string(),
        project_slug: "test-project".to_string(),
        composed_resources: Vec::new(),
        live_resolver: None,
    }
}

#[test]
fn render_manifest_builds_basic_scaffold() {
    let spec = test_spec("ActionsConnector");
    let manifest = render_manifest(&spec, "imported", "hops").expect("manifest");
    let yaml = serde_yaml::to_string(&manifest).expect("yaml");
    assert!(yaml.contains("kind: ActionsConnector"));
    assert!(yaml.contains("name: imported"));
    assert!(yaml.contains("namespace: hops"));
    assert!(yaml.contains("spec: {}"));
}

#[test]
fn sanitize_autoekscluster_sets_default_provider_config() {
    let spec = test_spec("AutoEKSCluster");
    let mut manifest = render_manifest(&spec, "pat-local", "default").expect("manifest");
    sanitize_manifest_defaults(&spec, &mut manifest, ManifestSource::Generated);
    let yaml = serde_yaml::to_string(&manifest).expect("yaml");
    assert!(yaml.contains("name: default"));
    assert!(yaml.contains("kind: ProviderConfig"));
}

#[test]
fn strip_runtime_fields_removes_status_and_resource_version() {
    let mut manifest: Value = serde_yaml::from_str(
        r#"
apiVersion: aws.hops.ops.com.ai/v1alpha1
kind: AutoEKSCluster
metadata:
  name: pat-local
  resourceVersion: "123"
status:
  ready: true
"#,
    )
    .expect("yaml");
    strip_runtime_k8s_fields(&mut manifest);
    let yaml = serde_yaml::to_string(&manifest).expect("yaml");
    assert!(!yaml.contains("resourceVersion"));
    assert!(!yaml.contains("status:"));
}

#[test]
fn sanitize_authored_manifest_removes_runtime_crossplane_fields() {
    let mut manifest: Value = serde_yaml::from_str(
        r#"
apiVersion: aws.hops.ops.com.ai/v1alpha1
kind: AutoEKSCluster
metadata:
  name: pat-local
  namespace: default
  annotations:
    kubectl.kubernetes.io/last-applied-configuration: "{}"
  finalizers:
    - composite.apiextensions.crossplane.io
  labels:
    crossplane.io/composite: pat-local
spec:
  crossplane:
    resourceRefs:
      - name: managed-a
  managementPolicies:
    - "*"
"#,
    )
    .expect("yaml");

    sanitize_authored_manifest(&mut manifest);
    let yaml = serde_yaml::to_string(&manifest).expect("yaml");
    assert!(!yaml.contains("last-applied-configuration"));
    assert!(!yaml.contains("finalizers:"));
    assert!(!yaml.contains("crossplane.io/composite"));
    assert!(!yaml.contains("resourceRefs"));
}

#[test]
fn prune_value_to_openapi_schema_removes_undefined_fields() {
    let mut value: Value = serde_yaml::from_str(
        r#"
accountId: "123"
crossplane:
  resourceRefs:
    - name: managed-a
nodeConfig:
  enabled: true
  extraField: nope
"#,
    )
    .expect("yaml");
    let schema: JsonValue = serde_json::from_str(
        r#"
{
  "type": "object",
  "properties": {
    "accountId": { "type": "string" },
    "nodeConfig": {
      "type": "object",
      "properties": {
        "enabled": { "type": "boolean" }
      }
    }
  }
}
"#,
    )
    .expect("json");

    prune_value_to_openapi_schema(&mut value, &schema);
    let yaml = serde_yaml::to_string(&value).expect("yaml");
    assert!(yaml.contains("accountId: '123'"));
    assert!(yaml.contains("enabled: true"));
    assert!(!yaml.contains("crossplane:"));
    assert!(!yaml.contains("extraField"));
}

#[test]
fn strip_external_name_fields_removes_nested_identity_fields() {
    let mut manifest: Value = serde_yaml::from_str(
        r#"
spec:
  iam:
    controlPlaneRole:
      externalName: pat-local-controlplane
    nodeRole:
      externalName: pat-local-node
  kms:
    externalName: 2f7bebfa-cdc1-436f-8fe7-2256fd73b794
  routeTables:
    externalNames:
      public: rtb-123
    associationExternalNames:
      private-a: rtbassoc-123
  nat:
    eipExternalNames:
      a: eipalloc-123
"#,
    )
    .expect("yaml");

    strip_external_name_fields(&mut manifest);
    let yaml = serde_yaml::to_string(&manifest).expect("yaml");
    assert!(!yaml.contains("externalName"));
    assert!(!yaml.contains("externalNames"));
    assert!(!yaml.contains("associationExternalNames"));
    assert!(!yaml.contains("eipExternalNames"));
}

#[test]
fn tag_lookup_reads_aws_shape() {
    let json: JsonValue =
        serde_json::from_str(r#"{"Tags":[{"Key":"hops.ops.com.ai/network","Value":"demo"}]}"#)
            .expect("json");
    assert_eq!(tag_value(&json, NETWORK_TAG_KEY), Some("demo"));
    assert!(has_tag(&json, NETWORK_TAG_KEY, "demo"));
}

#[test]
fn match_spec_normalizes_hyphenated_names() {
    let specs = vec![ReclaimSpec {
        api_version: "aws.hops.ops.com.ai/v1alpha1".to_string(),
        kind: "AutoEKSCluster".to_string(),
        plural: "autoeksclusters".to_string(),
        group: "aws.hops.ops.com.ai".to_string(),
        project_slug: "auto-eks-cluster".to_string(),
        composed_resources: Vec::new(),
        live_resolver: Some("aws-autoekscluster".to_string()),
    }];
    let spec = match_spec(&specs, "autoekscluster").expect("auto eks cluster spec");
    assert_eq!(spec.kind, "AutoEKSCluster");
    assert_eq!(spec.live_resolver.as_deref(), Some("aws-autoekscluster"));
}

#[test]
fn parses_role_name_from_arn() {
    assert_eq!(
        role_name_from_arn("arn:aws:iam::123456789012:role/pat-local-controlplane").as_deref(),
        Some("pat-local-controlplane")
    );
}

#[test]
fn orphan_management_policies_replaces_wildcard_without_delete() {
    assert_eq!(
        canonical_orphan_management_policies(Some(vec!["*".to_string()])),
        vec![
            "Create".to_string(),
            "LateInitialize".to_string(),
            "Observe".to_string(),
            "Update".to_string(),
        ]
    );
}

#[test]
fn orphan_management_policies_removes_delete_and_skips_when_already_orphaned() {
    assert_eq!(
        canonical_orphan_management_policies(Some(vec![
            "Create".to_string(),
            "Delete".to_string(),
            "Observe".to_string(),
        ])),
        vec!["Create".to_string(), "Observe".to_string(),]
    );

    let manifest: Value = serde_yaml::from_str(
        r#"
apiVersion: aws.hops.ops.com.ai/v1alpha1
kind: AutoEKSCluster
metadata:
  name: pat-local
spec:
  managementPolicies:
    - Create
    - Observe
    - Update
    - LateInitialize
"#,
    )
    .expect("yaml");
    assert_eq!(orphan_xr_management_policies(&manifest), None);
}

#[test]
fn orphan_xr_management_policies_patches_top_level_spec() {
    let manifest: Value = serde_yaml::from_str(
        r#"
apiVersion: aws.hops.ops.com.ai/v1alpha1
kind: AutoEKSCluster
metadata:
  name: pat-local
spec:
  managementPolicies:
    - "*"
"#,
    )
    .expect("yaml");

    assert_eq!(
        orphan_xr_management_policies(&manifest),
        Some(vec![
            "Create".to_string(),
            "LateInitialize".to_string(),
            "Observe".to_string(),
            "Update".to_string(),
        ])
    );
}

#[test]
fn kms_key_external_name_prefers_key_id() {
    let item: JsonValue = serde_json::from_str(
        r#"
{
  "status": {
    "atProvider": {
      "keyId": "2f7bebfa-cdc1-436f-8fe7-2256fd73b794",
      "arn": "arn:aws:kms:us-east-2:123456789012:key/ignored"
    }
  }
}
"#,
    )
    .expect("json");

    assert_eq!(
        kms_key_external_name(&item)
            .expect("kms key external name")
            .as_deref(),
        Some("2f7bebfa-cdc1-436f-8fe7-2256fd73b794")
    );
}

#[test]
fn kms_key_external_name_falls_back_to_arn() {
    let item: JsonValue = serde_json::from_str(
        r#"
{
  "status": {
    "atProvider": {
      "arn": "arn:aws:kms:us-east-2:123456789012:key/2f7bebfa-cdc1-436f-8fe7-2256fd73b794"
    }
  }
}
"#,
    )
    .expect("json");

    assert_eq!(
        kms_key_external_name(&item)
            .expect("kms key external name")
            .as_deref(),
        Some("2f7bebfa-cdc1-436f-8fe7-2256fd73b794")
    );
}

#[test]
fn adopt_args_accept_recursive_flag() {
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        adopt: AdoptArgs,
    }

    let cli = Cli::try_parse_from([
        "test",
        "--kind",
        "AutoEKSCluster",
        "--name",
        "pat-local",
        "--recursive",
        "--apply",
    ])
    .expect("parse");

    assert!(cli.adopt.recursive);
    assert!(cli.adopt.apply);
}
