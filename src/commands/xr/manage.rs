use crate::commands::local::kubectl_apply_stdin;
use crate::commands::xr::helpers::discovery::{
    load_existing_cluster_manifest, render_managed_resource_patches,
};
use crate::commands::xr::helpers::manifest::load_specs;
use crate::commands::xr::helpers::manifest::match_spec;
use crate::commands::xr::helpers::types::{ManageXrArgs, ManagedResourcePatch};
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

    let patch_yaml = render_managed_resource_patches(&[ManagedResourcePatch {
        api_version: spec.api_version.clone(),
        kind: spec.kind.clone(),
        namespace: args.namespace.clone(),
        name: args.name.clone(),
        external_name: None,
        management_policies: Some(vec!["*".to_string()]),
    }])?;

    if let Some(output) = &args.output {
        fs::write(output, &patch_yaml)?;
        log::info!("managed XR patch written to {output}");
    }

    if args.apply {
        kubectl_apply_stdin(&patch_yaml)?;
        log::info!("applied managed XR patch to the cluster");
    } else if args.output.is_none() {
        print!("{patch_yaml}");
    }

    if !args.apply {
        log::debug!(
            "rendered managed XR patch; pipe the output to kubectl apply if you want to apply it"
        );
    }

    Ok(())
}

fn xr_is_fully_managed(manifest: &Value) -> bool {
    manifest
        .as_mapping()
        .and_then(|root| root.get(Value::String("spec".to_string())))
        .and_then(Value::as_mapping)
        .and_then(|spec| spec.get(Value::String("managementPolicies".to_string())))
        .and_then(Value::as_sequence)
        .map(|policies| {
            policies
                .iter()
                .filter_map(Value::as_str)
                .any(|policy| policy == "*")
        })
        .unwrap_or(false)
}
