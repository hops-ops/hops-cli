use crate::commands::xr::helpers::types::MigrateArgs;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::process::Command;

const EXTERNAL_NAME_ANNOTATION: &str = "crossplane.io/external-name";
const COMPOSITION_NAME_ANNOTATIONS: [&str; 2] = [
    "gotemplating.fn.crossplane.io/composition-resource-name",
    "crossplane.io/composition-resource-name",
];

pub(crate) fn run(args: &MigrateArgs) -> Result<(), Box<dyn Error>> {
    if args.source_context.trim().is_empty() || args.target_context.trim().is_empty() {
        return Err("source and target contexts must not be empty".into());
    }
    if args.source_context == args.target_context {
        return Err("source and target contexts must be different".into());
    }

    let mut source = KubectlClient::new(&args.source_context);
    let mut target = KubectlClient::new(&args.target_context);

    let source_root = source.resolve_root(&args.kind, &args.name, &args.source_namespace)?;
    let target_root = target.resolve_root(&args.kind, &args.name, &args.target_namespace)?;
    if source_root.api_version != target_root.api_version || source_root.kind != target_root.kind {
        return Err(format!(
            "source XR {} does not match target XR {}",
            source_root, target_root
        )
        .into());
    }

    let source_graph = collect_graph(&mut source, &source_root)?;
    let target_graph = collect_graph(&mut target, &target_root)?;
    let plan = build_plan(&source_graph, &target_graph)?;
    let rendered = plan.render(
        &args.source_context,
        &args.source_namespace,
        &args.target_context,
        &args.target_namespace,
    );

    if let Some(output) = &args.output {
        fs::write(output, &rendered)?;
        log::info!("XR migration plan written to {output}");
    } else {
        print!("{rendered}");
    }

    if args.apply {
        let current_root = target.load(&target_root)?;
        if !is_observe_only(&current_root.management_policies) {
            return Err(format!(
                "target XR {} is no longer observe-only; refusing to apply patches",
                current_root.reference
            )
            .into());
        }

        let mut applied_count = 0usize;
        for entry in plan.entries.iter().filter(|entry| entry.needs_patch) {
            let current = target.load(&entry.target)?;
            if !is_observe_only(&current.management_policies) {
                return Err(format!(
                    "target managed resource {} is no longer observe-only; refusing to patch",
                    current.reference
                )
                .into());
            }
            match current
                .external_name
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                Some(value) if value == entry.external_name.as_str() => continue,
                Some(value) => {
                    return Err(format!(
                        "target managed resource {} changed external name to {:?} after planning; refusing to overwrite it",
                        current.reference, value
                    )
                    .into());
                }
                None => {}
            }

            target.patch_external_name(&entry.target, &entry.external_name)?;
            applied_count += 1;
            let observed = target.load(&entry.target)?;
            if observed.external_name.as_deref() != Some(entry.external_name.as_str()) {
                return Err(format!(
                    "target {} did not retain external name {:?} after patch",
                    entry.target, entry.external_name
                )
                .into());
            }
        }
        log::info!(
            "applied and verified {} external-name patch(es) in target context {}; {} source resource(s) remain deferred; source context was not modified",
            applied_count,
            args.target_context,
            plan.deferred_count()
        );
    } else {
        log::info!(
            "dry run only; rerun with --apply to patch {} target resource(s); {} source resource(s) remain deferred",
            plan.patch_count(),
            plan.deferred_count()
        );
    }

    Ok(())
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct ObjectRef {
    api_version: String,
    kind: String,
    namespace: Option<String>,
    name: String,
}

impl fmt::Display for ObjectRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let namespace = self.namespace.as_deref().unwrap_or("<cluster>");
        write!(
            formatter,
            "{}/{} {}/{}",
            self.api_version, self.kind, namespace, self.name
        )
    }
}

#[derive(Clone, Debug)]
struct LoadedResource {
    reference: ObjectRef,
    composition_name: Option<String>,
    external_name: Option<String>,
    management_policies: Vec<String>,
    resource_refs: Vec<ObjectRef>,
    is_managed: bool,
    is_composite: bool,
}

trait ResourceLoader {
    fn load(&mut self, reference: &ObjectRef) -> Result<LoadedResource, Box<dyn Error>>;
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MigrationKey {
    path: String,
    group: String,
    kind: String,
}

impl fmt::Display for MigrationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let group = if self.group.is_empty() {
            "core"
        } else {
            &self.group
        };
        write!(formatter, "{} [{} {}]", self.path, group, self.kind)
    }
}

#[derive(Clone, Debug)]
struct GraphResource {
    resource: LoadedResource,
}

#[derive(Clone, Debug, Default)]
struct ResourceGraph {
    resources: BTreeMap<MigrationKey, GraphResource>,
}

#[derive(Clone, Debug)]
struct MigrationEntry {
    key: MigrationKey,
    target: ObjectRef,
    external_name: String,
    needs_patch: bool,
}

#[derive(Clone, Debug, Default)]
struct MigrationPlan {
    entries: Vec<MigrationEntry>,
    deferred: Vec<MigrationKey>,
}

impl MigrationPlan {
    fn patch_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.needs_patch)
            .count()
    }

    fn matching_count(&self) -> usize {
        self.entries.len() - self.patch_count()
    }

    fn deferred_count(&self) -> usize {
        self.deferred.len()
    }

    fn render(
        &self,
        source_context: &str,
        source_namespace: &str,
        target_context: &str,
        target_namespace: &str,
    ) -> String {
        let mut rendered = format!(
            "XR migration plan\nsource context: {source_context}\nsource namespace: {source_namespace}\ntarget context: {target_context}\ntarget namespace: {target_namespace}\nmanaged resources: {}\ncurrently rendered: {}\npatches required: {}\nalready matching: {}\ndeferred: {}\n",
            self.entries.len() + self.deferred_count(),
            self.entries.len(),
            self.patch_count(),
            self.matching_count(),
            self.deferred_count()
        );

        for entry in &self.entries {
            let action = if entry.needs_patch { "PATCH" } else { "MATCH" };
            let external_name = serde_json::to_string(&entry.external_name)
                .unwrap_or_else(|_| "<invalid external name>".to_string());
            rendered.push_str(&format!(
                "{action}\t{}\t{}\t{external_name}\n",
                entry.key, entry.target
            ));
        }

        for key in &self.deferred {
            rendered.push_str(&format!("DEFER\t{key}\n"));
        }

        rendered
    }
}

fn collect_graph<L: ResourceLoader>(
    loader: &mut L,
    root: &ObjectRef,
) -> Result<ResourceGraph, Box<dyn Error>> {
    let root_resource = loader.load(root)?;
    let mut graph = ResourceGraph::default();
    let mut visited = HashSet::new();
    visit_resource(
        loader,
        root_resource,
        "<root>".to_string(),
        &mut graph,
        &mut visited,
    )?;
    Ok(graph)
}

fn visit_resource<L: ResourceLoader>(
    loader: &mut L,
    resource: LoadedResource,
    path: String,
    graph: &mut ResourceGraph,
    visited: &mut HashSet<ObjectRef>,
) -> Result<(), Box<dyn Error>> {
    if !visited.insert(resource.reference.clone()) {
        return Err(format!(
            "composition graph contains a repeated or cyclic reference to {}",
            resource.reference
        )
        .into());
    }

    let key = MigrationKey {
        path: path.clone(),
        group: api_group(&resource.reference.api_version).to_string(),
        kind: resource.reference.kind.clone(),
    };
    if graph
        .resources
        .insert(
            key.clone(),
            GraphResource {
                resource: resource.clone(),
            },
        )
        .is_some()
    {
        return Err(format!("composition graph contains duplicate key {key}").into());
    }

    for child_ref in &resource.resource_refs {
        let child = loader.load(child_ref)?;
        let segment = child.composition_name.clone().unwrap_or_else(|| {
            format!(
                "{}:{}",
                normalize_identity(&child.reference.kind),
                child.reference.name
            )
        });
        let child_path = if path == "<root>" {
            segment
        } else {
            format!("{path}/{segment}")
        };
        visit_resource(loader, child, child_path, graph, visited)?;
    }

    Ok(())
}

fn build_plan(
    source: &ResourceGraph,
    target: &ResourceGraph,
) -> Result<MigrationPlan, Box<dyn Error>> {
    let unsafe_targets = target
        .resources
        .iter()
        .filter(|(_, item)| item.resource.is_managed || item.resource.is_composite)
        .filter(|(_, item)| !is_observe_only(&item.resource.management_policies))
        .map(|(key, item)| {
            format!(
                "{} ({}) policies={:?}",
                key, item.resource.reference, item.resource.management_policies
            )
        })
        .collect::<Vec<_>>();
    if !unsafe_targets.is_empty() {
        return Err(format!(
            "target graph is not observe-only; refusing migration:\n- {}",
            unsafe_targets.join("\n- ")
        )
        .into());
    }

    let source_managed = source
        .resources
        .iter()
        .filter(|(_, item)| item.resource.is_managed)
        .collect::<BTreeMap<_, _>>();
    let target_managed = target
        .resources
        .iter()
        .filter(|(_, item)| item.resource.is_managed)
        .collect::<BTreeMap<_, _>>();

    let source_keys = source_managed.keys().copied().collect::<BTreeSet<_>>();
    let target_keys = target_managed.keys().copied().collect::<BTreeSet<_>>();
    let deferred = source_keys
        .difference(&target_keys)
        .map(|key| (*key).clone())
        .collect::<Vec<_>>();
    let extra = target_keys
        .difference(&source_keys)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if !extra.is_empty() {
        return Err(format!(
            "target managed-resource graph is not a subset of the source; extra in target: [{}]",
            extra.join(", ")
        )
        .into());
    }

    let mut entries = Vec::with_capacity(target_managed.len());
    for (key, target_item) in target_managed {
        let source_item = source_managed
            .get(key)
            .expect("target graph subset was checked above");
        let source_external_name = source_item
            .resource
            .external_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                format!(
                    "source managed resource {} ({}) has no external-name annotation",
                    key, source_item.resource.reference
                )
            })?;

        let needs_patch = match target_item
            .resource
            .external_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            None => true,
            Some(target_external_name) if target_external_name == source_external_name => false,
            Some(target_external_name) => {
                return Err(format!(
                    "target managed resource {} ({}) has conflicting external name {:?}; source uses {:?}",
                    key,
                    target_item.resource.reference,
                    target_external_name,
                    source_external_name
                )
                .into());
            }
        };

        entries.push(MigrationEntry {
            key: key.clone(),
            target: target_item.resource.reference.clone(),
            external_name: source_external_name.to_string(),
            needs_patch,
        });
    }

    Ok(MigrationPlan { entries, deferred })
}

fn is_observe_only(policies: &[String]) -> bool {
    !policies.is_empty()
        && policies.iter().any(|policy| policy == "Observe")
        && policies
            .iter()
            .all(|policy| matches!(policy.as_str(), "Observe" | "LateInitialize"))
}

#[derive(Clone, Debug)]
struct ApiResource {
    plural: String,
    namespaced: bool,
    is_managed: bool,
    is_composite: bool,
}

struct KubectlClient {
    context: String,
    discovery: HashMap<String, Vec<(String, ApiResource)>>,
}

impl KubectlClient {
    fn new(context: &str) -> Self {
        Self {
            context: context.to_string(),
            discovery: HashMap::new(),
        }
    }

    fn resolve_root(
        &self,
        needle: &str,
        name: &str,
        namespace: &str,
    ) -> Result<ObjectRef, Box<dyn Error>> {
        let crds = self.run_json(&["get", "crd", "-o", "json"])?;
        let needle = normalize_identity(needle);
        let matches = crds
            .get("items")
            .and_then(Value::as_array)
            .ok_or("kubectl CRD output missing items")?
            .iter()
            .filter_map(|item| {
                let spec = item.get("spec")?;
                let names = spec.get("names")?;
                let group = spec.get("group")?.as_str()?;
                let kind = names.get("kind")?.as_str()?;
                let plural = names.get("plural")?.as_str()?;
                let singular = names.get("singular").and_then(Value::as_str).unwrap_or("");
                let project = item
                    .pointer("/metadata/labels/hops.ops.com.ai~1project")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let candidates = [kind, plural, singular, group, project];
                if !candidates
                    .iter()
                    .any(|candidate| normalize_identity(candidate) == needle)
                {
                    return None;
                }
                let version = spec
                    .get("versions")?
                    .as_array()?
                    .iter()
                    .find(|version| version.get("served").and_then(Value::as_bool) == Some(true))
                    .or_else(|| spec.get("versions")?.as_array()?.first())?
                    .get("name")?
                    .as_str()?;
                let namespaced = spec.get("scope").and_then(Value::as_str) == Some("Namespaced");
                Some(ObjectRef {
                    api_version: format!("{group}/{version}"),
                    kind: kind.to_string(),
                    namespace: namespaced.then(|| namespace.to_string()),
                    name: name.to_string(),
                })
            })
            .collect::<Vec<_>>();

        match matches.len() {
            1 => Ok(matches[0].clone()),
            0 => Err(format!(
                "no XR CRD matching {needle:?} exists in context {}",
                self.context
            )
            .into()),
            _ => Err(format!(
                "multiple XR CRDs matching {needle:?} exist in context {}",
                self.context
            )
            .into()),
        }
    }

    fn api_resource(
        &mut self,
        api_version: &str,
        kind: &str,
    ) -> Result<ApiResource, Box<dyn Error>> {
        if !self.discovery.contains_key(api_version) {
            let discovery_path = match api_version.split_once('/') {
                Some((group, version)) => format!("/apis/{group}/{version}"),
                None => format!("/api/{api_version}"),
            };
            let discovered = self.run_json(&["get", "--raw", &discovery_path])?;
            let resources = discovered
                .get("resources")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("API discovery for {api_version} has no resources"))?
                .iter()
                .filter_map(|item| {
                    let plural = item.get("name")?.as_str()?;
                    if plural.contains('/') {
                        return None;
                    }
                    let discovered_kind = item.get("kind")?.as_str()?.to_string();
                    let categories = item
                        .get("categories")
                        .and_then(Value::as_array)
                        .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                        .unwrap_or_default();
                    Some((
                        discovered_kind,
                        ApiResource {
                            plural: plural.to_string(),
                            namespaced: item
                                .get("namespaced")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            is_managed: categories.contains(&"managed"),
                            is_composite: categories.contains(&"composite"),
                        },
                    ))
                })
                .collect::<Vec<_>>();
            self.discovery.insert(api_version.to_string(), resources);
        }

        self.discovery
            .get(api_version)
            .and_then(|resources| {
                resources
                    .iter()
                    .find(|(discovered_kind, _)| discovered_kind == kind)
                    .map(|(_, resource)| resource.clone())
            })
            .ok_or_else(|| {
                format!(
                    "API discovery in context {} has no {} kind {}",
                    self.context, api_version, kind
                )
                .into()
            })
    }

    fn run_json(&self, args: &[&str]) -> Result<Value, Box<dyn Error>> {
        let output = Command::new("kubectl")
            .arg("--context")
            .arg(&self.context)
            .args(args)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(
                format!("kubectl context {} failed: {}", self.context, stderr.trim()).into(),
            );
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    fn patch_external_name(
        &mut self,
        reference: &ObjectRef,
        external_name: &str,
    ) -> Result<(), Box<dyn Error>> {
        let api = self.api_resource(&reference.api_version, &reference.kind)?;
        let group = api_group(&reference.api_version);
        let resource_type = if group.is_empty() {
            api.plural
        } else {
            format!("{}.{}", api.plural, group)
        };
        let mut args = vec![
            "--context".to_string(),
            self.context.clone(),
            "patch".to_string(),
            resource_type,
            reference.name.clone(),
        ];
        if let Some(namespace) = &reference.namespace {
            args.extend(["--namespace".to_string(), namespace.clone()]);
        }
        args.extend([
            "--type".to_string(),
            "merge".to_string(),
            "--field-manager".to_string(),
            "hops-xr-migrate".to_string(),
            "--patch".to_string(),
            serde_json::json!({
                "metadata": {
                    "annotations": {
                        EXTERNAL_NAME_ANNOTATION: external_name
                    }
                }
            })
            .to_string(),
        ]);

        let output = Command::new("kubectl").args(&args).output()?;
        if !output.status.success() {
            return Err(format!(
                "failed to patch target {} in context {}: {}",
                reference,
                self.context,
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        Ok(())
    }
}

impl ResourceLoader for KubectlClient {
    fn load(&mut self, reference: &ObjectRef) -> Result<LoadedResource, Box<dyn Error>> {
        let api = self.api_resource(&reference.api_version, &reference.kind)?;
        let group = api_group(&reference.api_version);
        let resource_type = if group.is_empty() {
            api.plural.clone()
        } else {
            format!("{}.{}", api.plural, group)
        };
        let mut owned_args = vec!["get".to_string(), resource_type, reference.name.clone()];
        if api.namespaced {
            let namespace = reference
                .namespace
                .as_deref()
                .ok_or_else(|| format!("namespaced resource {} has no namespace", reference))?;
            owned_args.extend(["--namespace".to_string(), namespace.to_string()]);
        }
        owned_args.extend(["-o".to_string(), "json".to_string()]);
        let borrowed_args = owned_args.iter().map(String::as_str).collect::<Vec<_>>();
        let value = self.run_json(&borrowed_args)?;

        let metadata = value
            .get("metadata")
            .ok_or_else(|| format!("resource {} has no metadata", reference))?;
        let actual = ObjectRef {
            api_version: value
                .get("apiVersion")
                .and_then(Value::as_str)
                .unwrap_or(&reference.api_version)
                .to_string(),
            kind: value
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or(&reference.kind)
                .to_string(),
            namespace: api.namespaced.then(|| {
                metadata
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| reference.namespace.as_deref().unwrap_or("default"))
                    .to_string()
            }),
            name: metadata
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&reference.name)
                .to_string(),
        };
        let annotations = metadata.get("annotations").and_then(Value::as_object);
        let composition_name = COMPOSITION_NAME_ANNOTATIONS.iter().find_map(|key| {
            annotations
                .and_then(|annotations| annotations.get(*key))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        });
        let external_name = annotations
            .and_then(|annotations| annotations.get(EXTERNAL_NAME_ANNOTATION))
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let management_policies = value
            .pointer("/spec/managementPolicies")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let resource_refs = value
            .pointer("/spec/crossplane/resourceRefs")
            .and_then(Value::as_array)
            .map(|refs| {
                refs.iter()
                    .map(|child| parse_resource_ref(child, actual.namespace.as_deref()))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();

        Ok(LoadedResource {
            reference: actual,
            composition_name,
            external_name,
            management_policies,
            resource_refs,
            is_managed: api.is_managed,
            is_composite: api.is_composite,
        })
    }
}

fn parse_resource_ref(
    value: &Value,
    default_namespace: Option<&str>,
) -> Result<ObjectRef, Box<dyn Error>> {
    let api_version = required_string(value, "apiVersion")?;
    let kind = required_string(value, "kind")?;
    let name = required_string(value, "name")?;
    let namespace = value
        .get("namespace")
        .and_then(Value::as_str)
        .or(default_namespace)
        .map(ToString::to_string);
    Ok(ObjectRef {
        api_version,
        kind,
        namespace,
        name,
    })
}

fn required_string(value: &Value, key: &str) -> Result<String, Box<dyn Error>> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| format!("resource reference is missing {key}").into())
}

fn api_group(api_version: &str) -> &str {
    api_version
        .split_once('/')
        .map(|(group, _)| group)
        .unwrap_or("")
}

fn normalize_identity(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Default)]
    struct FakeLoader {
        resources: HashMap<ObjectRef, LoadedResource>,
    }

    impl ResourceLoader for FakeLoader {
        fn load(&mut self, reference: &ObjectRef) -> Result<LoadedResource, Box<dyn Error>> {
            self.resources
                .get(reference)
                .cloned()
                .ok_or_else(|| format!("missing fixture {reference}").into())
        }
    }

    fn reference(api_version: &str, kind: &str, name: &str) -> ObjectRef {
        ObjectRef {
            api_version: api_version.to_string(),
            kind: kind.to_string(),
            namespace: Some("default".to_string()),
            name: name.to_string(),
        }
    }

    fn resource(
        reference: ObjectRef,
        composition_name: Option<&str>,
        external_name: Option<&str>,
        policies: &[&str],
        refs: Vec<ObjectRef>,
        managed: bool,
        composite: bool,
    ) -> LoadedResource {
        LoadedResource {
            reference,
            composition_name: composition_name.map(ToString::to_string),
            external_name: external_name.map(ToString::to_string),
            management_policies: policies.iter().map(|value| value.to_string()).collect(),
            resource_refs: refs,
            is_managed: managed,
            is_composite: composite,
        }
    }

    fn graph_with_external_name(external_name: Option<&str>, policies: &[&str]) -> ResourceGraph {
        let root = reference(
            "aws.hops.ops.com.ai/v1alpha1",
            "RegistryCache",
            "production",
        );
        let child = reference("s3.aws.m.upbound.io/v1beta1", "Bucket", "bucket");
        let mut loader = FakeLoader::default();
        loader.resources.insert(
            root.clone(),
            resource(
                root.clone(),
                None,
                None,
                policies,
                vec![child.clone()],
                false,
                true,
            ),
        );
        loader.resources.insert(
            child.clone(),
            resource(
                child,
                Some("bucket"),
                external_name,
                policies,
                Vec::new(),
                true,
                false,
            ),
        );
        collect_graph(&mut loader, &root).expect("graph")
    }

    #[test]
    fn collect_graph_recurses_through_composites() {
        let root = reference(
            "aws.hops.ops.com.ai/v1alpha1",
            "RegistryCache",
            "production",
        );
        let child = reference(
            "aws.hops.ops.com.ai/v1alpha1",
            "PodIdentity",
            "distribution",
        );
        let leaf = reference("iam.aws.m.upbound.io/v1beta1", "Role", "distribution");
        let mut loader = FakeLoader::default();
        loader.resources.insert(
            root.clone(),
            resource(
                root.clone(),
                None,
                None,
                &["Observe"],
                vec![child.clone()],
                false,
                true,
            ),
        );
        loader.resources.insert(
            child.clone(),
            resource(
                child,
                Some("pod-identity"),
                None,
                &["Observe"],
                vec![leaf.clone()],
                false,
                true,
            ),
        );
        loader.resources.insert(
            leaf.clone(),
            resource(
                leaf,
                Some("role"),
                Some("role-id"),
                &["Observe"],
                Vec::new(),
                true,
                false,
            ),
        );

        let graph = collect_graph(&mut loader, &root).expect("graph");
        assert!(graph.resources.keys().any(|key| {
            key.path == "pod-identity/role"
                && key.group == "iam.aws.m.upbound.io"
                && key.kind == "Role"
        }));
    }

    #[test]
    fn build_plan_is_noop_for_matching_external_names() {
        let source = graph_with_external_name(Some("bucket-id"), &["*"]);
        let target = graph_with_external_name(Some("bucket-id"), &["Observe"]);
        let plan = build_plan(&source, &target).expect("plan");
        assert_eq!(plan.patch_count(), 0);
        assert_eq!(plan.matching_count(), 1);
    }

    #[test]
    fn build_plan_rejects_conflicting_external_names() {
        let source = graph_with_external_name(Some("source-id"), &["*"]);
        let target = graph_with_external_name(Some("target-id"), &["Observe"]);
        let error = build_plan(&source, &target).expect_err("conflict must fail");
        assert!(error.to_string().contains("conflicting external name"));
    }

    #[test]
    fn build_plan_rejects_unsafe_target_policies() {
        let source = graph_with_external_name(Some("bucket-id"), &["*"]);
        let target = graph_with_external_name(None, &["Observe", "Create"]);
        let error = build_plan(&source, &target).expect_err("unsafe target must fail");
        assert!(error
            .to_string()
            .contains("target graph is not observe-only"));
    }

    #[test]
    fn build_plan_patches_missing_target_identity() {
        let source = graph_with_external_name(Some("bucket-id"), &["*"]);
        let target = graph_with_external_name(None, &["Observe", "LateInitialize"]);
        let plan = build_plan(&source, &target).expect("plan");
        assert_eq!(plan.patch_count(), 1);
        assert_eq!(plan.entries[0].external_name, "bucket-id");
    }

    #[test]
    fn build_plan_defers_source_resources_missing_from_target() {
        let mut source = graph_with_external_name(Some("bucket-id"), &["*"]);
        let target = graph_with_external_name(None, &["Observe", "LateInitialize"]);
        let later_ref = reference("s3.aws.m.upbound.io/v1beta1", "Bucket", "later");
        let later_key = MigrationKey {
            path: "later".to_string(),
            group: "s3.aws.m.upbound.io".to_string(),
            kind: "Bucket".to_string(),
        };
        source.resources.insert(
            later_key.clone(),
            GraphResource {
                resource: resource(
                    later_ref,
                    Some("later"),
                    Some("later-id"),
                    &["*"],
                    Vec::new(),
                    true,
                    false,
                ),
            },
        );

        let plan = build_plan(&source, &target).expect("progressive plan");
        assert_eq!(plan.patch_count(), 1);
        assert_eq!(plan.deferred, vec![later_key]);
        assert!(plan
            .render("source", "default", "target", "target")
            .contains("DEFER\tlater [s3.aws.m.upbound.io Bucket]"));
    }

    #[test]
    fn build_plan_rejects_target_resources_missing_from_source() {
        let source = graph_with_external_name(Some("bucket-id"), &["*"]);
        let mut target = graph_with_external_name(None, &["Observe"]);
        let extra_ref = reference("s3.aws.m.upbound.io/v1beta1", "Bucket", "extra");
        let extra_key = MigrationKey {
            path: "extra".to_string(),
            group: "s3.aws.m.upbound.io".to_string(),
            kind: "Bucket".to_string(),
        };
        target.resources.insert(
            extra_key,
            GraphResource {
                resource: resource(
                    extra_ref,
                    Some("extra"),
                    None,
                    &["Observe"],
                    Vec::new(),
                    true,
                    false,
                ),
            },
        );

        let error = build_plan(&source, &target).expect_err("target-only resource must fail");
        assert!(error
            .to_string()
            .contains("target managed-resource graph is not a subset"));
    }

    #[test]
    fn migrate_args_require_explicit_contexts() {
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            migrate: MigrateArgs,
        }

        let cli = Cli::try_parse_from([
            "test",
            "--kind",
            "RegistryCache",
            "--name",
            "production",
            "--source-namespace",
            "default",
            "--target-namespace",
            "production",
            "--source-context",
            "kind-bootstrap",
            "--target-context",
            "production",
            "--apply",
        ])
        .expect("parse");

        assert_eq!(cli.migrate.source_namespace, "default");
        assert_eq!(cli.migrate.target_namespace, "production");
        assert_eq!(cli.migrate.source_context, "kind-bootstrap");
        assert_eq!(cli.migrate.target_context, "production");
        assert!(cli.migrate.apply);
    }
}
