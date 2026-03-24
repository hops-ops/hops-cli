use crate::commands::local::kubectl_apply_stdin;
use crate::commands::xr::helpers::discovery::{
    build_managed_resource_adoption_patches, load_existing_cluster_manifest,
    render_managed_resource_patches,
};
use crate::commands::xr::helpers::manifest::{load_specs, match_spec};
use crate::commands::xr::helpers::types::AdoptArgs;
use std::error::Error;
use std::fs;

pub(crate) fn run(args: &AdoptArgs) -> Result<(), Box<dyn Error>> {
    let specs = load_specs()?;
    let spec = match_spec(&specs, &args.kind)?;
    let _ = load_existing_cluster_manifest(&spec, &args.name, &args.namespace)?;
    let patches = build_managed_resource_adoption_patches(&spec, &args.name)?;

    if patches.is_empty() {
        log::info!("no managed resources require adoption patches");
        return Ok(());
    }

    let patch_yaml = render_managed_resource_patches(&patches)?;

    if let Some(output) = &args.output {
        fs::write(output, &patch_yaml)?;
        log::info!("managed-resource adoption patches written to {output}");
    }

    if args.apply {
        kubectl_apply_stdin(&patch_yaml)?;
        log::info!(
            "applied {} managed-resource adoption patches to the cluster",
            patches.len()
        );
    } else if args.output.is_none() {
        print!("{patch_yaml}");
    }

    if !args.apply {
        log::debug!(
            "rendered {} managed-resource adoption patches; pipe the output to kubectl apply if you want to apply them",
            patches.len()
        );
    }

    Ok(())
}
