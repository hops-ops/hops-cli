//! Additive local read contracts. Only allowlisted fields cross this boundary.
use super::workbench::{machine, registry};
use serde_json::{json, Value};
use std::{error::Error, path::Path, process::Command};

const MAX_RECORDS: usize = 256;
const MAX_BYTES: usize = 1_048_576;

pub(super) fn bounded(
    mut envelope: Value,
    key: &str,
    mut records: Vec<Value>,
) -> Result<String, Box<dyn Error>> {
    let total = records.len();
    records.truncate(MAX_RECORDS);
    envelope[key] = Value::Array(records);
    loop {
        let count = envelope[key].as_array().unwrap().len();
        envelope["truncation"] = json!({"records_omitted": total - count});
        let output = serde_json::to_string(&envelope)? + "\n";
        if output.len() <= MAX_BYTES {
            return Ok(output);
        }
        if envelope[key].as_array_mut().unwrap().pop().is_none() {
            return Err("local JSON envelope exceeds output limit".into());
        }
    }
}

pub(super) fn print_bounded(
    envelope: Value,
    key: &str,
    records: Vec<Value>,
) -> Result<(), Box<dyn Error>> {
    use std::io::Write;
    std::io::stdout()
        .lock()
        .write_all(bounded(envelope, key, records)?.as_bytes())?;
    Ok(())
}

// Explicit context bypasses ambient HOPS_KUBE_CONTEXT and kubectl's current context.
fn query(context: &str, args: &[&str]) -> Result<Value, &'static str> {
    let output = Command::new("kubectl")
        .args(["--context", context, "--request-timeout=5s"])
        .args(args)
        .output()
        .map_err(|_| "command_unavailable")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let state = if stderr.contains("Error from server (Forbidden):") {
            "forbidden"
        } else if stderr.contains("the server doesn't have a resource type") {
            "unavailable"
        } else {
            "unreachable"
        };
        return Err(state);
    }
    if output.stdout.len() > MAX_BYTES {
        return Err("truncated");
    }
    serde_json::from_slice(&output.stdout).map_err(|_| "invalid_json")
}

fn ready(item: &Value, types: &[&str]) -> bool {
    types.iter().all(|ty| {
        item.pointer("/status/conditions")
            .and_then(Value::as_array)
            .is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|c| c["type"] == *ty && c["status"] == "True")
            })
    })
}

fn health(context: &str, resource: &str, namespace: Option<&str>, types: &[&str]) -> Value {
    let mut args = vec!["get", resource, "-o", "json"];
    if let Some(ns) = namespace {
        if ns.is_empty() {
            args.push("-A");
        } else {
            args.extend(["-n", ns]);
        }
    }
    match query(context, &args) {
        Err(state) => json!({"state": state}),
        Ok(value) => match value.get("items").and_then(Value::as_array) {
            None => json!({"state": "invalid_json"}),
            Some(items) => {
                let count = items.iter().filter(|i| ready(i, types)).count();
                json!({"state": if items.is_empty() {"not_found"} else if count == items.len() {"ready"} else {"degraded"},
                    "total": items.len(), "ready": count})
            }
        },
    }
}

pub(super) fn status(
    state_dir: &Path,
    name: Option<&str>,
    check: bool,
) -> Result<(), Box<dyn Error>> {
    let machine = machine::load(state_dir).map_err(|_| "invalid local cluster record")?;
    let mut workspaces = strict_workspaces(state_dir)?;
    if let Some(name) = name {
        workspaces.retain(|w| w.name == name);
        if workspaces.is_empty() {
            return Err(format!("Workspace `{name}` is not registered.").into());
        }
    }
    workspaces.sort_by(|a, b| {
        a.namespace.cmp(&b.namespace).then(
            Path::new(&a.env_path)
                .canonicalize()
                .unwrap_or_else(|_| a.env_path.clone().into())
                .cmp(
                    &Path::new(&b.env_path)
                        .canonicalize()
                        .unwrap_or_else(|_| b.env_path.clone().into()),
                ),
        )
    });
    let mut cluster = json!({"state": "absent_record"});
    let mut all_ready = false;
    if let Some(ref m) = machine {
        cluster = json!({"name": m.name, "kube_context": m.kube_context, "source": m.source,
            "host_path": m.host_path, "local_domain": m.local_domain, "state": "missing_context"});
        if !m.kube_context.trim().is_empty() {
            // Context inventory distinguishes missing configuration from a down cluster.
            let contexts = Command::new("kubectl")
                .args(["config", "get-contexts", "-o", "name"])
                .output();
            match contexts {
                Err(_) => cluster["state"] = json!("command_unavailable"),
                Ok(out) if !out.status.success() => cluster["state"] = json!("context_unavailable"),
                Ok(out)
                    if !String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .any(|c| c.trim() == m.kube_context) => {}
                Ok(_) => {
                    cluster["nodes"] = health(&m.kube_context, "nodes", None, &["Ready"]);
                    cluster["configurations"] = health(
                        &m.kube_context,
                        "configurations.pkg.crossplane.io",
                        None,
                        &["Healthy", "Installed"],
                    );
                    cluster["providers"] = health(
                        &m.kube_context,
                        "providers.pkg.crossplane.io",
                        None,
                        &["Healthy", "Installed"],
                    );
                    cluster["authstacks"] =
                        health(&m.kube_context, "authstack", Some(""), &["Ready"]);
                    let nodes = cluster["nodes"]["state"].as_str().unwrap_or("unknown");
                    let state = if nodes == "ready"
                        && cluster["configurations"]["state"] == "ready"
                        && cluster["providers"]["state"] == "ready"
                        && cluster["authstacks"]["state"] == "ready"
                    {
                        "ready"
                    } else if nodes == "ready" {
                        "degraded"
                    } else {
                        nodes
                    };
                    all_ready = state == "ready";
                    cluster["state"] = json!(state);
                }
            }
        }
    }
    let records = workspaces.iter().map(|w| {
        let state = match (&machine, w.kube_context.as_deref()) {
            (_, None | Some("")) => json!({"state": "missing_context"}),
            (Some(m), Some(ctx)) if ctx == m.kube_context && w.cluster_name.as_deref() == Some(&m.name) => {
                if cluster["state"] == "ready" || cluster["state"] == "degraded" {
                    health(ctx, "pods", Some(&w.namespace), &["Ready"])
                } else { json!({"state": cluster["state"]}) }
            }
            _ => json!({"state": "context_mismatch"}),
        };
        if state["state"] != "ready" { all_ready = false; }
        json!({"name": w.name, "namespace": w.namespace, "env_path": w.env_path,
            "project_root": w.project_root, "cluster_name": w.cluster_name, "kube_context": w.kube_context,
            "health": state})
    }).collect();
    print_bounded(
        json!({"schema_version": 1, "cluster": cluster}),
        "workspaces",
        records,
    )?;
    if check && !all_ready {
        return Err("local cluster or workspaces are not ready".into());
    }
    Ok(())
}

// JSON consumers must not mistake skipped corrupt records for a complete registry.
fn strict_workspaces(state_dir: &Path) -> Result<Vec<registry::WorkspaceRecord>, Box<dyn Error>> {
    let dir = state_dir.join(registry::ENVS_SUBDIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|_| "local workspace registry unavailable")? {
        let path = entry
            .map_err(|_| "local workspace registry unavailable")?
            .path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read(path).map_err(|_| "local workspace record unavailable")?;
        records.push(serde_json::from_slice(&raw).map_err(|_| "invalid local workspace record")?);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_records_bytes_and_fixed_envelope() {
        let value: Value = serde_json::from_str(
            &bounded(
                json!({"schema_version":1}),
                "entries",
                vec![json!({"id":"x"}); 300],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(value["entries"].as_array().unwrap().len(), 256);
        assert_eq!(value["truncation"]["records_omitted"], 44);
        let output = bounded(
            json!({"schema_version":1}),
            "entries",
            vec![json!({"text":"é".repeat(300_000)}); 3],
        )
        .unwrap();
        assert!(output.len() <= MAX_BYTES);
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["truncation"]["records_omitted"], 2);
        assert!(bounded(json!({"fixed":"x".repeat(MAX_BYTES)}), "entries", vec![]).is_err());
    }
}
