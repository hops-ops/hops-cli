use crate::commands::local::kubectl_apply_stdin;
use crate::commands::xr::helpers::discovery::{
    build_managed_resource_adoption_patches, load_existing_cluster_manifest,
    render_managed_resource_patches,
};
use crate::commands::xr::helpers::manifest::{load_specs, match_spec};
use crate::commands::xr::helpers::types::AdoptArgs;
use std::error::Error;
use std::fs;
use std::thread::sleep;
use std::time::Duration;

const RECURSIVE_ADOPTION_MAX_PASSES: usize = 20;
const RECURSIVE_ADOPTION_WAIT: Duration = Duration::from_secs(2);

pub(crate) fn run(args: &AdoptArgs) -> Result<(), Box<dyn Error>> {
    if args.recursive && !args.apply {
        return Err(
            "--recursive requires --apply so new managed resources can be discovered".into(),
        );
    }

    let specs = load_specs()?;
    let spec = match_spec(&specs, &args.kind)?;
    let _ = load_existing_cluster_manifest(&spec, &args.name, &args.namespace)?;

    let max_passes = if args.recursive {
        RECURSIVE_ADOPTION_MAX_PASSES
    } else {
        1
    };
    let mut aggregated_yaml = Vec::new();
    let mut total_patches = 0usize;

    for pass in 1..=max_passes {
        let patches = build_managed_resource_adoption_patches(&spec, &args.name)?;

        if patches.is_empty() {
            if total_patches == 0 {
                log::info!("no managed resources require adoption patches");
            } else if args.recursive {
                log::info!(
                    "recursive adoption completed after {} pass(es); applied {} managed-resource adoption patches total",
                    pass - 1,
                    total_patches
                );
            }
            break;
        }

        let patch_yaml = render_managed_resource_patches(&patches)?;
        aggregated_yaml.push(patch_yaml.clone());
        total_patches += patches.len();

        if args.recursive {
            log::info!(
                "recursive adoption pass {} discovered {} managed-resource patch(es)",
                pass,
                patches.len()
            );
        }

        if args.apply {
            kubectl_apply_stdin(&patch_yaml)?;
            if args.recursive {
                log::info!(
                    "applied {} managed-resource adoption patch(es) in pass {}",
                    patches.len(),
                    pass
                );
            } else {
                log::info!(
                    "applied {} managed-resource adoption patches to the cluster",
                    patches.len()
                );
            }
        } else if args.output.is_none() {
            print!("{patch_yaml}");
        }

        if !args.recursive {
            break;
        }

        if pass == max_passes {
            return Err(format!(
                "recursive adoption reached the maximum of {} passes before converging",
                RECURSIVE_ADOPTION_MAX_PASSES
            )
            .into());
        }

        sleep(RECURSIVE_ADOPTION_WAIT);
    }

    if let Some(output) = &args.output {
        let combined_yaml = aggregated_yaml.join("---\n");
        fs::write(output, &combined_yaml)?;
        log::info!("managed-resource adoption patches written to {output}");
    }

    if !args.apply && total_patches > 0 {
        log::debug!(
            "rendered {} managed-resource adoption patches; pipe the output to kubectl apply if you want to apply them",
            total_patches
        );
    }

    Ok(())
}
