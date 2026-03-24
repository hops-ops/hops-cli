use crate::commands::local::kubectl_patch_merge;
use crate::commands::xr::helpers::discovery::load_existing_cluster_manifest;
use crate::commands::xr::helpers::manifest::{load_specs, match_spec, vs};
use crate::commands::xr::helpers::types::ManageXrArgs;
use serde_yaml::Value;
use std::error::Error;
use std::fs;

pub(crate) fn run(args: &ManageXrArgs) -> Result<(), Box<dyn Error>> {
    let specs = load_specs()?;
    let spec = match_spec(&specs, &args.kind)?;
    let (manifest, _, _) = load_existing_cluster_manifest(&spec, &args.name, &args.namespace)?;

    if xr_is_fully_managed(&manifest) {
        log::info!("XR already has full top-level managementPolicies");
        return Ok(());
    }

    let patch_json = serde_json::json!({
        "spec": {
            "managementPolicies": ["*"]
        }
    });
    let patch_json = serde_json::to_string_pretty(&patch_json)?;
    let resource = format!("{}.{}", spec.plural, spec.group);

    if let Some(output) = &args.output {
        fs::write(output, &patch_json)?;
        log::info!("managed XR merge patch written to {output}");
    }

    if args.apply {
        kubectl_patch_merge(&resource, &args.name, &args.namespace, &patch_json)?;
        log::info!("applied managed XR merge patch to the cluster");
    } else if args.output.is_none() {
        println!("{patch_json}");
    }

    if !args.apply {
        log::debug!("rendered managed XR merge patch; apply it with kubectl patch --type merge -p");
    }

    Ok(())
}

fn xr_is_fully_managed(manifest: &Value) -> bool {
    manifest
        .as_mapping()
        .and_then(|root| root.get(vs("spec")))
        .and_then(Value::as_mapping)
        .and_then(|spec| spec.get(vs("managementPolicies")))
        .and_then(Value::as_sequence)
        .map(|policies| {
            policies
                .iter()
                .filter_map(Value::as_str)
                .any(|policy| policy == "*")
        })
        .unwrap_or(false)
}
