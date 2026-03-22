use serde::Serialize;
use serde_yaml::{Mapping, Value};
use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct ReclaimSpec {
    api_version: String,
    kind: String,
    plural: String,
    group: String,
    project_slug: String,
    composed_resources: Vec<ResourceRef>,
    live_resolver: Option<String>,
}

#[derive(Clone, Serialize, Eq, Ord, PartialEq, PartialOrd)]
struct ResourceRef {
    api_version: String,
    kind: String,
}

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let xrs_root = xrs_root(&manifest_dir)?;

    println!("cargo:rerun-if-changed={}", xrs_root.display());
    println!("cargo:rerun-if-env-changed=HOPS_XRS_DIR");

    let specs = discover_reclaim_specs(&xrs_root)?;
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let output_path = out_dir.join("reclaim-metadata.json");
    fs::write(output_path, serde_json::to_vec_pretty(&specs)?)?;

    Ok(())
}

fn xrs_root(manifest_dir: &Path) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = env::var_os("HOPS_XRS_DIR") {
        return Ok(PathBuf::from(path));
    }

    Ok(manifest_dir.join("xrs"))
}

fn discover_reclaim_specs(xrs_root: &Path) -> Result<Vec<ReclaimSpec>, Box<dyn Error>> {
    let mut specs = Vec::new();
    visit_dirs(xrs_root, &mut |path| {
        if path.file_name().and_then(|n| n.to_str()) != Some("definition.yaml") {
            return Ok(());
        }

        let value = load_yaml(path)?;
        let root = value
            .as_mapping()
            .ok_or_else(|| format!("expected YAML mapping in {}", path.display()))?;
        let spec = get_mapping(root, "spec")
            .ok_or_else(|| format!("missing spec in {}", path.display()))?;
        let names = get_mapping(spec, "names")
            .ok_or_else(|| format!("missing spec.names in {}", path.display()))?;

        let group = get_string(spec, "group")
            .ok_or_else(|| format!("missing spec.group in {}", path.display()))?;
        let kind = get_string(names, "kind")
            .ok_or_else(|| format!("missing spec.names.kind in {}", path.display()))?;
        let plural = get_string(names, "plural")
            .ok_or_else(|| format!("missing spec.names.plural in {}", path.display()))?;
        let version = spec
            .get(vs("versions"))
            .and_then(Value::as_sequence)
            .and_then(|versions| versions.first())
            .and_then(Value::as_mapping)
            .and_then(|mapping| mapping.get(vs("name")))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("missing spec.versions[0].name in {}", path.display()))?;

        let project_root = path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .ok_or_else(|| format!("unexpected definition path layout: {}", path.display()))?;
        let project_slug = project_root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("missing project slug for {}", path.display()))?
            .to_string();

        specs.push(ReclaimSpec {
            api_version: format!("{group}/{version}"),
            kind: kind.clone(),
            plural,
            group,
            project_slug: project_slug.clone(),
            composed_resources: collect_composed_resources(project_root, &kind)?,
            live_resolver: live_resolver_for(&project_slug, &kind),
        });

        Ok(())
    })?;

    specs.sort_by(|a, b| a.kind.cmp(&b.kind));
    Ok(specs)
}

fn collect_composed_resources(project_root: &Path, xr_kind: &str) -> Result<Vec<ResourceRef>, Box<dyn Error>> {
    let render_root = project_root.join("functions").join("render");
    let mut resources = BTreeSet::new();
    if !render_root.is_dir() {
        return Ok(Vec::new());
    }

    visit_dirs(&render_root, &mut |path| {
        if path.extension().and_then(|ext| ext.to_str()) != Some("gotmpl") {
            return Ok(());
        }
        let content = fs::read_to_string(path)?;
        for resource in parse_resource_refs(&content) {
            if resource.kind != xr_kind {
                resources.insert(resource);
            }
        }
        Ok(())
    })?;

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

fn live_resolver_for(project_slug: &str, kind: &str) -> Option<String> {
    match (project_slug, kind) {
        ("network", "Network") => Some("aws-network-by-tag".to_string()),
        ("auto-eks-cluster", "AutoEKSCluster") => Some("aws-autoekscluster".to_string()),
        _ => None,
    }
}

fn load_yaml(path: &Path) -> Result<Value, Box<dyn Error>> {
    Ok(serde_yaml::from_str(&fs::read_to_string(path)?)?)
}

fn visit_dirs<F>(dir: &Path, cb: &mut F) -> Result<(), Box<dyn Error>>
where
    F: FnMut(&Path) -> Result<(), Box<dyn Error>>,
{
    if !dir.is_dir() {
        return Ok(());
    }

    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            visit_dirs(&path, cb)?;
        } else {
            cb(&path)?;
        }
    }

    Ok(())
}

fn get_mapping<'a>(map: &'a Mapping, key: &str) -> Option<&'a Mapping> {
    map.get(vs(key)).and_then(Value::as_mapping)
}

fn get_string(map: &Mapping, key: &str) -> Option<String> {
    map.get(vs(key))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn vs(value: &str) -> Value {
    Value::String(value.to_string())
}
