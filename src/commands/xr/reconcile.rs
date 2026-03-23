use crate::commands::local::run_cmd_output;
use crate::commands::xr::helpers::discovery::load_existing_cluster_manifest;
use crate::commands::xr::helpers::manifest::{
    emit_report, ensure_mapping, load_specs, match_spec, vs,
};
use crate::commands::xr::helpers::types::{ReclaimSpec, ReconcileArgs};
use serde_json::Value as JsonValue;
use serde_yaml::Value;
use std::error::Error;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn run(args: &ReconcileArgs) -> Result<(), Box<dyn Error>> {
    let specs = load_specs()?;
    let spec = match_spec(&specs, &args.kind)?;
    let (mut manifest, source, cluster_notes) =
        load_existing_cluster_manifest(&spec, &args.name, &args.namespace)?;

    let live_notes = match spec.kind.as_str() {
        "AutoEKSCluster" => {
            reconcile_autoekscluster(&spec, &mut manifest, &args.name, &args.aws_region)?
        }
        _ => Vec::new(),
    };

    emit_report(
        &spec,
        &manifest,
        &live_notes,
        &cluster_notes,
        source,
        args.output.as_deref(),
        args.apply,
        "reconciled XR manifest",
    )
}

fn reconcile_autoekscluster(
    spec: &ReclaimSpec,
    manifest: &mut Value,
    cluster_name: &str,
    aws_region: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    let wants_nodeclass = has_composed_resource(spec, "eks.amazonaws.com/v1", "NodeClass");
    let wants_nodepool = has_composed_resource(spec, "karpenter.sh/v1", "NodePool");
    if !wants_nodeclass && !wants_nodepool {
        return Ok(vec![
            "no discoverable node resources in render function".to_string()
        ]);
    }

    let kubeconfig = generate_eks_kubeconfig(cluster_name, aws_region)?;
    let result = reconcile_autoekscluster_from_cluster(
        manifest,
        cluster_name,
        &kubeconfig,
        wants_nodeclass,
        wants_nodepool,
    );
    let _ = fs::remove_file(&kubeconfig);
    result
}

fn reconcile_autoekscluster_from_cluster(
    manifest: &mut Value,
    xr_name: &str,
    kubeconfig: &str,
    wants_nodeclass: bool,
    wants_nodepool: bool,
) -> Result<Vec<String>, Box<dyn Error>> {
    let nodeclasses = kubectl_json(
        kubeconfig,
        &["get", "nodeclasses.eks.amazonaws.com", "-o", "json"],
    )?;
    let nodeclass = if wants_nodeclass {
        select_nodeclass(&nodeclasses, "AutoEKSCluster", xr_name)?
    } else {
        None
    };

    let nodepools = kubectl_json(kubeconfig, &["get", "nodepools.karpenter.sh", "-o", "json"])?;
    let nodepool = if wants_nodepool {
        select_nodepool(
            &nodepools,
            nodeclass.as_ref().and_then(|item| json_name(item)),
        )?
    } else {
        None
    };

    let root = manifest
        .as_mapping_mut()
        .ok_or("manifest root must be a mapping")?;
    let spec_map = ensure_mapping(root, "spec");
    let mut notes = Vec::new();

    if let Some(nodeclass) = nodeclass {
        let node_config = ensure_mapping(spec_map, "nodeConfig");
        node_config.insert(vs("enabled"), Value::Bool(true));
        let nodeclass_cfg = ensure_mapping(node_config, "nodeClass");

        if let Some(name) = json_name(&nodeclass) {
            nodeclass_cfg.insert(vs("name"), vs(name));
            notes.push(format!(
                "nodeConfig.nodeClass.name <- live NodeClass ({name})"
            ));
        }

        if let Some(ephemeral_storage) = nodeclass
            .get("spec")
            .and_then(|spec| spec.get("ephemeralStorage"))
            .and_then(JsonValue::as_object)
        {
            let storage_cfg = ensure_mapping(nodeclass_cfg, "ephemeralStorage");
            if let Some(size) = ephemeral_storage.get("size").and_then(JsonValue::as_str) {
                storage_cfg.insert(vs("size"), vs(size));
            }
            if let Some(iops) = ephemeral_storage.get("iops").and_then(JsonValue::as_i64) {
                storage_cfg.insert(vs("iops"), Value::Number(iops.into()));
            }
            if let Some(throughput) = ephemeral_storage
                .get("throughput")
                .and_then(JsonValue::as_i64)
            {
                storage_cfg.insert(vs("throughput"), Value::Number(throughput.into()));
            }
            notes.push("nodeConfig.nodeClass.ephemeralStorage <- live NodeClass".to_string());
        }
    }

    if let Some(nodepool) = nodepool {
        let node_config = ensure_mapping(spec_map, "nodeConfig");
        node_config.insert(vs("enabled"), Value::Bool(true));
        let nodepool_cfg = ensure_mapping(node_config, "nodePool");
        nodepool_cfg.insert(vs("enabled"), Value::Bool(true));

        if let Some(name) = json_name(&nodepool) {
            nodepool_cfg.insert(vs("name"), vs(name));
            notes.push(format!(
                "nodeConfig.nodePool.name <- live NodePool ({name})"
            ));
        }

        if let Some(expire_after) = nodepool
            .get("spec")
            .and_then(|spec| spec.get("template"))
            .and_then(|template| template.get("spec"))
            .and_then(|spec| spec.get("expireAfter"))
            .and_then(JsonValue::as_str)
        {
            nodepool_cfg.insert(vs("expireAfter"), vs(expire_after));
        }

        if let Some(requirements) = nodepool
            .get("spec")
            .and_then(|spec| spec.get("template"))
            .and_then(|template| template.get("spec"))
            .and_then(|spec| spec.get("requirements"))
        {
            nodepool_cfg.insert(
                vs("requirements"),
                serde_yaml::to_value(requirements.clone())?,
            );
            notes.push("nodeConfig.nodePool.requirements <- live NodePool".to_string());
        }

        if let Some(disruption) = nodepool.get("spec").and_then(|spec| spec.get("disruption")) {
            nodepool_cfg.insert(vs("disruption"), serde_yaml::to_value(disruption.clone())?);
            notes.push("nodeConfig.nodePool.disruption <- live NodePool".to_string());
        }
    }

    Ok(notes)
}

fn has_composed_resource(spec: &ReclaimSpec, api_version: &str, kind: &str) -> bool {
    spec.composed_resources
        .iter()
        .any(|resource| resource.api_version == api_version && resource.kind == kind)
}

fn generate_eks_kubeconfig(cluster_name: &str, aws_region: &str) -> Result<String, Box<dyn Error>> {
    let kubeconfig = run_cmd_output(
        "aws",
        &[
            "eks",
            "update-kubeconfig",
            "--name",
            cluster_name,
            "--region",
            aws_region,
            "--dry-run",
        ],
    )?;

    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = std::env::temp_dir().join(format!(
        "hops-xr-reconcile-{cluster_name}-{nonce}.kubeconfig"
    ));
    fs::write(&path, kubeconfig)?;
    Ok(path.to_string_lossy().to_string())
}

fn kubectl_json(kubeconfig: &str, args: &[&str]) -> Result<JsonValue, Box<dyn Error>> {
    let mut full_args = vec!["--kubeconfig", kubeconfig];
    full_args.extend_from_slice(args);
    let output = run_cmd_output("kubectl", &full_args)?;
    Ok(serde_json::from_str(&output)?)
}

fn select_nodeclass(
    root: &JsonValue,
    xr_kind: &str,
    xr_name: &str,
) -> Result<Option<JsonValue>, Box<dyn Error>> {
    let items = root
        .get("items")
        .and_then(JsonValue::as_array)
        .ok_or("nodeclass list missing items")?;
    let identity_tag = format!("hops.ops.com.ai/{}", xr_kind.to_ascii_lowercase());
    let derived_role = format!("{xr_name}-node");

    let tag_matches = items
        .iter()
        .filter(|item| {
            live_identity_value(item, &identity_tag) == Some(xr_name)
                && live_identity_value(item, "hops.ops.com.ai/managed") == Some("true")
        })
        .cloned()
        .collect::<Vec<_>>();
    if let Some(selected) = select_single_nodeclass_candidate(&tag_matches, &derived_role)? {
        return Ok(Some(selected));
    }

    let unscoped_tag_matches = items
        .iter()
        .filter(|item| live_identity_value(item, &identity_tag) == Some(xr_name))
        .cloned()
        .collect::<Vec<_>>();
    if let Some(selected) = select_single_nodeclass_candidate(&unscoped_tag_matches, &derived_role)?
    {
        return Ok(Some(selected));
    }

    let role_matches = items
        .iter()
        .filter(|item| {
            item.get("spec")
                .and_then(|spec| spec.get("role"))
                .and_then(JsonValue::as_str)
                == Some(derived_role.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    if let Some(selected) = select_single_nodeclass_candidate(&role_matches, &derived_role)? {
        return Ok(Some(selected));
    }

    match items.len() {
        1 => Ok(Some(items[0].clone())),
        _ => Ok(None),
    }
}

fn select_nodepool(
    root: &JsonValue,
    nodeclass_name: Option<&str>,
) -> Result<Option<JsonValue>, Box<dyn Error>> {
    let items = root
        .get("items")
        .and_then(JsonValue::as_array)
        .ok_or("nodepool list missing items")?;

    let matching = match nodeclass_name {
        Some(name) => items
            .iter()
            .filter(|item| {
                item.get("spec")
                    .and_then(|spec| spec.get("template"))
                    .and_then(|template| template.get("spec"))
                    .and_then(|spec| spec.get("nodeClassRef"))
                    .and_then(|reference| reference.get("name"))
                    .and_then(JsonValue::as_str)
                    == Some(name)
            })
            .cloned()
            .collect::<Vec<_>>(),
        None => Vec::new(),
    };

    match matching.len() {
        1 => Ok(Some(matching[0].clone())),
        0 => {
            if items.len() == 1 {
                Ok(Some(items[0].clone()))
            } else {
                Ok(None)
            }
        }
        _ => Err("multiple NodePool resources matched the selected NodeClass".into()),
    }
}

fn json_name<'a>(value: &'a JsonValue) -> Option<&'a str> {
    value
        .get("metadata")
        .and_then(|metadata| metadata.get("name"))
        .and_then(JsonValue::as_str)
}

fn live_identity_value<'a>(item: &'a JsonValue, key: &str) -> Option<&'a str> {
    item.get("spec")
        .and_then(|spec| spec.get("tags"))
        .and_then(|tags| tags.get(key))
        .and_then(JsonValue::as_str)
        .or_else(|| {
            item.get("metadata")
                .and_then(|metadata| metadata.get("labels"))
                .and_then(|labels| labels.get(key))
                .and_then(JsonValue::as_str)
        })
}

fn select_single_nodeclass_candidate(
    candidates: &[JsonValue],
    derived_role: &str,
) -> Result<Option<JsonValue>, Box<dyn Error>> {
    match candidates.len() {
        1 => Ok(Some(candidates[0].clone())),
        0 => Ok(None),
        _ => {
            let role_matches = candidates
                .iter()
                .filter(|item| {
                    item.get("spec")
                        .and_then(|spec| spec.get("role"))
                        .and_then(JsonValue::as_str)
                        == Some(derived_role)
                })
                .cloned()
                .collect::<Vec<_>>();
            match role_matches.len() {
                1 => Ok(Some(role_matches[0].clone())),
                0 => Err(format!(
                    "multiple live NodeClass resources matched; candidates: {}",
                    candidate_names(candidates)
                )
                .into()),
                _ => Err(format!(
                    "multiple live NodeClass resources matched role '{derived_role}'; candidates: {}",
                    candidate_names(&role_matches)
                )
                .into()),
            }
        }
    }
}

fn candidate_names(items: &[JsonValue]) -> String {
    items
        .iter()
        .filter_map(json_name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::{select_nodeclass, select_nodepool};
    use serde_json::json;

    #[test]
    fn select_nodeclass_prefers_live_identity_tag() {
        let root = json!({
            "items": [
                {
                    "metadata":{"name":"other"},
                    "spec":{
                        "role":"pat-local-node",
                        "tags":{
                            "hops.ops.com.ai/managed":"true",
                            "hops.ops.com.ai/autoekscluster":"other"
                        }
                    }
                },
                {
                    "metadata":{"name":"target"},
                    "spec":{
                        "role":"pat-local-node",
                        "tags":{
                            "hops.ops.com.ai/managed":"true",
                            "hops.ops.com.ai/autoekscluster":"pat-local"
                        }
                    }
                }
            ]
        });

        let selected = select_nodeclass(&root, "AutoEKSCluster", "pat-local").expect("select");
        assert_eq!(
            selected
                .as_ref()
                .and_then(|item| item.get("metadata"))
                .and_then(|metadata| metadata.get("name"))
                .and_then(|value| value.as_str()),
            Some("target")
        );
    }

    #[test]
    fn select_nodeclass_falls_back_to_role_when_tags_are_missing() {
        let root = json!({
            "items": [
                {"metadata":{"name":"other"},"spec":{"role":"other-node"}},
                {"metadata":{"name":"target"},"spec":{"role":"pat-local-node"}}
            ]
        });

        let selected = select_nodeclass(&root, "AutoEKSCluster", "pat-local").expect("select");
        assert_eq!(
            selected
                .as_ref()
                .and_then(|item| item.get("metadata"))
                .and_then(|metadata| metadata.get("name"))
                .and_then(|value| value.as_str()),
            Some("target")
        );
    }

    #[test]
    fn select_nodepool_prefers_matching_nodeclass_ref() {
        let root = json!({
            "items": [
                {"metadata":{"name":"other"},"spec":{"template":{"spec":{"nodeClassRef":{"name":"other"}}}}},
                {"metadata":{"name":"target"},"spec":{"template":{"spec":{"nodeClassRef":{"name":"nodeclass-a"}}}}}
            ]
        });

        let selected = select_nodepool(&root, Some("nodeclass-a")).expect("select");
        assert_eq!(
            selected
                .as_ref()
                .and_then(|item| item.get("metadata"))
                .and_then(|metadata| metadata.get("name"))
                .and_then(|value| value.as_str()),
            Some("target")
        );
    }
}
