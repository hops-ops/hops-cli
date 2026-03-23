use crate::commands::local::kubectl_patch_merge;
use crate::commands::xr::helpers::discovery::load_existing_cluster_manifest;
use crate::commands::xr::helpers::manifest::{load_specs, match_spec, vs};
use crate::commands::xr::helpers::types::OrphanArgs;
use serde_yaml::Value;
use std::collections::BTreeSet;
use std::error::Error;
use std::fs;

pub(crate) fn run(args: &OrphanArgs) -> Result<(), Box<dyn Error>> {
    let specs = load_specs()?;
    let spec = match_spec(&specs, &args.kind)?;
    let (manifest, _, _) = load_existing_cluster_manifest(&spec, &args.name, &args.namespace)?;
    let Some(management_policies) = orphan_xr_management_policies(&manifest) else {
        log::info!("XR already excludes Delete from top-level managementPolicies");
        return Ok(());
    };

    let patch_json = serde_json::json!({
        "spec": {
            "managementPolicies": management_policies
        }
    });
    let patch_json = serde_json::to_string_pretty(&patch_json)?;
    let resource = format!("{}.{}", spec.plural, spec.group);

    if let Some(output) = &args.output {
        fs::write(output, &patch_json)?;
        log::info!("XR orphaning merge patch written to {output}");
    }

    if args.apply {
        kubectl_patch_merge(&resource, &args.name, &args.namespace, &patch_json)?;
        log::info!("applied XR orphaning merge patch to the cluster");
    } else if args.output.is_none() {
        println!("{patch_json}");
    }

    if !args.apply {
        log::debug!(
            "rendered XR orphaning merge patch; apply it with kubectl patch --type merge -p"
        );
    }

    Ok(())
}

pub(crate) fn orphan_xr_management_policies(manifest: &Value) -> Option<Vec<String>> {
    let current = manifest
        .as_mapping()
        .and_then(|root| root.get(vs("spec")))
        .and_then(Value::as_mapping)
        .and_then(|spec| spec.get(vs("managementPolicies")))
        .and_then(Value::as_sequence);

    let mut policies = match current {
        Some(values) => {
            let items = values
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>();

            if items.iter().any(|value| value == "*") {
                vec![
                    "Create".to_string(),
                    "Observe".to_string(),
                    "Update".to_string(),
                    "LateInitialize".to_string(),
                ]
            } else {
                items
                    .into_iter()
                    .filter(|value| value != "Delete")
                    .collect::<Vec<_>>()
            }
        }
        None => vec![
            "Create".to_string(),
            "Observe".to_string(),
            "Update".to_string(),
            "LateInitialize".to_string(),
        ],
    };

    let mut deduped = BTreeSet::new();
    policies.retain(|policy| deduped.insert(policy.clone()));

    let current_normalized = current.map(|values| {
        let mut items = values
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        items.sort_unstable();
        items.dedup();
        items
    });

    let mut normalized_policies = policies.clone();
    normalized_policies.sort_unstable();

    if current_normalized.as_ref() == Some(&normalized_policies) {
        None
    } else {
        Some(policies)
    }
}
