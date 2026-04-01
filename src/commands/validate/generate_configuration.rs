use clap::Args;
use serde_yaml::{Mapping, Value};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_UPBOUND_FILE: &str = "upbound.yaml";
const DEFAULT_GITIGNORE_PATTERN: &str = "apis/**/configuration.yaml";

#[derive(Args, Debug)]
pub struct GenerateArgs {
    /// Path to the project root (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,

    /// Path to the API directory (defaults to auto-detect via apis/*/definition.yaml)
    #[arg(long)]
    pub api_path: Option<String>,

    /// Path to the Upbound project metadata file (relative to --path unless absolute)
    #[arg(long, default_value = DEFAULT_UPBOUND_FILE)]
    pub upbound_file: String,

    /// Skip updating .gitignore with apis/**/configuration.yaml
    #[arg(long)]
    pub no_gitignore_update: bool,
}

pub fn run(args: &GenerateArgs) -> Result<(), Box<dyn Error>> {
    let project_root = PathBuf::from(&args.path);
    let api_path = resolve_api_path(&project_root, args.api_path.as_deref())?;
    let upbound_path = resolve_path(&project_root, Path::new(&args.upbound_file));

    if !upbound_path.is_file() {
        return Err(format!(
            "expected upbound metadata file at {}",
            upbound_path.display()
        )
        .into());
    }

    let upbound_contents = fs::read_to_string(&upbound_path)?;
    let configuration_yaml = render_configuration_yaml(&upbound_contents)?;

    let output_path = api_path.join("configuration.yaml");
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output_path, configuration_yaml)?;
    log::info!("Wrote {}", output_path.display());

    if !args.no_gitignore_update {
        let gitignore_path = project_root.join(".gitignore");
        if ensure_gitignore_entry(&gitignore_path, DEFAULT_GITIGNORE_PATTERN)? {
            log::info!(
                "Added '{}' to {}",
                DEFAULT_GITIGNORE_PATTERN,
                gitignore_path.display()
            );
        } else {
            log::info!(
                "'{}' already present in {}",
                DEFAULT_GITIGNORE_PATTERN,
                gitignore_path.display()
            );
        }
    }

    Ok(())
}

fn resolve_api_path(
    project_root: &Path,
    api_path: Option<&str>,
) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = api_path {
        return Ok(resolve_path(project_root, Path::new(path)));
    }
    auto_detect_api_path(project_root)
}

fn resolve_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

fn auto_detect_api_path(project_root: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let apis_dir = project_root.join("apis");
    if !apis_dir.is_dir() {
        return Err(format!(
            "could not auto-detect api path: {} does not exist. Pass --api-path.",
            apis_dir.display()
        )
        .into());
    }

    let mut matches = Vec::new();
    for entry in fs::read_dir(&apis_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && path.join("definition.yaml").is_file() {
            matches.push(path);
        }
    }
    matches.sort();

    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(format!(
            "could not auto-detect api path under {} (expected apis/*/definition.yaml). Pass --api-path.",
            apis_dir.display()
        )
        .into()),
        _ => {
            let options = matches
                .iter()
                .map(|m| m.strip_prefix(project_root).unwrap_or(m).display().to_string())
                .collect::<Vec<String>>()
                .join(", ");
            Err(format!(
                "multiple api paths found ({options}). Pass --api-path explicitly."
            )
            .into())
        }
    }
}

fn render_configuration_yaml(upbound_yaml: &str) -> Result<String, Box<dyn Error>> {
    let project: Value = serde_yaml::from_str(upbound_yaml)?;
    let project_map = project
        .as_mapping()
        .ok_or("upbound.yaml must be a YAML mapping at the document root")?;

    let metadata = get_mapping(project_map, "metadata");
    let spec = get_mapping(project_map, "spec");

    let name = metadata
        .and_then(|m| get_string(m, "name"))
        .ok_or("upbound.yaml is missing metadata.name")?;

    let maintainer = spec
        .and_then(|m| get_string(m, "maintainer"))
        .map(|s| sanitize_maintainer(&s))
        .unwrap_or_default();
    let source = spec
        .and_then(|m| get_string(m, "source"))
        .unwrap_or_default();
    let description = spec
        .and_then(|m| get_string(m, "description"))
        .unwrap_or_default();

    let mut annotations = Mapping::new();
    if !maintainer.is_empty() {
        annotations.insert(vs("meta.crossplane.io/maintainer"), vs(&maintainer));
    }
    if !source.is_empty() {
        annotations.insert(vs("meta.crossplane.io/source"), vs(&source));
    }
    if !description.is_empty() {
        annotations.insert(vs("meta.crossplane.io/description"), vs(&description));
    }

    let mut depends_on = Vec::new();
    if let Some(dep_values) = spec
        .and_then(|m| m.get(vs("dependsOn")))
        .and_then(Value::as_sequence)
    {
        for dep in dep_values {
            let Some(dep_map) = dep.as_mapping() else {
                continue;
            };

            let kind = dep_map
                .get(vs("kind"))
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            let Some(package) = dep_map.get(vs("package")).and_then(Value::as_str) else {
                continue;
            };

            let key = match kind.as_str() {
                "provider" => "provider",
                "function" => "function",
                "configuration" => "configuration",
                _ => continue,
            };

            let mut dep_item = Mapping::new();
            dep_item.insert(vs(key), vs(package));
            if let Some(version) = dep_map.get(vs("version")) {
                if !version.is_null() {
                    dep_item.insert(vs("version"), version.clone());
                }
            }
            depends_on.push(Value::Mapping(dep_item));
        }
    }

    let mut metadata_out = Mapping::new();
    metadata_out.insert(vs("name"), vs(&name));
    metadata_out.insert(vs("annotations"), Value::Mapping(annotations));

    let mut spec_out = Mapping::new();
    spec_out.insert(vs("dependsOn"), Value::Sequence(depends_on));

    let mut output = Mapping::new();
    output.insert(vs("apiVersion"), vs("meta.pkg.crossplane.io/v1alpha1"));
    output.insert(vs("kind"), vs("Configuration"));
    output.insert(vs("metadata"), Value::Mapping(metadata_out));
    output.insert(vs("spec"), Value::Mapping(spec_out));

    let mut rendered = serde_yaml::to_string(&Value::Mapping(output))?;
    if rendered.starts_with("---\n") {
        rendered = rendered.replacen("---\n", "", 1);
    }
    Ok(rendered)
}

fn get_mapping<'a>(map: &'a Mapping, key: &str) -> Option<&'a Mapping> {
    map.get(vs(key)).and_then(Value::as_mapping)
}

fn get_string(map: &Mapping, key: &str) -> Option<String> {
    map.get(vs(key))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn sanitize_maintainer(maintainer: &str) -> String {
    let trimmed = maintainer.trim();
    if !trimmed.ends_with('>') {
        return trimmed.to_string();
    }

    let Some(start) = trimmed.rfind('<') else {
        return trimmed.to_string();
    };
    if start == 0 || start >= trimmed.len() - 1 {
        return trimmed.to_string();
    }

    let inner = &trimmed[start + 1..trimmed.len() - 1];
    if inner.is_empty() || inner.contains('<') || inner.contains('>') {
        return trimmed.to_string();
    }

    trimmed[..start].trim_end().to_string()
}

fn ensure_gitignore_entry(path: &Path, pattern: &str) -> Result<bool, Box<dyn Error>> {
    let original = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };

    let (updated, changed) = append_gitignore_pattern_if_missing(&original, pattern);
    if changed {
        fs::write(path, updated)?;
    }

    Ok(changed)
}

fn append_gitignore_pattern_if_missing(contents: &str, pattern: &str) -> (String, bool) {
    if contents.lines().any(|line| line.trim() == pattern) {
        return (contents.to_string(), false);
    }

    let mut updated = contents.to_string();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(pattern);
    updated.push('\n');

    (updated, true)
}

fn vs(value: &str) -> Value {
    Value::String(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn render_configuration_yaml_matches_expected_shape() {
        let input = r#"
metadata:
  name: stack-aws-observe
spec:
  maintainer: Team Name <team@example.com>
  source: https://github.com/example/repo
  description: Example package
  dependsOn:
    - kind: Provider
      package: xpkg.crossplane.io/upbound/provider-aws-s3
      version: ">=v1.0.0"
    - kind: Function
      package: xpkg.crossplane.io/crossplane-contrib/function-go-templating
    - kind: Configuration
      package: xpkg.crossplane.io/example/configuration-base
      version: v0.1.0
    - kind: Unknown
      package: ignore/me
"#;

        let output = render_configuration_yaml(input).expect("render should succeed");
        let parsed: Value = serde_yaml::from_str(&output).expect("output should be valid yaml");

        assert_eq!(
            parsed
                .get("metadata")
                .and_then(Value::as_mapping)
                .and_then(|m| m.get(vs("name")))
                .and_then(Value::as_str),
            Some("stack-aws-observe")
        );
        assert_eq!(
            parsed
                .get("metadata")
                .and_then(Value::as_mapping)
                .and_then(|m| m.get(vs("annotations")))
                .and_then(Value::as_mapping)
                .and_then(|m| m.get(vs("meta.crossplane.io/maintainer")))
                .and_then(Value::as_str),
            Some("Team Name")
        );
        assert_eq!(
            parsed
                .get("spec")
                .and_then(Value::as_mapping)
                .and_then(|m| m.get(vs("dependsOn")))
                .and_then(Value::as_sequence)
                .map(Vec::len),
            Some(3)
        );
    }

    #[test]
    fn append_gitignore_pattern_is_idempotent() {
        let input = "# existing\n";
        let (updated, changed) =
            append_gitignore_pattern_if_missing(input, "apis/**/configuration.yaml");
        assert!(changed);
        assert_eq!(updated, "# existing\napis/**/configuration.yaml\n");

        let (updated_again, changed_again) =
            append_gitignore_pattern_if_missing(&updated, "apis/**/configuration.yaml");
        assert!(!changed_again);
        assert_eq!(updated_again, updated);
    }

    #[test]
    fn auto_detect_api_path_finds_single_definition() {
        let tmp = temp_dir("hops-generate-config");
        let project_root = tmp.join("project");
        let api_dir = project_root.join("apis").join("observes");
        fs::create_dir_all(&api_dir).expect("should create api dir");
        fs::write(
            api_dir.join("definition.yaml"),
            "apiVersion: apiextensions.crossplane.io/v1",
        )
        .expect("should write definition");

        let detected = auto_detect_api_path(&project_root).expect("should detect api path");
        assert_eq!(detected, api_dir);

        fs::remove_dir_all(tmp).expect("cleanup should succeed");
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be monotonic")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{ts}-{}", std::process::id()))
    }
}
