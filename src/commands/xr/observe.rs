use crate::commands::xr::helpers::discovery::apply_live_aws;
use crate::commands::xr::helpers::manifest::{
    emit_report, load_specs, match_spec, render_manifest, sanitize_manifest_defaults,
    set_observe_only_management, strip_external_name_fields,
};
use crate::commands::xr::helpers::types::{ManifestSource, ObserveArgs};
use std::error::Error;

pub(crate) fn run(args: &ObserveArgs) -> Result<(), Box<dyn Error>> {
    let specs = load_specs()?;
    let spec = match_spec(&specs, &args.kind)?;

    let mut manifest = render_manifest(&spec, &args.name, &args.namespace)?;
    sanitize_manifest_defaults(&spec, &mut manifest, ManifestSource::Generated);
    let live_notes = apply_live_aws(&spec, &mut manifest, &args.name, &args.aws_region)?;
    strip_external_name_fields(&mut manifest);
    set_observe_only_management(&mut manifest);

    emit_report(
        &spec,
        &manifest,
        &live_notes,
        &["generated bootstrap observe-only manifest".to_string()],
        ManifestSource::Generated,
        args.output.as_deref(),
        args.apply,
        "observe manifest",
    )
}
