//! Conservative ImageConfig routing preflight, before any package pin change.
//! Rules with post-rewrite credentials/runtime/verification need independent
//! validation; this first transport contract refuses them rather than guessing.
use super::registry::digest;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::error::Error;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub struct DevelopmentRewrite {
    pub source: String,
    pub destination: String,
    pub verified_digest: String,
    pub session: String,
}

impl DevelopmentRewrite {
    pub fn image_config(&self) -> Result<Value> {
        let source_digest = self.source.rsplit_once('@').map(|(_, d)| d);
        let destination_digest = self.destination.rsplit_once('@').map(|(_, d)| d);
        if self.verified_digest.len() != 71
            || !self.verified_digest.starts_with("sha256:")
            || !self.verified_digest[7..]
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            || source_digest != Some(self.verified_digest.as_str())
            || destination_digest != Some(self.verified_digest.as_str())
            || uuid::Uuid::parse_str(&self.session).is_err()
        {
            return Err(
                "development routes require full, matching verified digests and a session UUID"
                    .into(),
            );
        }
        for reference in [&self.source, &self.destination] {
            let repository = reference.rsplit_once('@').unwrap().0;
            // The transport currently generates digest-only image references.
            super::registry::validate_image_name(repository)?;
        }
        let hash = digest(self.source.as_bytes());
        Ok(json!({
            "apiVersion": "pkg.crossplane.io/v1beta1", "kind": "ImageConfig",
            "metadata": {
                "name": format!("hops-dev-{}", &hash[7..27]),
                "labels": {"hops.ops.com.ai/development-session": self.session}
            },
            "spec": {
                "matchImages": [{"type": "Prefix", "prefix": self.source}],
                "rewriteImage": {"prefix": self.destination}
            }
        }))
    }
}

struct Rule<'a> {
    name: &'a str,
    prefixes: Vec<&'a str>,
    spec: &'a Value,
}

fn rules(configs: &[Value]) -> Result<Vec<Rule<'_>>> {
    let mut names = HashSet::new();
    configs
        .iter()
        .map(|config| {
            let name = config["metadata"]["name"]
                .as_str()
                .ok_or("ImageConfig name missing")?;
            if !names.insert(name) {
                return Err("duplicate ImageConfig identity".into());
            }
            if config["kind"] != "ImageConfig"
                || config["apiVersion"] != "pkg.crossplane.io/v1beta1"
            {
                return Err("unsupported ImageConfig API".into());
            }
            let matches = config["spec"]["matchImages"]
                .as_array()
                .ok_or("ImageConfig matches missing")?;
            let mut prefixes = Vec::new();
            for matcher in matches {
                if !matcher["type"].is_null() && matcher["type"] != "Prefix" {
                    return Err("unsupported ImageConfig match type".into());
                }
                let prefix = matcher["prefix"]
                    .as_str()
                    .filter(|p| !p.is_empty())
                    .ok_or("empty ImageConfig prefix")?;
                prefixes.push(prefix);
            }
            if prefixes.is_empty() {
                return Err("ImageConfig has no match prefixes".into());
            }
            Ok(Rule {
                name,
                prefixes,
                spec: &config["spec"],
            })
        })
        .collect()
}

fn selected<'a>(rules: &'a [Rule<'a>], reference: &str) -> Result<Option<(&'a Rule<'a>, usize)>> {
    let mut best: Option<(&Rule<'_>, usize)> = None;
    let mut tied = false;
    for rule in rules {
        let Some(length) = rule
            .prefixes
            .iter()
            .filter(|p| reference.starts_with(**p))
            .map(|p| p.len())
            .max()
        else {
            continue;
        };
        match best {
            None => {
                best = Some((rule, length));
                tied = false;
            }
            Some((_, old)) if length > old => {
                best = Some((rule, length));
                tied = false;
            }
            Some((_, old)) if length == old => tied = true,
            _ => {}
        }
    }
    if tied {
        return Err(
            "ambiguous equal-longest ImageConfig prefixes; Crossplane selection would be arbitrary"
                .into(),
        );
    }
    Ok(best)
}

fn resolution(rules: &[Rule<'_>], reference: &str) -> Result<(Option<String>, String)> {
    let Some((rule, length)) = selected(rules, reference)? else {
        return Ok((None, reference.into()));
    };
    let resolved = match rule.spec["rewriteImage"]["prefix"].as_str() {
        Some(prefix) => format!("{prefix}{}", &reference[length..]),
        None => reference.into(),
    };
    Ok((Some(rule.name.into()), resolved))
}

/// Returns only the generated exact rules after checking the combined routing
/// table. Does not apply manifests or assert runtime TLS/health.
pub fn preflight(
    live: &[Value],
    development: &[DevelopmentRewrite],
    protected_refs: &[String],
) -> Result<Vec<Value>> {
    let before = rules(live)?;
    let mut combined = live.to_vec();
    let mut proposed = Vec::new();
    let mut sources = HashMap::new();
    for rewrite in development {
        if sources
            .insert(&rewrite.source, &rewrite.destination)
            .is_some()
        {
            return Err("duplicate development source identity".into());
        }
        let config = rewrite.image_config()?;
        if let Some(existing) = combined
            .iter()
            .find(|old| old["metadata"]["name"] == config["metadata"]["name"])
        {
            if existing["spec"] != config["spec"]
                || existing["metadata"]["labels"]["hops.ops.com.ai/development-session"]
                    != rewrite.session
            {
                return Err(
                    "development ImageConfig belongs to another session or has changed".into(),
                );
            }
        } else {
            combined.push(config.clone());
        }
        proposed.push(config);
    }
    let after = rules(&combined)?;
    for (rewrite, config) in development.iter().zip(&proposed) {
        let (selected_name, resolved) = resolution(&after, &rewrite.source)?;
        if selected_name.as_deref() != config["metadata"]["name"].as_str()
            || resolved != rewrite.destination
        {
            return Err(
                "development reference does not resolve through its exact verified rewrite".into(),
            );
        }
        // Rewriting is followed by independent auth/runtime/verification lookup.
        // Refuse any additional policy until its exact live identity and trust
        // have been separately verified by the activation workflow.
        if after.iter().any(|rule| {
            rule.prefixes
                .iter()
                .any(|p| rewrite.destination.starts_with(p))
        }) {
            return Err("post-rewrite ImageConfig policy requires explicit authentication, trust, and runtime validation".into());
        }
    }
    for reference in protected_refs {
        if resolution(&before, reference)? != resolution(&after, reference)? {
            return Err(
                "development ImageConfig would change a released or unrelated reference".into(),
            );
        }
    }
    Ok(proposed)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cache() -> Value {
        json!({"apiVersion":"pkg.crossplane.io/v1beta1","kind":"ImageConfig","metadata":{"name":"ghcr-cache"},
            "spec":{"matchImages":[{"prefix":"ghcr.io"}],"rewriteImage":{"prefix":"rc-internal.example.com/ghcr"}}})
    }
    fn development() -> DevelopmentRewrite {
        let hash = digest(b"immutable-manifest");
        DevelopmentRewrite {
            source: format!("ghcr.io/hops-ops/example@{hash}"),
            destination: format!(
                "registry.crossplane-dev.svc.cluster.local:5000/hops-ops/example@{hash}"
            ),
            verified_digest: hash,
            session: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
        }
    }

    #[test]
    fn exact_digest_rule_wins_while_releases_keep_broad_cache() {
        let dev = development();
        let protected = vec![
            "ghcr.io/hops-ops/example:v1.0.0".into(),
            "ghcr.io/another-org/other:v2".into(),
        ];
        let live = vec![cache()];
        let before = live.clone();
        let proposed = preflight(&live, &[dev], &protected).unwrap();
        assert_eq!(proposed.len(), 1);
        assert_eq!(live, before);
        assert_eq!(
            proposed[0]["spec"]["matchImages"][0]["prefix"],
            development().source
        );
        let mut combined = live;
        combined.extend(proposed.clone());
        assert_eq!(
            preflight(&combined, &[development()], &protected).unwrap(),
            proposed
        );
    }

    #[test]
    fn ties_other_sessions_and_post_rewrite_rules_fail_closed() {
        let mut exact = development().image_config().unwrap();
        exact["metadata"]["name"] = "unexpected-exact".into();
        assert!(preflight(&[cache(), exact], &[development()], &[]).is_err());
        let mut other = development().image_config().unwrap();
        other["metadata"]["labels"]["hops.ops.com.ai/development-session"] = "other".into();
        assert!(preflight(&[other], &[development()], &[]).is_err());
        let mut post = cache();
        post["spec"]["matchImages"][0]["prefix"] = "registry.crossplane-dev".into();
        post["spec"]["rewriteImage"] = Value::Null;
        post["spec"]["registry"] = json!({"authentication":{"pullSecretRef":{"name":"unknown"}}});
        assert!(preflight(&[post], &[development()], &[]).is_err());
    }

    #[test]
    fn wrong_digest_duplicate_identity_and_protected_capture_are_rejected() {
        let mut dev = development();
        dev.destination = format!("registry.example.com/org/pkg@{}", digest(b"different"));
        assert!(preflight(&[], &[dev], &[]).is_err());
        assert!(preflight(&[], &[development(), development()], &[]).is_err());
        assert!(preflight(&[], &[development()], &[development().source]).is_err());
        assert!(preflight(&[cache(), cache()], &[development()], &[]).is_err());
    }
}
