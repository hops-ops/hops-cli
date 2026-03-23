use crate::commands::local::kubectl_apply_stdin;
use crate::commands::xr::helpers::discovery::{
    load_existing_cluster_manifest, render_managed_resource_patches,
};
use crate::commands::xr::helpers::manifest::{load_specs, match_spec, vs};
use crate::commands::xr::helpers::types::{ManagedResourcePatch, OrphanArgs};
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

    let patches = vec![ManagedResourcePatch {
        api_version: spec.api_version.clone(),
        kind: spec.kind.clone(),
        namespace: args.namespace.clone(),
        name: args.name.clone(),
        external_name: None,
        management_policies: Some(management_policies),
    }];

    let patch_yaml = render_managed_resource_patches(&patches)?;

    if let Some(output) = &args.output {
        fs::write(output, &patch_yaml)?;
        log::info!("XR orphaning patch written to {output}");
    }

    if args.apply {
        kubectl_apply_stdin(&patch_yaml)?;
        log::info!("applied XR orphaning patch to the cluster");
    } else if args.output.is_none() {
        print!("{patch_yaml}");
    }

    if !args.apply {
        log::debug!(
            "rendered XR orphaning patch; pipe the output to kubectl apply if you want to apply it"
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
