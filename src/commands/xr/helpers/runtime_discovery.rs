use crate::commands::local::run_cmd_output;
use crate::commands::xr::helpers::types::{ReclaimSpec, ResourceRef};
use serde_json::Value as JsonValue;
use std::collections::BTreeSet;
use std::error::Error;
use std::io::Cursor;
use std::process::Command;
use tar::Archive;

pub(crate) fn enrich_spec_with_runtime_discovery(spec: &ReclaimSpec) -> ReclaimSpec {
    let mut enriched = spec.clone();
    match discover_composed_resources(spec) {
        Ok(resources) => enriched.composed_resources = resources,
        Err(err) => {
            log::debug!(
                "failed to discover composed resources for {} {}: {}",
                spec.api_version,
                spec.kind,
                err
            );
        }
    }
    enriched
}

fn discover_composed_resources(spec: &ReclaimSpec) -> Result<Vec<ResourceRef>, Box<dyn Error>> {
    let render_function = resolve_render_function_name(spec)?;
    let image = resolve_function_image(&render_function)?;
    let source_dir = resolve_function_source_dir(&image)?.unwrap_or_else(|| "/src".to_string());
    let archive = export_function_filesystem(&image)?;
    extract_resource_refs_from_archive(&archive, &source_dir)
}

fn resolve_render_function_name(spec: &ReclaimSpec) -> Result<String, Box<dyn Error>> {
    let output = run_cmd_output("kubectl", &["get", "compositionrevision", "-o", "json"])?;
    let root: JsonValue = serde_json::from_str(&output)?;
    let items = root
        .get("items")
        .and_then(JsonValue::as_array)
        .ok_or("kubectl compositionrevision output missing items")?;

    let revision = items
        .iter()
        .filter(|item| {
            item.get("spec")
                .and_then(|spec_value| spec_value.get("compositeTypeRef"))
                .map(|composite| {
                    composite.get("apiVersion").and_then(JsonValue::as_str)
                        == Some(spec.api_version.as_str())
                        && composite.get("kind").and_then(JsonValue::as_str)
                            == Some(spec.kind.as_str())
                })
                .unwrap_or(false)
        })
        .max_by_key(|item| {
            item.get("spec")
                .and_then(|spec_value| spec_value.get("revision"))
                .and_then(JsonValue::as_i64)
                .unwrap_or_default()
        })
        .ok_or_else(|| {
            format!(
                "no CompositionRevision found for {} {}",
                spec.api_version, spec.kind
            )
        })?;

    let function_name = revision
        .get("spec")
        .and_then(|spec_value| spec_value.get("pipeline"))
        .and_then(JsonValue::as_array)
        .and_then(|pipeline| {
            pipeline
                .iter()
                .find(|step| step.get("step").and_then(JsonValue::as_str) == Some("render"))
                .or_else(|| pipeline.first())
        })
        .and_then(|step| step.get("functionRef"))
        .and_then(|function_ref| function_ref.get("name"))
        .and_then(JsonValue::as_str)
        .ok_or("CompositionRevision missing pipeline functionRef.name")?;

    Ok(function_name.to_string())
}

fn resolve_function_image(function_name: &str) -> Result<String, Box<dyn Error>> {
    let output = run_cmd_output(
        "kubectl",
        &[
            "get",
            "function.pkg.crossplane.io",
            function_name,
            "-o",
            "json",
        ],
    )?;
    let root: JsonValue = serde_json::from_str(&output)?;

    let current_revision = root
        .get("status")
        .and_then(|status| status.get("currentRevision"))
        .and_then(JsonValue::as_str)
        .ok_or("Function status.currentRevision missing")?;

    let revision_output = run_cmd_output(
        "kubectl",
        &[
            "get",
            "functionrevision.pkg.crossplane.io",
            current_revision,
            "-o",
            "json",
        ],
    )?;
    let revision: JsonValue = serde_json::from_str(&revision_output)?;
    let image = revision
        .get("spec")
        .and_then(|spec_value| spec_value.get("image"))
        .and_then(JsonValue::as_str)
        .ok_or("FunctionRevision spec.image missing")?;

    Ok(image.to_string())
}

fn resolve_function_source_dir(image: &str) -> Result<Option<String>, Box<dyn Error>> {
    let output = run_cmd_output("docker", &["image", "inspect", image])?;
    let root: JsonValue = serde_json::from_str(&output)?;
    let envs = root
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item.get("Config"))
        .and_then(|config| config.get("Env"))
        .and_then(JsonValue::as_array);

    Ok(envs.and_then(|values| {
        values
            .iter()
            .filter_map(JsonValue::as_str)
            .find_map(|value| {
                value
                    .strip_prefix("FUNCTION_GO_TEMPLATING_DEFAULT_SOURCE=")
                    .map(ToString::to_string)
            })
    }))
}

fn export_function_filesystem(image: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let create_output = Command::new("docker").args(["create", image]).output()?;
    if !create_output.status.success() {
        return Err(format!(
            "docker create exited with {}: {}",
            create_output.status,
            String::from_utf8_lossy(&create_output.stderr)
        )
        .into());
    }

    let container_id = String::from_utf8_lossy(&create_output.stdout)
        .trim()
        .to_string();
    let export_result = Command::new("docker")
        .args(["export", &container_id])
        .output();
    let _ = Command::new("docker").args(["rm", &container_id]).output();

    let export_output = export_result?;
    if !export_output.status.success() {
        return Err(format!(
            "docker export exited with {}: {}",
            export_output.status,
            String::from_utf8_lossy(&export_output.stderr)
        )
        .into());
    }

    Ok(export_output.stdout)
}

fn extract_resource_refs_from_archive(
    archive_bytes: &[u8],
    source_dir: &str,
) -> Result<Vec<ResourceRef>, Box<dyn Error>> {
    let source_prefix = source_dir
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string()
        + "/";
    let mut archive = Archive::new(Cursor::new(archive_bytes));
    let mut resources = BTreeSet::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        if !path.starts_with(&source_prefix) || !path.ends_with(".gotmpl") {
            continue;
        }

        let mut content = String::new();
        use std::io::Read;
        entry.read_to_string(&mut content)?;
        for resource in parse_resource_refs(&content) {
            resources.insert(resource);
        }
    }

    Ok(resources.into_iter().collect())
}

fn parse_resource_refs(content: &str) -> Vec<ResourceRef> {
    let mut refs = Vec::new();
    let mut api_version: Option<String> = None;
    let mut kind: Option<String> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if let Some(value) = line.strip_prefix("apiVersion:") {
            let value = value.trim();
            api_version = is_literal_yaml_scalar(value).then(|| value.to_string());
            continue;
        }

        if let Some(value) = line.strip_prefix("kind:") {
            let value = value.trim();
            kind = is_literal_yaml_scalar(value).then(|| value.to_string());
        }

        if let (Some(api), Some(kind_value)) = (&api_version, &kind) {
            refs.push(ResourceRef {
                api_version: api.clone(),
                kind: kind_value.clone(),
            });
            api_version = None;
            kind = None;
        }
    }

    refs
}

fn is_literal_yaml_scalar(value: &str) -> bool {
    !value.is_empty() && !value.contains("{{") && !value.contains("{%") && !value.contains('$')
}

#[cfg(test)]
mod tests {
    use super::parse_resource_refs;

    #[test]
    fn parse_resource_refs_discovers_wrapped_and_inner_kinds() {
        let refs = parse_resource_refs(
            r#"
apiVersion: kubernetes.m.crossplane.io/v1alpha1
kind: Object
spec:
  forProvider:
    manifest:
      apiVersion: eks.amazonaws.com/v1
      kind: NodeClass
---
apiVersion: kubernetes.m.crossplane.io/v1alpha1
kind: Object
spec:
  forProvider:
    manifest:
      apiVersion: karpenter.sh/v1
      kind: NodePool
---
apiVersion: {{ $xr.apiVersion }}
kind: {{ $xr.kind }}
"#,
        );

        assert!(refs
            .iter()
            .any(|r| r.api_version == "kubernetes.m.crossplane.io/v1alpha1" && r.kind == "Object"));
        assert!(refs
            .iter()
            .any(|r| r.api_version == "eks.amazonaws.com/v1" && r.kind == "NodeClass"));
        assert!(refs
            .iter()
            .any(|r| r.api_version == "karpenter.sh/v1" && r.kind == "NodePool"));
        assert!(!refs.iter().any(|r| r.kind.contains("{{")));
    }
}
