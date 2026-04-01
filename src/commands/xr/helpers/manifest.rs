use crate::commands::local::kubectl_apply_stdin;
use crate::commands::local::run_cmd_output;
use crate::commands::xr::helpers::runtime_discovery::enrich_spec_with_runtime_discovery;
use crate::commands::xr::helpers::types::{ManifestSource, ReclaimReport, ReclaimSpec};
use serde_json::Value as JsonValue;
use serde_yaml::{Mapping, Value};
use std::error::Error;
use std::fs;

pub(crate) fn load_specs() -> Result<Vec<ReclaimSpec>, Box<dyn Error>> {
    load_cluster_specs()
}

pub(crate) fn match_spec<'a>(
    specs: &'a [ReclaimSpec],
    needle: &str,
) -> Result<ReclaimSpec, Box<dyn Error>> {
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
        1 => Ok(enrich_spec_with_runtime_discovery(matches[0])),
        0 => Err(format!("no XR found matching '{needle}'").into()),
        _ => Err(format!("multiple XRs match '{needle}'").into()),
    }
}

pub(crate) fn render_manifest(
    spec: &ReclaimSpec,
    object_name: &str,
    namespace: &str,
) -> Result<Value, Box<dyn Error>> {
    let mut root = Mapping::new();
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

pub(crate) fn sanitize_manifest_defaults(
    spec: &ReclaimSpec,
    manifest: &mut Value,
    source: ManifestSource,
) {
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

pub(crate) fn set_observe_only_management(manifest: &mut Value) {
    let Some(root) = manifest.as_mapping_mut() else {
        return;
    };
    let spec = ensure_mapping(root, "spec");
    spec.insert(
        vs("managementPolicies"),
        Value::Sequence(vec![vs("Observe"), vs("LateInitialize")]),
    );
}

pub(crate) fn strip_runtime_k8s_fields(value: &mut Value) {
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

pub(crate) fn sanitize_authored_manifest(value: &mut Value) {
    strip_runtime_k8s_fields(value);

    let Some(root) = value.as_mapping_mut() else {
        return;
    };

    let metadata = ensure_mapping(root, "metadata");
    metadata.remove(vs("finalizers"));
    metadata.remove(vs("ownerReferences"));

    if let Some(annotations) = metadata
        .get_mut(vs("annotations"))
        .and_then(Value::as_mapping_mut)
    {
        annotations.remove(vs("kubectl.kubernetes.io/last-applied-configuration"));
        if annotations.is_empty() {
            metadata.remove(vs("annotations"));
        }
    }

    if let Some(labels) = metadata
        .get_mut(vs("labels"))
        .and_then(Value::as_mapping_mut)
    {
        labels.remove(vs("crossplane.io/composite"));
        if labels.is_empty() {
            metadata.remove(vs("labels"));
        }
    }

    let spec = ensure_mapping(root, "spec");
    spec.remove(vs("crossplane"));
}

pub(crate) fn prune_manifest_to_crd_spec(
    spec: &ReclaimSpec,
    manifest: &mut Value,
) -> Result<(), Box<dyn Error>> {
    let crd_name = format!("{}.{}", spec.plural, spec.group);
    let crd_json = run_cmd_output("kubectl", &["get", "crd", &crd_name, "-o", "json"])?;
    let root: JsonValue = serde_json::from_str(&crd_json)?;
    let version_name = spec.api_version.split('/').nth(1).unwrap_or_default();

    let schema = root
        .get("spec")
        .and_then(|spec| spec.get("versions"))
        .and_then(JsonValue::as_array)
        .and_then(|versions| {
            versions
                .iter()
                .find(|version| {
                    version.get("name").and_then(JsonValue::as_str) == Some(version_name)
                })
                .or_else(|| versions.first())
        })
        .and_then(|version| version.get("schema"))
        .and_then(|schema| schema.get("openAPIV3Schema"))
        .and_then(|schema| schema.get("properties"))
        .and_then(|properties| properties.get("spec"))
        .ok_or_else(|| format!("CRD schema for {} is missing spec properties", crd_name))?;

    let Some(root) = manifest.as_mapping_mut() else {
        return Ok(());
    };
    if let Some(spec_value) = root.get_mut(vs("spec")) {
        prune_value_to_openapi_schema(spec_value, schema);
    }

    Ok(())
}

pub(crate) fn prune_value_to_openapi_schema(value: &mut Value, schema: &JsonValue) {
    if schema
        .get("x-kubernetes-preserve-unknown-fields")
        .and_then(JsonValue::as_bool)
        == Some(true)
    {
        return;
    }

    match value {
        Value::Mapping(map) => {
            let properties = schema.get("properties").and_then(JsonValue::as_object);
            let additional = schema.get("additionalProperties");
            let keys = map.keys().cloned().collect::<Vec<_>>();

            for key in keys {
                let Some(key_name) = key.as_str() else {
                    map.remove(&key);
                    continue;
                };

                if let Some(child_schema) = properties.and_then(|props| props.get(key_name)) {
                    if let Some(child) = map.get_mut(&key) {
                        prune_value_to_openapi_schema(child, child_schema);
                    }
                    continue;
                }

                match additional {
                    Some(JsonValue::Bool(true)) => {}
                    Some(JsonValue::Object(_)) => {
                        if let Some(child) = map.get_mut(&key) {
                            prune_value_to_openapi_schema(
                                child,
                                additional.expect("matched above"),
                            );
                        }
                    }
                    _ => {
                        map.remove(&key);
                    }
                }
            }
        }
        Value::Sequence(items) => {
            if let Some(item_schema) = schema.get("items") {
                for item in items {
                    prune_value_to_openapi_schema(item, item_schema);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn strip_external_name_fields(value: &mut Value) {
    match value {
        Value::Mapping(map) => {
            map.remove(vs("externalName"));
            map.remove(vs("externalNames"));
            map.remove(vs("associationExternalNames"));
            map.remove(vs("eipExternalNames"));

            let keys = map.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                if let Some(child) = map.get_mut(&key) {
                    strip_external_name_fields(child);
                }
            }
        }
        Value::Sequence(items) => {
            for item in items {
                strip_external_name_fields(item);
            }
        }
        _ => {}
    }
}

pub(crate) fn log_report(report: &ReclaimReport, live_aws: bool) {
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

pub(crate) fn emit_report(
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

pub(crate) fn ensure_mapping<'a>(map: &'a mut Mapping, key: &str) -> &'a mut Mapping {
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

pub(crate) fn vs(value: &str) -> Value {
    Value::String(value.to_string())
}

fn load_cluster_specs() -> Result<Vec<ReclaimSpec>, Box<dyn Error>> {
    let crd_json = run_cmd_output("kubectl", &["get", "crd", "-o", "json"])?;
    let root: JsonValue = serde_json::from_str(&crd_json)?;
    let items = root
        .get("items")
        .and_then(JsonValue::as_array)
        .ok_or("kubectl CRD output missing items")?;

    let mut specs = Vec::new();
    for item in items {
        let spec = item.get("spec").and_then(JsonValue::as_object);
        let names = spec
            .and_then(|spec| spec.get("names"))
            .and_then(JsonValue::as_object);

        let Some(group) = spec
            .and_then(|spec| spec.get("group"))
            .and_then(JsonValue::as_str)
        else {
            continue;
        };
        let Some(kind) = names
            .and_then(|names| names.get("kind"))
            .and_then(JsonValue::as_str)
        else {
            continue;
        };
        let Some(plural) = names
            .and_then(|names| names.get("plural"))
            .and_then(JsonValue::as_str)
        else {
            continue;
        };
        let Some(version) = spec
            .and_then(|spec| spec.get("versions"))
            .and_then(JsonValue::as_array)
            .and_then(|versions| {
                versions
                    .iter()
                    .find(|version| {
                        version
                            .get("served")
                            .and_then(JsonValue::as_bool)
                            .unwrap_or(false)
                    })
                    .or_else(|| versions.first())
            })
            .and_then(|version| version.get("name"))
            .and_then(JsonValue::as_str)
        else {
            continue;
        };

        let project_slug = item
            .get("metadata")
            .and_then(|metadata| metadata.get("labels"))
            .and_then(|labels| labels.get("hops.ops.com.ai/project"))
            .and_then(JsonValue::as_str)
            .unwrap_or(plural)
            .to_string();

        specs.push(ReclaimSpec {
            api_version: format!("{group}/{version}"),
            kind: kind.to_string(),
            plural: plural.to_string(),
            group: group.to_string(),
            project_slug,
            composed_resources: Vec::new(),
            live_resolver: live_resolver_for(group, kind),
        });
    }

    Ok(specs)
}

fn live_resolver_for(group: &str, kind: &str) -> Option<String> {
    match (group, kind) {
        ("aws.hops.ops.com.ai", "Network") => Some("aws-network-by-tag".to_string()),
        ("aws.hops.ops.com.ai", "AutoEKSCluster") => Some("aws-autoekscluster".to_string()),
        _ => None,
    }
}
