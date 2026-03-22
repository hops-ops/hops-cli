use crate::commands::local::{kubectl_apply_stdin, run_cmd_output};
use clap::{Args, Subcommand};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_yaml::{Mapping, Value};
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fs;

const EMBEDDED_RECLAIM_METADATA: &str =
    include_str!(concat!(env!("OUT_DIR"), "/reclaim-metadata.json"));
const NETWORK_TAG_KEY: &str = "hops.ops.com.ai/network";
const SUBNET_TIER_TAG_KEY: &str = "hops.ops.com.ai/tier";
const ROUTE_TABLE_AZ_TAG_KEY: &str = "hops.ops.com.ai/az";

#[derive(Args, Debug)]
pub struct XrArgs {
    #[command(subcommand)]
    pub command: XrCommand,
}

#[derive(Subcommand, Debug)]
pub enum XrCommand {
    /// Generate an observe-only XR manifest for an existing resource
    Observe(ObserveArgs),
    /// Generate the final managed XR manifest from an observed/adopted XR
    Manage(ManageXrArgs),
    /// Patch an observe XR with import identities so it can attach to existing resources
    Adopt(AdoptArgs),
}

#[derive(Args, Debug)]
pub struct ObserveArgs {
    /// XR kind, plural, or project slug (for example: Network, networks, network)
    #[arg(long)]
    pub kind: String,

    /// Kubernetes object name and AWS lookup selector
    #[arg(long)]
    pub name: String,

    /// Namespace to write into the generated manifest
    #[arg(long, default_value = "default")]
    pub namespace: String,

    /// AWS region used for live AWS discovery
    #[arg(long)]
    pub aws_region: String,

    /// Write the generated manifest to a file instead of stdout
    #[arg(long)]
    pub output: Option<String>,
}

#[derive(Args, Debug)]
pub struct ManageXrArgs {
    /// XR kind, plural, or project slug (for example: Network, networks, network)
    #[arg(long)]
    pub kind: String,

    /// Kubernetes object name and AWS lookup selector
    #[arg(long)]
    pub name: String,

    /// Namespace of the existing XR
    #[arg(long, default_value = "default")]
    pub namespace: String,

    /// AWS region used for live AWS discovery
    #[arg(long)]
    pub aws_region: String,

    /// Also write the resulting object to a file
    #[arg(long)]
    pub output: Option<String>,
}

#[derive(Args, Debug)]
pub struct AdoptArgs {
    /// XR kind, plural, or project slug (for example: Network, networks, network)
    #[arg(long)]
    pub kind: String,

    /// Kubernetes object name and AWS lookup selector
    #[arg(long)]
    pub name: String,

    /// Namespace of the existing XR
    #[arg(long, default_value = "default")]
    pub namespace: String,

    /// AWS region used for live AWS discovery
    #[arg(long)]
    pub aws_region: String,

    /// Print the adopted XR instead of applying it
    #[arg(long)]
    pub dry_run: bool,

    /// Also write the resulting object to a file
    #[arg(long)]
    pub output: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReclaimSpec {
    api_version: String,
    kind: String,
    plural: String,
    group: String,
    project_slug: String,
    composed_resources: Vec<ResourceRef>,
    live_resolver: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
struct ResourceRef {
    api_version: String,
    kind: String,
}

#[derive(Debug)]
struct ReclaimReport {
    spec: ReclaimSpec,
    live_notes: Vec<String>,
    cluster_notes: Vec<String>,
    source: ManifestSource,
}

#[derive(Debug, Clone)]
struct ManagedResourcePatch {
    api_version: String,
    kind: String,
    namespace: String,
    name: String,
    external_name: Option<String>,
    management_policies: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy)]
enum ManifestSource {
    Cluster,
    Generated,
}

pub fn run(args: &XrArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        XrCommand::Observe(observe_args) => run_observe(observe_args),
        XrCommand::Manage(manage_args) => run_manage(manage_args),
        XrCommand::Adopt(adopt_args) => run_adopt(adopt_args),
    }
}

fn run_observe(args: &ObserveArgs) -> Result<(), Box<dyn Error>> {
    let specs = load_specs()?;
    let spec = match_spec(&specs, &args.kind)?;

    let mut manifest = render_manifest(spec, &args.name, &args.namespace)?;
    sanitize_manifest_defaults(spec, &mut manifest, ManifestSource::Generated);
    let live_notes = apply_live_aws(spec, &mut manifest, &args.name, &args.aws_region)?;
    set_observe_only_management(&mut manifest);

    emit_report(
        spec,
        &manifest,
        &live_notes,
        &["generated bootstrap observe-only manifest".to_string()],
        ManifestSource::Generated,
        args.output.as_deref(),
        false,
        "observe manifest",
    )
}

fn run_manage(args: &ManageXrArgs) -> Result<(), Box<dyn Error>> {
    let specs = load_specs()?;
    let spec = match_spec(&specs, &args.kind)?;
    let (mut manifest, source, cluster_notes) = load_existing_cluster_manifest(spec, &args.name, &args.namespace)?;
    strip_manage_only_fields(&mut manifest);
    let live_notes = apply_observed_cluster(spec, &mut manifest, &args.name, &args.namespace)?;
    prune_empty_maps(&mut manifest);
    set_adopt_management(&mut manifest);
    validate_manage_manifest(spec, &manifest)?;

    emit_report(
        spec,
        &manifest,
        &live_notes,
        &cluster_notes,
        source,
        args.output.as_deref(),
        false,
        "managed XR manifest",
    )
}

fn run_adopt(args: &AdoptArgs) -> Result<(), Box<dyn Error>> {
    let specs = load_specs()?;
    let spec = match_spec(&specs, &args.kind)?;
    let _ = load_existing_cluster_manifest(spec, &args.name, &args.namespace)?;
    let patches = build_managed_resource_adoption_patches(spec, &args.name)?;

    if patches.is_empty() {
        log::info!("no managed resources require adoption patches");
        return Ok(());
    }

    let patch_yaml = render_managed_resource_patches(&patches)?;

    if let Some(output) = &args.output {
        fs::write(output, &patch_yaml)?;
        log::info!("managed-resource adoption patches written to {output}");
    }

    if args.dry_run {
        if args.output.is_none() {
            print!("{patch_yaml}");
        }
        return Ok(());
    }

    kubectl_apply_stdin(&patch_yaml)?;
    log::info!("applied {} managed-resource adoption patches", patches.len());
    Ok(())
}

fn emit_report(
    spec: &ReclaimSpec,
    manifest: &Value,
    live_notes: &[String],
    cluster_notes: &[String],
    source: ManifestSource,
    output: Option<&str>,
    apply: bool,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    let mut manifest_yaml = serde_yaml::to_string(manifest)?;
    if manifest_yaml.starts_with("---\n") {
        manifest_yaml = manifest_yaml.replacen("---\n", "", 1);
    }

    let report = ReclaimReport {
        spec: spec.clone(),
        live_notes: live_notes.to_vec(),
        cluster_notes: cluster_notes.to_vec(),
        source,
    };

    log_report(&report, true);

    if let Some(output) = output {
        fs::write(output, &manifest_yaml)?;
        log::info!("{label} written to {output}");
    }

    if apply {
        kubectl_apply_stdin(&manifest_yaml)?;
        log::info!("{label} applied to cluster");
    } else if output.is_none() {
        print!("{manifest_yaml}");
    }

    Ok(())
}

fn load_specs() -> Result<Vec<ReclaimSpec>, Box<dyn Error>> {
    Ok(serde_json::from_str(EMBEDDED_RECLAIM_METADATA)?)
}

fn match_spec<'a>(specs: &'a [ReclaimSpec], needle: &str) -> Result<&'a ReclaimSpec, Box<dyn Error>> {
    let needle_lower = normalize_identity(needle);
    let matches: Vec<&ReclaimSpec> = specs
        .iter()
        .filter(|spec| {
            [
                spec.kind.as_str(),
                spec.plural.as_str(),
                spec.project_slug.as_str(),
                spec.group.as_str(),
            ]
            .into_iter()
            .any(|candidate| normalize_identity(candidate) == needle_lower)
        })
        .collect();

    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(format!("no XR found matching '{needle}'").into()),
        _ => Err(format!("multiple XRs match '{needle}'").into()),
    }
}

fn render_manifest(
    spec: &ReclaimSpec,
    object_name: &str,
    namespace: &str,
) -> Result<Value, Box<dyn Error>> {
    let mut root = Mapping::new();
    root.insert(vs("apiVersion"), vs(&spec.api_version));
    root.insert(vs("kind"), vs(&spec.kind));
    root.insert(vs("metadata"), Value::Mapping(Mapping::new()));
    root.insert(vs("spec"), Value::Mapping(Mapping::new()));
    let mut doc = Value::Mapping(root);

    let root = doc
        .as_mapping_mut()
        .ok_or("reclaim manifest root must be a YAML mapping")?;
    root.insert(vs("apiVersion"), vs(&spec.api_version));
    root.insert(vs("kind"), vs(&spec.kind));
    let metadata = ensure_mapping(root, "metadata");
    metadata.insert(vs("name"), vs(object_name));
    metadata.insert(vs("namespace"), vs(namespace));

    Ok(doc)
}

fn load_existing_cluster_manifest(
    spec: &ReclaimSpec,
    object_name: &str,
    namespace: &str,
) -> Result<(Value, ManifestSource, Vec<String>), Box<dyn Error>> {
    let resource = format!("{}.{}", spec.plural, spec.group);
    let mut notes = Vec::new();

    match run_cmd_output(
        "kubectl",
        &["get", &resource, object_name, "-n", namespace, "-o", "yaml"],
    ) {
        Ok(yaml) => {
            let mut value: Value = serde_yaml::from_str(&yaml)?;
            strip_runtime_k8s_fields(&mut value);
            notes.push("loaded existing XR from cluster".to_string());
            Ok((value, ManifestSource::Cluster, notes))
        }
        Err(err) => Err(format!(
            "failed to inspect live XR from cluster: {}",
            err
        )
        .into()),
    }
}

fn sanitize_manifest_defaults(spec: &ReclaimSpec, manifest: &mut Value, source: ManifestSource) {
    let Some(root) = manifest.as_mapping_mut() else {
        return;
    };
    let spec_map = ensure_mapping(root, "spec");

    if spec.kind == "AutoEKSCluster" && !matches!(source, ManifestSource::Cluster) {
        spec_map.remove(vs("tags"));

        let provider_config = ensure_mapping(spec_map, "providerConfigRef");
        provider_config.insert(vs("name"), vs("default"));
        provider_config.insert(vs("kind"), vs("ProviderConfig"));
    }
}

fn prune_empty_maps(value: &mut Value) -> bool {
    match value {
        Value::Mapping(map) => {
            let keys = map.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                let remove = map
                    .get_mut(&key)
                    .map(prune_empty_maps)
                    .unwrap_or(false);
                if remove {
                    map.remove(&key);
                }
            }
            map.is_empty()
        }
        Value::Sequence(items) => {
            items.retain_mut(|item| !prune_empty_maps(item));
            items.is_empty()
        }
        _ => false,
    }
}

fn set_observe_only_management(manifest: &mut Value) {
    let Some(root) = manifest.as_mapping_mut() else {
        return;
    };
    let spec = ensure_mapping(root, "spec");
    spec.insert(
        vs("managementPolicies"),
        Value::Sequence(vec![vs("Observe"), vs("LateInitialize")]),
    );
}

fn set_adopt_management(manifest: &mut Value) {
    let Some(root) = manifest.as_mapping_mut() else {
        return;
    };
    let spec = ensure_mapping(root, "spec");
    spec.insert(vs("managementPolicies"), Value::Sequence(vec![vs("*")]));
}

fn strip_runtime_k8s_fields(value: &mut Value) {
    let Some(root) = value.as_mapping_mut() else {
        return;
    };
    root.remove(vs("status"));

    let metadata = ensure_mapping(root, "metadata");
    metadata.remove(vs("creationTimestamp"));
    metadata.remove(vs("deletionGracePeriodSeconds"));
    metadata.remove(vs("deletionTimestamp"));
    metadata.remove(vs("generation"));
    metadata.remove(vs("managedFields"));
    metadata.remove(vs("resourceVersion"));
    metadata.remove(vs("selfLink"));
    metadata.remove(vs("uid"));
}

fn strip_manage_only_fields(value: &mut Value) {
    let Some(root) = value.as_mapping_mut() else {
        return;
    };

    if let Some(metadata) = root.get_mut(vs("metadata")).and_then(Value::as_mapping_mut) {
        metadata.remove(vs("annotations"));
        metadata.remove(vs("finalizers"));
        metadata.remove(vs("labels"));
    }

    if let Some(spec) = root.get_mut(vs("spec")).and_then(Value::as_mapping_mut) {
        spec.remove(vs("crossplane"));
    }
}

fn apply_live_aws(
    spec: &ReclaimSpec,
    manifest: &mut Value,
    selector_name: &str,
    region: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    match spec.live_resolver.as_deref() {
        Some("aws-network-by-tag") => apply_live_aws_network(manifest, selector_name, region),
        Some("aws-autoekscluster") => apply_live_aws_autoekscluster(manifest, selector_name, region),
        Some(resolver) => Err(format!("live AWS resolver '{resolver}' is not implemented").into()),
        None => Err(format!("{} has no live AWS resolver", spec.kind).into()),
    }
}

fn apply_observed_cluster(
    spec: &ReclaimSpec,
    manifest: &mut Value,
    xr_name: &str,
    namespace: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    match spec.kind.as_str() {
        "AutoEKSCluster" => apply_observed_autoekscluster(manifest, xr_name, namespace),
        _ => Ok(Vec::new()),
    }
}

fn apply_observed_autoekscluster(
    manifest: &mut Value,
    xr_name: &str,
    _namespace: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    let managed_json = run_cmd_output(
        "kubectl",
        &[
            "get",
            "managed",
            "-A",
            "-l",
            &format!("crossplane.io/composite={xr_name}"),
            "-o",
            "json",
        ],
    )?;
    let managed: JsonValue = serde_json::from_str(&managed_json)?;
    let items = managed
        .get("items")
        .and_then(JsonValue::as_array)
        .ok_or("kubectl managed output missing items")?;

    let mut by_comp_name: HashMap<String, &JsonValue> = HashMap::new();
    for item in items {
        if let Some(name) = composition_resource_name(item) {
            by_comp_name.insert(name.to_string(), item);
        }
    }

    if by_comp_name.is_empty() {
        return Err(format!(
            "no composed managed resources found for XR '{}'; apply the observe XR first and wait for composed resources to appear",
            xr_name
        )
        .into());
    }

    let root = manifest
        .as_mapping_mut()
        .ok_or("manifest root must be a mapping")?;
    let spec = ensure_mapping(root, "spec");
    let mut notes = Vec::new();

    if let Some(cluster) = by_comp_name.get("cluster") {
        if let Some(external_name) = external_name_annotation(cluster) {
            spec.insert(vs("externalName"), vs(external_name));
            notes.push(format!("externalName <- managed cluster annotation ({external_name})"));
        }

        if let Some(cluster_name) = get_json_path(cluster, &["spec", "forProvider", "roleArn"])
            .and_then(JsonValue::as_str)
            .and_then(account_id_from_arn)
        {
            spec.insert(vs("accountId"), vs(&cluster_name));
            notes.push(format!("accountId <- managed cluster roleArn ({cluster_name})"));
        } else if let Some(cluster_arn) = get_json_path(cluster, &["status", "atProvider", "arn"])
            .and_then(JsonValue::as_str)
            .and_then(account_id_from_arn)
        {
            spec.insert(vs("accountId"), vs(&cluster_arn));
            notes.push(format!("accountId <- managed cluster arn ({cluster_arn})"));
        }

        if let Some(cluster_name) = get_json_path(cluster, &["status", "atProvider", "id"])
            .and_then(JsonValue::as_str)
            .or_else(|| external_name_annotation(cluster))
        {
            spec.insert(vs("clusterName"), vs(cluster_name));
            notes.push(format!("clusterName <- managed cluster identity ({cluster_name})"));
        }

        if let Some(region) = get_json_path(cluster, &["spec", "forProvider", "region"]).and_then(JsonValue::as_str) {
            spec.insert(vs("region"), vs(region));
            notes.push(format!("region <- managed cluster spec.forProvider ({region})"));
        }

        if let Some(version) = get_json_path(cluster, &["spec", "forProvider", "version"]).and_then(JsonValue::as_str) {
            spec.insert(vs("version"), vs(version));
            notes.push(format!("version <- managed cluster spec.forProvider ({version})"));
        }

        if let Some(subnets) = get_json_path(cluster, &["spec", "forProvider", "vpcConfig", "subnetIds"])
            .and_then(JsonValue::as_array)
        {
            let values = subnets.iter().filter_map(JsonValue::as_str).map(vs).collect::<Vec<_>>();
            if !values.is_empty() {
                spec.insert(vs("subnetIds"), Value::Sequence(values));
                notes.push("subnetIds <- managed cluster spec.forProvider".to_string());
            }
        }

        if let Some(private_access) =
            get_json_path(cluster, &["spec", "forProvider", "vpcConfig", "endpointPrivateAccess"])
                .and_then(JsonValue::as_bool)
        {
            spec.insert(vs("privateAccess"), Value::Bool(private_access));
            notes.push(format!(
                "privateAccess <- managed cluster spec.forProvider ({private_access})"
            ));
        }
        if let Some(public_access) =
            get_json_path(cluster, &["spec", "forProvider", "vpcConfig", "endpointPublicAccess"])
                .and_then(JsonValue::as_bool)
        {
            spec.insert(vs("publicAccess"), Value::Bool(public_access));
            notes.push(format!(
                "publicAccess <- managed cluster spec.forProvider ({public_access})"
            ));
        }

        let encryption_enabled = get_json_path(cluster, &["spec", "forProvider", "encryptionConfig"])
            .map(|value| match value {
                JsonValue::Array(items) => !items.is_empty(),
                JsonValue::Object(map) => !map.is_empty(),
                _ => false,
            })
            .unwrap_or(false);
        spec.insert(vs("encryptionEnabled"), Value::Bool(encryption_enabled));
        notes.push(format!(
            "encryptionEnabled <- managed cluster spec.forProvider ({encryption_enabled})"
        ));

        if let Some(provider_name) =
            get_json_path(cluster, &["spec", "providerConfigRef", "name"]).and_then(JsonValue::as_str)
        {
            let provider_ref = ensure_mapping(spec, "providerConfigRef");
            provider_ref.insert(vs("name"), vs(provider_name));
            provider_ref.insert(vs("kind"), vs("ProviderConfig"));
            notes.push(format!("providerConfigRef.name <- managed cluster ({provider_name})"));
        }

        if let Some(custom_tags) = get_json_path(cluster, &["status", "atProvider", "tags"])
            .and_then(JsonValue::as_object)
            .and_then(extract_custom_autoeks_tags)
        {
            if !custom_tags.is_empty() {
                spec.insert(vs("tags"), json_object_to_yaml_mapping(&custom_tags)?);
                notes.push("tags <- managed cluster observed custom tags".to_string());
            }
        }
    }

    if let Some(cp_role) = by_comp_name.get("iam-role-controlplane") {
        if let Some(role_name) = external_name_annotation(cp_role)
            .or_else(|| get_json_path(cp_role, &["status", "atProvider", "name"]).and_then(JsonValue::as_str))
            .or_else(|| get_json_path(cp_role, &["status", "atProvider", "id"]).and_then(JsonValue::as_str))
        {
            ensure_mapping(ensure_mapping(spec, "iam"), "controlPlaneRole")
                .insert(vs("externalName"), vs(role_name));
            notes.push(format!(
                "iam.controlPlaneRole.externalName <- observed managed role ({role_name})"
            ));
        }
    }

    if let Some(node_role) = by_comp_name.get("iam-role-node") {
        if let Some(role_name) = external_name_annotation(node_role)
            .or_else(|| get_json_path(node_role, &["status", "atProvider", "name"]).and_then(JsonValue::as_str))
            .or_else(|| get_json_path(node_role, &["status", "atProvider", "id"]).and_then(JsonValue::as_str))
        {
            ensure_mapping(ensure_mapping(spec, "iam"), "nodeRole")
                .insert(vs("externalName"), vs(role_name));
            notes.push(format!(
                "iam.nodeRole.externalName <- observed managed role ({role_name})"
            ));
        }
    }

    if let Some(kms) = by_comp_name.get("kms-key") {
        if let Some(key_id) = external_name_annotation(kms)
            .or_else(|| get_json_path(kms, &["status", "atProvider", "keyId"]).and_then(JsonValue::as_str))
            .or_else(|| get_json_path(kms, &["status", "atProvider", "id"]).and_then(JsonValue::as_str))
        {
            ensure_mapping(spec, "kms").insert(vs("externalName"), vs(key_id));
            notes.push(format!("kms.externalName <- observed managed key ({key_id})"));
        }
    }

    if let Some(oidc) = by_comp_name.get("oidc-provider") {
        if let Some(provider_arn) = external_name_annotation(oidc)
            .or_else(|| get_json_path(oidc, &["status", "atProvider", "arn"]).and_then(JsonValue::as_str))
        {
            let oidc_spec = ensure_mapping(spec, "oidc");
            oidc_spec.insert(vs("enabled"), Value::Bool(true));
            oidc_spec.insert(vs("externalName"), vs(provider_arn));
            notes.push(format!(
                "oidc.externalName <- observed managed provider ({provider_arn})"
            ));
        }
    }

    if let Some(k8s_provider_config) = by_comp_name.get("k8s-provider-config") {
        if let Some(provider_name) =
            get_json_path(k8s_provider_config, &["spec", "providerConfigRef", "name"]).and_then(JsonValue::as_str)
        {
            let provider_ref = ensure_mapping(spec, "kubernetesProviderConfigRef");
            provider_ref.insert(vs("name"), vs(provider_name));
            provider_ref.insert(vs("kind"), vs("ProviderConfig"));
            notes.push(format!(
                "kubernetesProviderConfigRef.name <- observed provider-config object ({provider_name})"
            ));
        }
    }

    apply_observed_autoekscluster_node_config(spec, &by_comp_name, &mut notes)?;

    Ok(notes)
}

fn apply_observed_autoekscluster_node_config(
    spec: &mut Mapping,
    by_comp_name: &HashMap<String, &JsonValue>,
    notes: &mut Vec<String>,
) -> Result<(), Box<dyn Error>> {
    let nodeclass_spec = by_comp_name
        .get("nodeclass")
        .and_then(|item| observed_object_manifest_spec(item));
    let nodepool_spec = by_comp_name
        .get("nodepool")
        .and_then(|item| observed_object_manifest_spec(item));

    if nodeclass_spec.is_none() && nodepool_spec.is_none() {
        return Ok(());
    }

    let node_config = ensure_mapping(spec, "nodeConfig");
    node_config.insert(vs("enabled"), Value::Bool(true));
    notes.push("nodeConfig.enabled <- observed node resources".to_string());

    if let Some(nodeclass) = by_comp_name.get("nodeclass") {
        if let Some(manifest) = observed_object_manifest(nodeclass) {
            let nodeclass_cfg = ensure_mapping(node_config, "nodeClass");

            if let Some(name) = manifest
                .get("metadata")
                .and_then(|m| m.get("name"))
                .and_then(JsonValue::as_str)
            {
                nodeclass_cfg.insert(vs("name"), vs(name));
                notes.push(format!("nodeConfig.nodeClass.name <- observed NodeClass ({name})"));
            }

            if let Some(ephemeral_storage) = manifest
                .get("spec")
                .and_then(|s| s.get("ephemeralStorage"))
                .and_then(JsonValue::as_object)
            {
                let storage_cfg = ensure_mapping(nodeclass_cfg, "ephemeralStorage");

                if let Some(size) = ephemeral_storage.get("size").and_then(JsonValue::as_str) {
                    storage_cfg.insert(vs("size"), vs(size));
                }
                if let Some(iops) = ephemeral_storage.get("iops").and_then(JsonValue::as_i64) {
                    storage_cfg.insert(vs("iops"), Value::Number(iops.into()));
                }
                if let Some(throughput) =
                    ephemeral_storage.get("throughput").and_then(JsonValue::as_i64)
                {
                    storage_cfg.insert(vs("throughput"), Value::Number(throughput.into()));
                }
                notes.push("nodeConfig.nodeClass.ephemeralStorage <- observed NodeClass".to_string());
            }
        }
    }

    if let Some(nodepool) = by_comp_name.get("nodepool") {
        if let Some(manifest) = observed_object_manifest(nodepool) {
            let nodepool_cfg = ensure_mapping(node_config, "nodePool");
            nodepool_cfg.insert(vs("enabled"), Value::Bool(true));

            if let Some(name) = manifest
                .get("metadata")
                .and_then(|m| m.get("name"))
                .and_then(JsonValue::as_str)
            {
                nodepool_cfg.insert(vs("name"), vs(name));
                notes.push(format!("nodeConfig.nodePool.name <- observed NodePool ({name})"));
            }

            if let Some(expire_after) = manifest
                .get("spec")
                .and_then(|s| s.get("template"))
                .and_then(|t| t.get("spec"))
                .and_then(|s| s.get("expireAfter"))
                .and_then(JsonValue::as_str)
            {
                nodepool_cfg.insert(vs("expireAfter"), vs(expire_after));
            }

            if let Some(requirements) = manifest
                .get("spec")
                .and_then(|s| s.get("template"))
                .and_then(|t| t.get("spec"))
                .and_then(|s| s.get("requirements"))
            {
                nodepool_cfg.insert(vs("requirements"), serde_yaml::to_value(requirements.clone())?);
            }

            if let Some(disruption) = manifest.get("spec").and_then(|s| s.get("disruption")) {
                nodepool_cfg.insert(vs("disruption"), serde_yaml::to_value(disruption.clone())?);
                notes.push("nodeConfig.nodePool.disruption <- observed NodePool".to_string());
            }
        }
    }

    Ok(())
}

fn build_managed_resource_adoption_patches(
    spec: &ReclaimSpec,
    xr_name: &str,
) -> Result<Vec<ManagedResourcePatch>, Box<dyn Error>> {
    match spec.kind.as_str() {
        "AutoEKSCluster" => build_autoekscluster_adoption_patches(xr_name),
        _ => Ok(Vec::new()),
    }
}

fn build_autoekscluster_adoption_patches(
    xr_name: &str,
) -> Result<Vec<ManagedResourcePatch>, Box<dyn Error>> {
    let managed_json = run_cmd_output(
        "kubectl",
        &[
            "get",
            "managed",
            "-A",
            "-l",
            &format!("crossplane.io/composite={xr_name}"),
            "-o",
            "json",
        ],
    )?;
    let managed: JsonValue = serde_json::from_str(&managed_json)?;
    let items = managed
        .get("items")
        .and_then(JsonValue::as_array)
        .ok_or("kubectl managed output missing items")?;

    let mut patches = Vec::new();
    for item in items {
        let kind = item.get("kind").and_then(JsonValue::as_str).unwrap_or_default();
        let name = item
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(JsonValue::as_str)
            .ok_or("managed resource missing metadata.name")?;
        let namespace = item
            .get("metadata")
            .and_then(|m| m.get("namespace"))
            .and_then(JsonValue::as_str)
            .unwrap_or("default");
        let api_version = item
            .get("apiVersion")
            .and_then(JsonValue::as_str)
            .ok_or("managed resource missing apiVersion")?;

        let mut patch = ManagedResourcePatch {
            api_version: api_version.to_string(),
            kind: kind.to_string(),
            namespace: namespace.to_string(),
            name: name.to_string(),
            external_name: None,
            management_policies: None,
        };

        if external_name_annotation(item).is_none() && kind == "RolePolicyAttachment" {
            let role = get_json_path(item, &["spec", "forProvider", "role"])
                .and_then(JsonValue::as_str)
                .or_else(|| {
                    get_json_path(item, &["spec", "forProvider", "roleRef", "name"])
                        .and_then(JsonValue::as_str)
                })
                .ok_or("RolePolicyAttachment missing role identity")?;
            let policy_arn = get_json_path(item, &["spec", "forProvider", "policyArn"])
                .and_then(JsonValue::as_str)
                .ok_or("RolePolicyAttachment missing policyArn")?;
            patch.external_name = Some(format!("{role}/{policy_arn}"));
        }

        if kind == "Object"
            && matches!(
                composition_resource_name(item),
                Some("k8s-provider-config") | Some("helm-provider-config")
            )
            && is_observe_only(item)
        {
            patch.management_policies = Some(vec![
                "Create".to_string(),
                "Observe".to_string(),
                "Update".to_string(),
                "LateInitialize".to_string(),
            ]);
        }

        if patch.external_name.is_some() || patch.management_policies.is_some() {
            patches.push(patch);
        }
    }

    Ok(patches)
}

fn render_managed_resource_patches(
    patches: &[ManagedResourcePatch],
) -> Result<String, Box<dyn Error>> {
    let mut docs = Vec::new();
    for patch in patches {
        let mut metadata = Mapping::new();
        metadata.insert(vs("name"), vs(&patch.name));
        metadata.insert(vs("namespace"), vs(&patch.namespace));

        let mut root = Mapping::new();
        root.insert(vs("apiVersion"), vs(&patch.api_version));
        root.insert(vs("kind"), vs(&patch.kind));

        if let Some(external_name) = &patch.external_name {
            let mut annotations = Mapping::new();
            annotations.insert(vs("crossplane.io/external-name"), vs(external_name));
            metadata.insert(vs("annotations"), Value::Mapping(annotations));
        }

        root.insert(vs("metadata"), Value::Mapping(metadata));

        if let Some(management_policies) = &patch.management_policies {
            let mut spec = Mapping::new();
            spec.insert(
                vs("managementPolicies"),
                Value::Sequence(management_policies.iter().map(|policy| vs(policy)).collect()),
            );
            root.insert(vs("spec"), Value::Mapping(spec));
        }

        docs.push(Value::Mapping(root));
    }

    let mut out = String::new();
    for doc in docs {
        if !out.is_empty() {
            out.push_str("---\n");
        }
        let mut yaml = serde_yaml::to_string(&doc)?;
        if yaml.starts_with("---\n") {
            yaml = yaml.replacen("---\n", "", 1);
        }
        out.push_str(&yaml);
    }
    Ok(out)
}

fn composition_resource_name<'a>(item: &'a JsonValue) -> Option<&'a str> {
    item.get("metadata")
        .and_then(|m| m.get("annotations"))
        .and_then(JsonValue::as_object)
        .and_then(|ann| {
            ann.get("gotemplating.fn.crossplane.io/composition-resource-name")
                .or_else(|| ann.get("crossplane.io/composition-resource-name"))
        })
        .and_then(JsonValue::as_str)
}

fn external_name_annotation<'a>(item: &'a JsonValue) -> Option<&'a str> {
    item.get("metadata")
        .and_then(|m| m.get("annotations"))
        .and_then(|ann| ann.get("crossplane.io/external-name"))
        .and_then(JsonValue::as_str)
}

fn is_observe_only(item: &JsonValue) -> bool {
    let Some(policies) = get_json_path(item, &["spec", "managementPolicies"]).and_then(JsonValue::as_array) else {
        return false;
    };

    let mut values = policies.iter().filter_map(JsonValue::as_str).collect::<Vec<_>>();
    values.sort_unstable();
    values == ["LateInitialize", "Observe"]
}

fn observed_object_manifest<'a>(item: &'a JsonValue) -> Option<&'a JsonValue> {
    get_json_path(item, &["status", "atProvider", "manifest"])
        .or_else(|| get_json_path(item, &["spec", "forProvider", "manifest"]))
}

fn observed_object_manifest_spec<'a>(item: &'a JsonValue) -> Option<&'a JsonValue> {
    observed_object_manifest(item)?.get("spec")
}

fn get_json_path<'a>(value: &'a JsonValue, path: &[&str]) -> Option<&'a JsonValue> {
    let mut current = value;
    for segment in path {
        current = match current {
            JsonValue::Object(map) => map.get(*segment)?,
            JsonValue::Array(items) => items.first()?,
            _ => return None,
        };
    }
    Some(current)
}

fn has_path(value: &Value, path: &str) -> bool {
    let mut current = value;
    for segment in path.split('.') {
        let key = segment.strip_suffix("[]").unwrap_or(segment);
        current = match current {
            Value::Mapping(map) => match map.get(vs(key)) {
                Some(next) => next,
                None => return false,
            },
            Value::Sequence(items) => match items.first() {
                Some(next) => next,
                None => return false,
            },
            _ => return false,
        };
    }
    true
}

fn apply_live_aws_autoekscluster(
    manifest: &mut Value,
    selector_name: &str,
    region: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut notes = Vec::new();
    let root = manifest
        .as_mapping_mut()
        .ok_or("manifest root must be a mapping")?;
    let spec = ensure_mapping(root, "spec");

    let cluster_json = aws_json(&[
        "eks",
        "describe-cluster",
        "--name",
        selector_name,
        "--region",
        region,
    ])?;
    let cluster = cluster_json
        .get("cluster")
        .ok_or_else(|| format!("AWS did not return cluster details for '{selector_name}'"))?;

    spec.insert(vs("externalName"), vs(selector_name));
    spec.insert(vs("clusterName"), vs(selector_name));
    spec.insert(vs("region"), vs(region));
    notes.push(format!("externalName <- {selector_name}"));

    if let Some(version) = cluster.get("version").and_then(JsonValue::as_str) {
        spec.insert(vs("version"), vs(version));
        notes.push(format!("version <- {version}"));
    }

    let account_id = aws_json(&["sts", "get-caller-identity"])?
        .get("Account")
        .and_then(JsonValue::as_str)
        .map(ToString::to_string)
        .ok_or("unable to determine AWS account ID from sts get-caller-identity")?;
    spec.insert(vs("accountId"), vs(&account_id));
    notes.push(format!("accountId <- {account_id}"));

    if let Some(subnets) = cluster
        .get("resourcesVpcConfig")
        .and_then(|cfg| cfg.get("subnetIds"))
        .and_then(JsonValue::as_array)
    {
        let subnet_values = subnets
            .iter()
            .filter_map(JsonValue::as_str)
            .map(vs)
            .collect::<Vec<_>>();
        if !subnet_values.is_empty() {
            spec.insert(vs("subnetIds"), Value::Sequence(subnet_values));
            notes.push("subnetIds <- eks cluster VPC config".to_string());
        }
    }

    if let Some(vpc_cfg) = cluster.get("resourcesVpcConfig") {
        if let Some(private_access) = vpc_cfg.get("endpointPrivateAccess").and_then(JsonValue::as_bool) {
            spec.insert(vs("privateAccess"), Value::Bool(private_access));
            notes.push(format!("privateAccess <- {private_access}"));
        }
        if let Some(public_access) = vpc_cfg.get("endpointPublicAccess").and_then(JsonValue::as_bool) {
            spec.insert(vs("publicAccess"), Value::Bool(public_access));
            notes.push(format!("publicAccess <- {public_access}"));
        }
    }

    let encryption_enabled = cluster
        .get("encryptionConfig")
        .and_then(JsonValue::as_array)
        .map(|configs| !configs.is_empty())
        .unwrap_or(false);
    spec.insert(vs("encryptionEnabled"), Value::Bool(encryption_enabled));
    notes.push(format!("encryptionEnabled <- {encryption_enabled}"));

    if let Some(role_arn) = cluster.get("roleArn").and_then(JsonValue::as_str) {
        let control_plane_role = role_name_from_arn(role_arn)
            .ok_or_else(|| format!("unable to parse control plane role name from ARN '{role_arn}'"))?;
        ensure_mapping(ensure_mapping(spec, "iam"), "controlPlaneRole")
            .insert(vs("externalName"), vs(&control_plane_role));
        notes.push(format!("iam.controlPlaneRole.externalName <- {control_plane_role}"));
    }

    let derived_node_role = format!("{selector_name}-node");
    if aws_iam_role_exists(&derived_node_role)? {
        ensure_mapping(ensure_mapping(spec, "iam"), "nodeRole")
            .insert(vs("externalName"), vs(&derived_node_role));
        notes.push(format!(
            "iam.nodeRole.externalName <- {derived_node_role} (derived from cluster naming convention)"
        ));
    } else {
        return Err(format!(
            "unable to locate IAM node role '{}'; AutoEKS reclaim expects the composed node role to exist",
            derived_node_role
        )
        .into());
    }

    if let Some(kms_key_arn) = cluster
        .get("encryptionConfig")
        .and_then(JsonValue::as_array)
        .and_then(|configs| configs.first())
        .and_then(|cfg| cfg.get("provider"))
        .and_then(|provider| provider.get("keyArn"))
        .and_then(JsonValue::as_str)
    {
        if let Some(key_id) = kms_key_id_from_arn(kms_key_arn) {
            ensure_mapping(spec, "kms").insert(vs("externalName"), vs(&key_id));
            notes.push(format!("kms.externalName <- {key_id}"));
        }
    }

    if let Some(issuer) = cluster
        .get("identity")
        .and_then(|identity| identity.get("oidc"))
        .and_then(|oidc| oidc.get("issuer"))
        .and_then(JsonValue::as_str)
    {
        if let Some(provider_arn) = find_oidc_provider_arn(issuer)? {
            let oidc = ensure_mapping(spec, "oidc");
            oidc.insert(vs("enabled"), Value::Bool(true));
            oidc.insert(vs("externalName"), vs(&provider_arn));
            notes.push(format!("oidc.externalName <- {provider_arn}"));
        }
    }

    Ok(notes)
}

fn apply_live_aws_network(
    manifest: &mut Value,
    selector_name: &str,
    region: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut notes = Vec::new();
    let root = manifest
        .as_mapping_mut()
        .ok_or("manifest root must be a mapping")?;
    let spec = ensure_mapping(root, "spec");
    spec.insert(vs("region"), vs(region));

    let vpc_json = aws_json(&[
        "ec2",
        "describe-vpcs",
        "--region",
        region,
        "--filters",
        &format!("Name=tag:{NETWORK_TAG_KEY},Values={selector_name}"),
    ])?;
    let vpc_id = vpc_json
        .get("Vpcs")
        .and_then(JsonValue::as_array)
        .and_then(|vpcs| vpcs.first())
        .and_then(|vpc| vpc.get("VpcId"))
        .and_then(JsonValue::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| format!("no VPC found in AWS for {NETWORK_TAG_KEY}={selector_name}"))?;

    let vpc = ensure_mapping(spec, "vpc");
    vpc.insert(vs("externalName"), vs(&vpc_id));
    notes.push(format!("vpc.externalName <- {vpc_id}"));

    let igw_json = aws_json(&[
        "ec2",
        "describe-internet-gateways",
        "--region",
        region,
        "--filters",
        &format!("Name=tag:{NETWORK_TAG_KEY},Values={selector_name}"),
    ])?;
    if let Some(igw_id) = igw_json
        .get("InternetGateways")
        .and_then(JsonValue::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("InternetGatewayId"))
        .and_then(JsonValue::as_str)
    {
        ensure_mapping(spec, "internetGateway").insert(vs("externalName"), vs(igw_id));
        notes.push(format!("internetGateway.externalName <- {igw_id}"));
    }

    let eigw_json = aws_json(&["ec2", "describe-egress-only-internet-gateways", "--region", region])?;
    if let Some(eigw_id) = eigw_json
        .get("EgressOnlyInternetGateways")
        .and_then(JsonValue::as_array)
        .and_then(|items| {
            items.iter().find(|item| has_tag(item, NETWORK_TAG_KEY, selector_name))
        })
        .and_then(|item| item.get("EgressOnlyInternetGatewayId"))
        .and_then(JsonValue::as_str)
    {
        ensure_mapping(spec, "egressOnlyInternetGateway").insert(vs("externalName"), vs(eigw_id));
        notes.push(format!("egressOnlyInternetGateway.externalName <- {eigw_id}"));
    }

    let subnets_json = aws_json(&[
        "ec2",
        "describe-subnets",
        "--region",
        region,
        "--filters",
        &format!("Name=tag:{NETWORK_TAG_KEY},Values={selector_name}"),
    ])?;
    let subnet_items = subnets_json
        .get("Subnets")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();

    let mut subnet_names = Mapping::new();
    let mut subnet_lookup = BTreeMap::new();
    for subnet in &subnet_items {
        let Some(subnet_id) = subnet.get("SubnetId").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some(zone) = subnet.get("AvailabilityZone").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some(tier) = tag_value(subnet, SUBNET_TIER_TAG_KEY) else {
            continue;
        };
        let Some(az_suffix) = zone.chars().last() else {
            continue;
        };
        let key = format!("{tier}-{az_suffix}");
        subnet_names.insert(vs(&key), vs(subnet_id));
        subnet_lookup.insert(subnet_id.to_string(), (tier.to_string(), az_suffix));
    }
    if !subnet_names.is_empty() {
        ensure_mapping(spec, "subnetLayout").insert(vs("externalNames"), Value::Mapping(subnet_names));
        notes.push("subnetLayout.externalNames <- live AWS tags".to_string());
    }

    let route_tables_json = aws_json(&[
        "ec2",
        "describe-route-tables",
        "--region",
        region,
        "--filters",
        &format!("Name=tag:{NETWORK_TAG_KEY},Values={selector_name}"),
    ])?;
    let route_table_items = route_tables_json
        .get("RouteTables")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();

    let mut route_table_names = Mapping::new();
    let mut association_names = Mapping::new();
    for route_table in &route_table_items {
        let Some(route_table_id) = route_table.get("RouteTableId").and_then(JsonValue::as_str) else {
            continue;
        };
        let tier = tag_value(route_table, SUBNET_TIER_TAG_KEY);
        let az_tag = tag_value(route_table, ROUTE_TABLE_AZ_TAG_KEY);
        let key = match (tier, az_tag) {
            (Some("public"), _) => Some("public".to_string()),
            (Some("private"), Some(az)) => Some(format!("private-{az}")),
            _ => None,
        };
        if let Some(key) = key {
            route_table_names.insert(vs(&key), vs(route_table_id));
        }

        if let Some(associations) = route_table.get("Associations").and_then(JsonValue::as_array) {
            for assoc in associations {
                if assoc.get("Main").and_then(JsonValue::as_bool).unwrap_or(false) {
                    continue;
                }
                let Some(assoc_id) = assoc.get("RouteTableAssociationId").and_then(JsonValue::as_str) else {
                    continue;
                };
                let Some(subnet_id) = assoc.get("SubnetId").and_then(JsonValue::as_str) else {
                    continue;
                };
                if let Some((tier, az_suffix)) = subnet_lookup.get(subnet_id) {
                    let key = format!("{tier}-{az_suffix}");
                    association_names.insert(vs(&key), vs(assoc_id));
                }
            }
        }
    }
    if !route_table_names.is_empty() || !association_names.is_empty() {
        let route_tables = ensure_mapping(spec, "routeTables");
        if !route_table_names.is_empty() {
            route_tables.insert(vs("externalNames"), Value::Mapping(route_table_names));
            notes.push("routeTables.externalNames <- live AWS tags".to_string());
        }
        if !association_names.is_empty() {
            route_tables.insert(vs("associationExternalNames"), Value::Mapping(association_names));
            notes.push("routeTables.associationExternalNames <- live AWS route-table associations".to_string());
        }
    }

    let nat_json = aws_json(&[
        "ec2",
        "describe-nat-gateways",
        "--region",
        region,
        "--filter",
        &format!("Name=tag:{NETWORK_TAG_KEY},Values={selector_name}"),
        "Name=state,Values=available",
    ])?;
    let nat_items = nat_json
        .get("NatGateways")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();
    let mut nat_names = Mapping::new();
    let mut eip_names = Mapping::new();
    for nat in nat_items {
        let Some(nat_id) = nat.get("NatGatewayId").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some(subnet_id) = nat.get("SubnetId").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some((_, az_suffix)) = subnet_lookup.get(subnet_id) else {
            continue;
        };
        let key = az_suffix.to_string();
        nat_names.insert(vs(&key), vs(nat_id));
        if let Some(allocation_id) = nat
            .get("NatGatewayAddresses")
            .and_then(JsonValue::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("AllocationId"))
            .and_then(JsonValue::as_str)
        {
            eip_names.insert(vs(&key), vs(allocation_id));
        }
    }
    if !nat_names.is_empty() || !eip_names.is_empty() {
        let nat = ensure_mapping(spec, "nat");
        if !nat_names.is_empty() {
            nat.insert(vs("externalNames"), Value::Mapping(nat_names));
            notes.push("nat.externalNames <- live AWS NAT gateways".to_string());
        }
        if !eip_names.is_empty() {
            nat.insert(vs("eipExternalNames"), Value::Mapping(eip_names));
            notes.push("nat.eipExternalNames <- live AWS EIP allocations".to_string());
        }
    }

    Ok(notes)
}

fn aws_json(args: &[&str]) -> Result<JsonValue, Box<dyn Error>> {
    let mut command_args = args.to_vec();
    command_args.push("--output");
    command_args.push("json");
    let output = run_cmd_output("aws", &command_args)?;
    Ok(serde_json::from_str(&output)?)
}

fn aws_iam_role_exists(role_name: &str) -> Result<bool, Box<dyn Error>> {
    let result = run_cmd_output("aws", &["iam", "get-role", "--role-name", role_name, "--output", "json"]);
    match result {
        Ok(_) => Ok(true),
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("NoSuchEntity") || msg.contains("cannot be found") {
                Ok(false)
            } else {
                Err(err)
            }
        }
    }
}

fn account_id_from_arn(arn: &str) -> Option<String> {
    let mut parts = arn.split(':');
    let _ = parts.next()?;
    let _ = parts.next()?;
    let _ = parts.next()?;
    let _ = parts.next()?;
    parts.next().map(ToString::to_string)
}

fn extract_custom_autoeks_tags(tags: &serde_json::Map<String, JsonValue>) -> Option<serde_json::Map<String, JsonValue>> {
    let mut filtered = serde_json::Map::new();

    for (key, value) in tags {
        if matches!(
            key.as_str(),
            "Name"
                | "crossplane-kind"
                | "crossplane-name"
                | "crossplane-providerconfig"
                | "hops.ops.com.ai/autoekscluster"
                | "hops.ops.com.ai/managed"
        ) {
            continue;
        }

        filtered.insert(key.clone(), value.clone());
    }

    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

fn json_object_to_yaml_mapping(
    object: &serde_json::Map<String, JsonValue>,
) -> Result<Value, Box<dyn Error>> {
    Ok(serde_yaml::to_value(JsonValue::Object(object.clone()))?)
}

fn validate_manage_manifest(spec: &ReclaimSpec, manifest: &Value) -> Result<(), Box<dyn Error>> {
    match spec.kind.as_str() {
        "AutoEKSCluster" => validate_autoekscluster_manage_manifest(manifest),
        _ => Ok(()),
    }
}

fn validate_autoekscluster_manage_manifest(manifest: &Value) -> Result<(), Box<dyn Error>> {
    let required = [
        "spec.clusterName",
        "spec.region",
        "spec.accountId",
        "spec.version",
    ];

    let missing = required
        .into_iter()
        .filter(|path| !has_path(manifest, path))
        .collect::<Vec<_>>();

    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "xr manage could not derive the final AutoEKSCluster spec from observed state; missing {}",
            missing.join(", ")
        )
        .into())
    }
}

fn role_name_from_arn(arn: &str) -> Option<String> {
    arn.rsplit('/').next().map(ToString::to_string)
}

fn kms_key_id_from_arn(arn: &str) -> Option<String> {
    arn.rsplit('/').next().map(ToString::to_string)
}

fn find_oidc_provider_arn(issuer_url: &str) -> Result<Option<String>, Box<dyn Error>> {
    let providers = aws_json(&["iam", "list-open-id-connect-providers"])?
        .get("OpenIDConnectProviderList")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();

    let normalized_issuer = issuer_url.trim_start_matches("https://");
    for provider in providers {
        let Some(arn) = provider.get("Arn").and_then(JsonValue::as_str) else {
            continue;
        };
        let details = aws_json(&["iam", "get-open-id-connect-provider", "--open-id-connect-provider-arn", arn])?;
        if details
            .get("Url")
            .and_then(JsonValue::as_str)
            .map(|url| url == normalized_issuer)
            .unwrap_or(false)
        {
            return Ok(Some(arn.to_string()));
        }
    }

    Ok(None)
}

fn has_tag(value: &JsonValue, key: &str, expected: &str) -> bool {
    tag_value(value, key).map(|actual| actual == expected).unwrap_or(false)
}

fn tag_value<'a>(value: &'a JsonValue, key: &str) -> Option<&'a str> {
    value.get("Tags")
        .and_then(JsonValue::as_array)
        .and_then(|tags| {
            tags.iter().find(|tag| {
                tag.get("Key").and_then(JsonValue::as_str) == Some(key)
            })
        })
        .and_then(|tag| tag.get("Value"))
        .and_then(JsonValue::as_str)
}

fn log_report(report: &ReclaimReport, live_aws: bool) {
    log::debug!("XR: {} {}", report.spec.api_version, report.spec.kind);
    log::debug!("project slug: {}", report.spec.project_slug);
    log::debug!(
        "base source: {}",
        match report.source {
            ManifestSource::Cluster => "cluster XR",
            ManifestSource::Generated => "generated reclaim scaffold",
        }
    );

    if let Some(resolver) = &report.spec.live_resolver {
        log::debug!("live resolver: {resolver}");
    } else {
        log::debug!("live resolver: none");
    }

    if !report.cluster_notes.is_empty() {
        log::debug!("cluster discovery:");
        for note in &report.cluster_notes {
            log::debug!("- {note}");
        }
    }

    if report.spec.composed_resources.is_empty() {
        log::debug!("composed resource kinds: none discovered");
    } else {
        log::debug!("composed resource kinds:");
        for resource in &report.spec.composed_resources {
            log::debug!("- {} {}", resource.api_version, resource.kind);
        }
    }

    if live_aws {
        if report.live_notes.is_empty() {
            log::debug!("live AWS discovery: no fields populated");
        } else {
            log::debug!("live AWS discovery:");
            for note in &report.live_notes {
                log::debug!("- {note}");
            }
        }
    }
}

fn ensure_mapping<'a>(map: &'a mut Mapping, key: &str) -> &'a mut Mapping {
    let entry = map
        .entry(vs(key))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    if !entry.is_mapping() {
        *entry = Value::Mapping(Mapping::new());
    }
    entry.as_mapping_mut().expect("mapping inserted above")
}

fn normalize_identity(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn vs(value: &str) -> Value {
    Value::String(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tag_lookup_reads_aws_shape() {
        let json: JsonValue = serde_json::from_str(
            r#"{"Tags":[{"Key":"hops.ops.com.ai/network","Value":"demo"}]}"#,
        )
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
}
