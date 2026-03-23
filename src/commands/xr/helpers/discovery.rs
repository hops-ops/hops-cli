use crate::commands::local::run_cmd_output;
use crate::commands::xr::helpers::manifest::{ensure_mapping, strip_runtime_k8s_fields, vs};
use crate::commands::xr::helpers::types::{
    ManagedResourcePatch, ManifestSource, ReclaimSpec, NETWORK_TAG_KEY, ROUTE_TABLE_AZ_TAG_KEY,
    SUBNET_TIER_TAG_KEY,
};
use serde_json::Value as JsonValue;
use serde_yaml::{Mapping, Value};
use std::collections::BTreeMap;
use std::error::Error;

pub(crate) fn load_existing_cluster_manifest(
    spec: &ReclaimSpec,
    object_name: &str,
    namespace: &str,
) -> Result<(Value, ManifestSource, Vec<String>), Box<dyn Error>> {
    let resource = format!("{}.{}", spec.plural, spec.group);
    let mut notes = Vec::new();

    match run_cmd_output(
        "kubectl",
        &["get", &resource, object_name, "-n", namespace, "-o", "yaml"],
    ) {
        Ok(yaml) => {
            let mut value: Value = serde_yaml::from_str(&yaml)?;
            strip_runtime_k8s_fields(&mut value);
            notes.push("loaded existing XR from cluster".to_string());
            Ok((value, ManifestSource::Cluster, notes))
        }
        Err(err) => Err(format!("failed to inspect live XR from cluster: {}", err).into()),
    }
}

pub(crate) fn apply_live_aws(
    spec: &ReclaimSpec,
    manifest: &mut Value,
    selector_name: &str,
    region: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    match spec.live_resolver.as_deref() {
        Some("aws-network-by-tag") => apply_live_aws_network(manifest, selector_name, region),
        Some("aws-autoekscluster") => {
            apply_live_aws_autoekscluster(manifest, selector_name, region)
        }
        Some(resolver) => Err(format!("live AWS resolver '{resolver}' is not implemented").into()),
        None => Err(format!("{} has no live AWS resolver", spec.kind).into()),
    }
}

pub(crate) fn build_managed_resource_adoption_patches(
    spec: &ReclaimSpec,
    xr_name: &str,
) -> Result<Vec<ManagedResourcePatch>, Box<dyn Error>> {
    match spec.kind.as_str() {
        "AutoEKSCluster" => build_autoekscluster_adoption_patches(xr_name),
        _ => Ok(Vec::new()),
    }
}

pub(crate) fn render_managed_resource_patches(
    patches: &[ManagedResourcePatch],
) -> Result<String, Box<dyn Error>> {
    let mut docs = Vec::new();
    for patch in patches {
        let mut metadata = Mapping::new();
        metadata.insert(vs("name"), vs(&patch.name));
        metadata.insert(vs("namespace"), vs(&patch.namespace));

        let mut root = Mapping::new();
        root.insert(vs("apiVersion"), vs(&patch.api_version));
        root.insert(vs("kind"), vs(&patch.kind));

        if let Some(external_name) = &patch.external_name {
            let mut annotations = Mapping::new();
            annotations.insert(vs("crossplane.io/external-name"), vs(external_name));
            metadata.insert(vs("annotations"), Value::Mapping(annotations));
        }

        root.insert(vs("metadata"), Value::Mapping(metadata));

        if let Some(management_policies) = &patch.management_policies {
            let mut spec = Mapping::new();
            spec.insert(
                vs("managementPolicies"),
                Value::Sequence(
                    management_policies
                        .iter()
                        .map(|policy| vs(policy))
                        .collect(),
                ),
            );
            root.insert(vs("spec"), Value::Mapping(spec));
        }

        docs.push(Value::Mapping(root));
    }

    let mut out = String::new();
    for doc in docs {
        if !out.is_empty() {
            out.push_str("---\n");
        }
        let mut yaml = serde_yaml::to_string(&doc)?;
        if yaml.starts_with("---\n") {
            yaml = yaml.replacen("---\n", "", 1);
        }
        out.push_str(&yaml);
    }
    Ok(out)
}

fn build_autoekscluster_adoption_patches(
    xr_name: &str,
) -> Result<Vec<ManagedResourcePatch>, Box<dyn Error>> {
    let managed_json = run_cmd_output(
        "kubectl",
        &[
            "get",
            "managed",
            "-A",
            "-l",
            &format!("crossplane.io/composite={xr_name}"),
            "-o",
            "json",
        ],
    )?;
    let managed: JsonValue = serde_json::from_str(&managed_json)?;
    let items = managed
        .get("items")
        .and_then(JsonValue::as_array)
        .ok_or("kubectl managed output missing items")?;

    let mut patches = Vec::new();
    for item in items {
        let kind = item
            .get("kind")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        let name = item
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(JsonValue::as_str)
            .ok_or("managed resource missing metadata.name")?;
        let namespace = item
            .get("metadata")
            .and_then(|m| m.get("namespace"))
            .and_then(JsonValue::as_str)
            .unwrap_or("default");
        let api_version = item
            .get("apiVersion")
            .and_then(JsonValue::as_str)
            .ok_or("managed resource missing apiVersion")?;

        let mut patch = ManagedResourcePatch {
            api_version: api_version.to_string(),
            kind: kind.to_string(),
            namespace: namespace.to_string(),
            name: name.to_string(),
            external_name: None,
            management_policies: None,
        };

        if external_name_annotation(item).is_none() && kind == "RolePolicyAttachment" {
            let role = get_json_path(item, &["spec", "forProvider", "role"])
                .and_then(JsonValue::as_str)
                .or_else(|| {
                    get_json_path(item, &["spec", "forProvider", "roleRef", "name"])
                        .and_then(JsonValue::as_str)
                })
                .ok_or("RolePolicyAttachment missing role identity")?;
            let policy_arn = get_json_path(item, &["spec", "forProvider", "policyArn"])
                .and_then(JsonValue::as_str)
                .ok_or("RolePolicyAttachment missing policyArn")?;
            patch.external_name = Some(format!("{role}/{policy_arn}"));
        }

        if kind == "Object"
            && matches!(
                composition_resource_name(item),
                Some("k8s-provider-config") | Some("helm-provider-config")
            )
            && is_observe_only(item)
        {
            patch.management_policies = Some(vec![
                "Create".to_string(),
                "Observe".to_string(),
                "Update".to_string(),
                "LateInitialize".to_string(),
            ]);
        }

        if patch.external_name.is_some() || patch.management_policies.is_some() {
            patches.push(patch);
        }
    }

    Ok(patches)
}

fn composition_resource_name<'a>(item: &'a JsonValue) -> Option<&'a str> {
    item.get("metadata")
        .and_then(|m| m.get("annotations"))
        .and_then(JsonValue::as_object)
        .and_then(|ann| {
            ann.get("gotemplating.fn.crossplane.io/composition-resource-name")
                .or_else(|| ann.get("crossplane.io/composition-resource-name"))
        })
        .and_then(JsonValue::as_str)
}

fn external_name_annotation<'a>(item: &'a JsonValue) -> Option<&'a str> {
    item.get("metadata")
        .and_then(|m| m.get("annotations"))
        .and_then(|ann| ann.get("crossplane.io/external-name"))
        .and_then(JsonValue::as_str)
}

fn is_observe_only(item: &JsonValue) -> bool {
    let Some(policies) =
        get_json_path(item, &["spec", "managementPolicies"]).and_then(JsonValue::as_array)
    else {
        return false;
    };

    let mut values = policies
        .iter()
        .filter_map(JsonValue::as_str)
        .collect::<Vec<_>>();
    values.sort_unstable();
    values == ["LateInitialize", "Observe"]
}

#[cfg(test)]
pub(crate) fn orphan_management_policies(item: &JsonValue) -> Option<Vec<String>> {
    let current =
        get_json_path(item, &["spec", "managementPolicies"]).and_then(JsonValue::as_array);

    let mut policies = match current {
        Some(values) => {
            let items = values
                .iter()
                .filter_map(JsonValue::as_str)
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

    policies.sort_unstable();
    policies.dedup();

    let current_normalized = current.map(|values| {
        let mut items = values
            .iter()
            .filter_map(JsonValue::as_str)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        items.sort_unstable();
        items.dedup();
        items
    });

    if current_normalized.as_ref() == Some(&policies) {
        None
    } else {
        Some(policies)
    }
}

fn get_json_path<'a>(value: &'a JsonValue, path: &[&str]) -> Option<&'a JsonValue> {
    let mut current = value;
    for segment in path {
        current = match current {
            JsonValue::Object(map) => map.get(*segment)?,
            JsonValue::Array(items) => items.first()?,
            _ => return None,
        };
    }
    Some(current)
}

fn apply_live_aws_autoekscluster(
    manifest: &mut Value,
    selector_name: &str,
    region: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut notes = Vec::new();
    let root = manifest
        .as_mapping_mut()
        .ok_or("manifest root must be a mapping")?;
    let spec = ensure_mapping(root, "spec");

    let cluster_json = aws_json(&[
        "eks",
        "describe-cluster",
        "--name",
        selector_name,
        "--region",
        region,
    ])?;
    let cluster = cluster_json
        .get("cluster")
        .ok_or_else(|| format!("AWS did not return cluster details for '{selector_name}'"))?;

    spec.insert(vs("clusterName"), vs(selector_name));
    spec.insert(vs("region"), vs(region));

    if let Some(version) = cluster.get("version").and_then(JsonValue::as_str) {
        spec.insert(vs("version"), vs(version));
        notes.push(format!("version <- {version}"));
    }

    let account_id = aws_json(&["sts", "get-caller-identity"])?
        .get("Account")
        .and_then(JsonValue::as_str)
        .map(ToString::to_string)
        .ok_or("unable to determine AWS account ID from sts get-caller-identity")?;
    spec.insert(vs("accountId"), vs(&account_id));
    notes.push(format!("accountId <- {account_id}"));

    if let Some(subnets) = cluster
        .get("resourcesVpcConfig")
        .and_then(|cfg| cfg.get("subnetIds"))
        .and_then(JsonValue::as_array)
    {
        let subnet_values = subnets
            .iter()
            .filter_map(JsonValue::as_str)
            .map(vs)
            .collect::<Vec<_>>();
        if !subnet_values.is_empty() {
            spec.insert(vs("subnetIds"), Value::Sequence(subnet_values));
            notes.push("subnetIds <- eks cluster VPC config".to_string());
        }
    }

    if let Some(vpc_cfg) = cluster.get("resourcesVpcConfig") {
        if let Some(private_access) = vpc_cfg
            .get("endpointPrivateAccess")
            .and_then(JsonValue::as_bool)
        {
            spec.insert(vs("privateAccess"), Value::Bool(private_access));
            notes.push(format!("privateAccess <- {private_access}"));
        }
        if let Some(public_access) = vpc_cfg
            .get("endpointPublicAccess")
            .and_then(JsonValue::as_bool)
        {
            spec.insert(vs("publicAccess"), Value::Bool(public_access));
            notes.push(format!("publicAccess <- {public_access}"));
        }
    }

    let encryption_enabled = cluster
        .get("encryptionConfig")
        .and_then(JsonValue::as_array)
        .map(|configs| !configs.is_empty())
        .unwrap_or(false);
    spec.insert(vs("encryptionEnabled"), Value::Bool(encryption_enabled));
    notes.push(format!("encryptionEnabled <- {encryption_enabled}"));

    if let Some(role_arn) = cluster.get("roleArn").and_then(JsonValue::as_str) {
        let control_plane_role = role_name_from_arn(role_arn).ok_or_else(|| {
            format!("unable to parse control plane role name from ARN '{role_arn}'")
        })?;
        ensure_mapping(ensure_mapping(spec, "iam"), "controlPlaneRole")
            .insert(vs("externalName"), vs(&control_plane_role));
        notes.push(format!(
            "iam.controlPlaneRole.externalName <- {control_plane_role}"
        ));
    }

    let derived_node_role = format!("{selector_name}-node");
    if aws_iam_role_exists(&derived_node_role)? {
        ensure_mapping(ensure_mapping(spec, "iam"), "nodeRole")
            .insert(vs("externalName"), vs(&derived_node_role));
        notes.push(format!(
            "iam.nodeRole.externalName <- {derived_node_role} (derived from cluster naming convention)"
        ));
    } else {
        return Err(format!(
            "unable to locate IAM node role '{}'; AutoEKS reclaim expects the composed node role to exist",
            derived_node_role
        )
        .into());
    }

    if let Some(kms_key_arn) = cluster
        .get("encryptionConfig")
        .and_then(JsonValue::as_array)
        .and_then(|configs| configs.first())
        .and_then(|cfg| cfg.get("provider"))
        .and_then(|provider| provider.get("keyArn"))
        .and_then(JsonValue::as_str)
    {
        if let Some(key_id) = kms_key_id_from_arn(kms_key_arn) {
            ensure_mapping(spec, "kms").insert(vs("externalName"), vs(&key_id));
            notes.push(format!("kms.externalName <- {key_id}"));
        }
    }

    if let Some(issuer) = cluster
        .get("identity")
        .and_then(|identity| identity.get("oidc"))
        .and_then(|oidc| oidc.get("issuer"))
        .and_then(JsonValue::as_str)
    {
        if let Some(provider_arn) = find_oidc_provider_arn(issuer)? {
            let oidc = ensure_mapping(spec, "oidc");
            oidc.insert(vs("enabled"), Value::Bool(true));
            oidc.insert(vs("externalName"), vs(&provider_arn));
            notes.push(format!("oidc.externalName <- {provider_arn}"));
        }
    }

    Ok(notes)
}

fn apply_live_aws_network(
    manifest: &mut Value,
    selector_name: &str,
    region: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut notes = Vec::new();
    let root = manifest
        .as_mapping_mut()
        .ok_or("manifest root must be a mapping")?;
    let spec = ensure_mapping(root, "spec");
    spec.insert(vs("region"), vs(region));

    let vpc_json = aws_json(&[
        "ec2",
        "describe-vpcs",
        "--region",
        region,
        "--filters",
        &format!("Name=tag:{NETWORK_TAG_KEY},Values={selector_name}"),
    ])?;
    let vpc_id = vpc_json
        .get("Vpcs")
        .and_then(JsonValue::as_array)
        .and_then(|vpcs| vpcs.first())
        .and_then(|vpc| vpc.get("VpcId"))
        .and_then(JsonValue::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| format!("no VPC found in AWS for {NETWORK_TAG_KEY}={selector_name}"))?;

    let vpc = ensure_mapping(spec, "vpc");
    vpc.insert(vs("externalName"), vs(&vpc_id));
    notes.push(format!("vpc.externalName <- {vpc_id}"));

    let igw_json = aws_json(&[
        "ec2",
        "describe-internet-gateways",
        "--region",
        region,
        "--filters",
        &format!("Name=tag:{NETWORK_TAG_KEY},Values={selector_name}"),
    ])?;
    if let Some(igw_id) = igw_json
        .get("InternetGateways")
        .and_then(JsonValue::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("InternetGatewayId"))
        .and_then(JsonValue::as_str)
    {
        ensure_mapping(spec, "internetGateway").insert(vs("externalName"), vs(igw_id));
        notes.push(format!("internetGateway.externalName <- {igw_id}"));
    }

    let eigw_json = aws_json(&[
        "ec2",
        "describe-egress-only-internet-gateways",
        "--region",
        region,
    ])?;
    if let Some(eigw_id) = eigw_json
        .get("EgressOnlyInternetGateways")
        .and_then(JsonValue::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| has_tag(item, NETWORK_TAG_KEY, selector_name))
        })
        .and_then(|item| item.get("EgressOnlyInternetGatewayId"))
        .and_then(JsonValue::as_str)
    {
        ensure_mapping(spec, "egressOnlyInternetGateway").insert(vs("externalName"), vs(eigw_id));
        notes.push(format!(
            "egressOnlyInternetGateway.externalName <- {eigw_id}"
        ));
    }

    let subnets_json = aws_json(&[
        "ec2",
        "describe-subnets",
        "--region",
        region,
        "--filters",
        &format!("Name=tag:{NETWORK_TAG_KEY},Values={selector_name}"),
    ])?;
    let subnet_items = subnets_json
        .get("Subnets")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();

    let mut subnet_names = Mapping::new();
    let mut subnet_lookup = BTreeMap::new();
    for subnet in &subnet_items {
        let Some(subnet_id) = subnet.get("SubnetId").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some(zone) = subnet.get("AvailabilityZone").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some(tier) = tag_value(subnet, SUBNET_TIER_TAG_KEY) else {
            continue;
        };
        let Some(az_suffix) = zone.chars().last() else {
            continue;
        };
        let key = format!("{tier}-{az_suffix}");
        subnet_names.insert(vs(&key), vs(subnet_id));
        subnet_lookup.insert(subnet_id.to_string(), (tier.to_string(), az_suffix));
    }
    if !subnet_names.is_empty() {
        ensure_mapping(spec, "subnetLayout")
            .insert(vs("externalNames"), Value::Mapping(subnet_names));
        notes.push("subnetLayout.externalNames <- live AWS tags".to_string());
    }

    let route_tables_json = aws_json(&[
        "ec2",
        "describe-route-tables",
        "--region",
        region,
        "--filters",
        &format!("Name=tag:{NETWORK_TAG_KEY},Values={selector_name}"),
    ])?;
    let route_table_items = route_tables_json
        .get("RouteTables")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();

    let mut route_table_names = Mapping::new();
    let mut association_names = Mapping::new();
    for route_table in &route_table_items {
        let Some(route_table_id) = route_table.get("RouteTableId").and_then(JsonValue::as_str)
        else {
            continue;
        };
        let tier = tag_value(route_table, SUBNET_TIER_TAG_KEY);
        let az_tag = tag_value(route_table, ROUTE_TABLE_AZ_TAG_KEY);
        let key = match (tier, az_tag) {
            (Some("public"), _) => Some("public".to_string()),
            (Some("private"), Some(az)) => Some(format!("private-{az}")),
            _ => None,
        };
        if let Some(key) = key {
            route_table_names.insert(vs(&key), vs(route_table_id));
        }

        if let Some(associations) = route_table
            .get("Associations")
            .and_then(JsonValue::as_array)
        {
            for assoc in associations {
                if assoc
                    .get("Main")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false)
                {
                    continue;
                }
                let Some(assoc_id) = assoc
                    .get("RouteTableAssociationId")
                    .and_then(JsonValue::as_str)
                else {
                    continue;
                };
                let Some(subnet_id) = assoc.get("SubnetId").and_then(JsonValue::as_str) else {
                    continue;
                };
                if let Some((tier, az_suffix)) = subnet_lookup.get(subnet_id) {
                    let key = format!("{tier}-{az_suffix}");
                    association_names.insert(vs(&key), vs(assoc_id));
                }
            }
        }
    }
    if !route_table_names.is_empty() || !association_names.is_empty() {
        let route_tables = ensure_mapping(spec, "routeTables");
        if !route_table_names.is_empty() {
            route_tables.insert(vs("externalNames"), Value::Mapping(route_table_names));
            notes.push("routeTables.externalNames <- live AWS tags".to_string());
        }
        if !association_names.is_empty() {
            route_tables.insert(
                vs("associationExternalNames"),
                Value::Mapping(association_names),
            );
            notes.push(
                "routeTables.associationExternalNames <- live AWS route-table associations"
                    .to_string(),
            );
        }
    }

    let nat_json = aws_json(&[
        "ec2",
        "describe-nat-gateways",
        "--region",
        region,
        "--filter",
        &format!("Name=tag:{NETWORK_TAG_KEY},Values={selector_name}"),
        "Name=state,Values=available",
    ])?;
    let nat_items = nat_json
        .get("NatGateways")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();
    let mut nat_names = Mapping::new();
    let mut eip_names = Mapping::new();
    for nat in nat_items {
        let Some(nat_id) = nat.get("NatGatewayId").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some(subnet_id) = nat.get("SubnetId").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some((_, az_suffix)) = subnet_lookup.get(subnet_id) else {
            continue;
        };
        let key = az_suffix.to_string();
        nat_names.insert(vs(&key), vs(nat_id));
        if let Some(allocation_id) = nat
            .get("NatGatewayAddresses")
            .and_then(JsonValue::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("AllocationId"))
            .and_then(JsonValue::as_str)
        {
            eip_names.insert(vs(&key), vs(allocation_id));
        }
    }
    if !nat_names.is_empty() || !eip_names.is_empty() {
        let nat = ensure_mapping(spec, "nat");
        if !nat_names.is_empty() {
            nat.insert(vs("externalNames"), Value::Mapping(nat_names));
            notes.push("nat.externalNames <- live AWS NAT gateways".to_string());
        }
        if !eip_names.is_empty() {
            nat.insert(vs("eipExternalNames"), Value::Mapping(eip_names));
            notes.push("nat.eipExternalNames <- live AWS EIP allocations".to_string());
        }
    }

    Ok(notes)
}

fn aws_json(args: &[&str]) -> Result<JsonValue, Box<dyn Error>> {
    let mut command_args = args.to_vec();
    command_args.push("--output");
    command_args.push("json");
    let output = run_cmd_output("aws", &command_args)?;
    Ok(serde_json::from_str(&output)?)
}

fn aws_iam_role_exists(role_name: &str) -> Result<bool, Box<dyn Error>> {
    let result = run_cmd_output(
        "aws",
        &[
            "iam",
            "get-role",
            "--role-name",
            role_name,
            "--output",
            "json",
        ],
    );
    match result {
        Ok(_) => Ok(true),
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("NoSuchEntity") || msg.contains("cannot be found") {
                Ok(false)
            } else {
                Err(err)
            }
        }
    }
}

pub(crate) fn role_name_from_arn(arn: &str) -> Option<String> {
    arn.rsplit('/').next().map(ToString::to_string)
}

fn kms_key_id_from_arn(arn: &str) -> Option<String> {
    arn.rsplit('/').next().map(ToString::to_string)
}

fn find_oidc_provider_arn(issuer_url: &str) -> Result<Option<String>, Box<dyn Error>> {
    let providers = aws_json(&["iam", "list-open-id-connect-providers"])?
        .get("OpenIDConnectProviderList")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();

    let normalized_issuer = issuer_url.trim_start_matches("https://");
    for provider in providers {
        let Some(arn) = provider.get("Arn").and_then(JsonValue::as_str) else {
            continue;
        };
        let details = aws_json(&[
            "iam",
            "get-open-id-connect-provider",
            "--open-id-connect-provider-arn",
            arn,
        ])?;
        if details
            .get("Url")
            .and_then(JsonValue::as_str)
            .map(|url| url == normalized_issuer)
            .unwrap_or(false)
        {
            return Ok(Some(arn.to_string()));
        }
    }

    Ok(None)
}

pub(crate) fn has_tag(value: &JsonValue, key: &str, expected: &str) -> bool {
    tag_value(value, key)
        .map(|actual| actual == expected)
        .unwrap_or(false)
}

pub(crate) fn tag_value<'a>(value: &'a JsonValue, key: &str) -> Option<&'a str> {
    value
        .get("Tags")
        .and_then(JsonValue::as_array)
        .and_then(|tags| {
            tags.iter()
                .find(|tag| tag.get("Key").and_then(JsonValue::as_str) == Some(key))
        })
        .and_then(|tag| tag.get("Value"))
        .and_then(JsonValue::as_str)
}
