use super::{
    collect_dir_vars, configured_github_settings, configured_vars_dir, require_command,
    run_command_output_string,
};
use clap::Args;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Source path. Defaults to <vars.dir>/<github.path>.
    #[arg(long)]
    pub dir: Option<String>,

    /// Repos to query for remote state. Defaults to configured shared.repos.
    #[arg(long = "repo")]
    pub repos: Vec<String>,

    /// Override configured GitHub owner.
    #[arg(long)]
    pub owner: Option<String>,
}

#[derive(Deserialize)]
struct GhVariable {
    name: String,
}

pub fn run(args: &ListArgs) -> Result<(), Box<dyn Error>> {
    require_command("gh")?;

    let github_settings = configured_github_settings()?;
    let vars_dir = configured_vars_dir()?;
    let default_source = vars_dir.join(&github_settings.path);
    let source_root = args
        .dir
        .clone()
        .map(PathBuf::from)
        .unwrap_or(default_source);

    let owner = args
        .owner
        .clone()
        .or(github_settings.owner.clone())
        .ok_or("GitHub owner is not configured. Set vars.github.owner or pass --owner.")?;

    let repos: Vec<String> = if !args.repos.is_empty() {
        args.repos.clone()
    } else {
        github_settings.shared_repos.clone()
    };
    if repos.is_empty() {
        return Err("No repos to list. Pass --repo or configure vars.github.shared.repos.".into());
    }

    let shared_root = source_root.join(&github_settings.shared_path);
    let shared_vars: BTreeSet<String> = collect_dir_vars(&shared_root)?
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();

    for repo in repos {
        let repo_dir = source_root.join(&repo);
        let mut local: BTreeSet<String> = shared_vars.clone();
        if repo_dir.is_dir() {
            for (name, _, _) in collect_dir_vars(&repo_dir)? {
                local.insert(name);
            }
        }

        let remote = fetch_remote_var_names(&owner, &repo).unwrap_or_else(|err| {
            log::warn!("failed to list remote vars for {}/{}: {}", owner, repo, err);
            BTreeSet::new()
        });

        let mut all: BTreeSet<&String> = local.iter().collect();
        for r in &remote {
            all.insert(r);
        }

        println!("\n{}/{}", owner, repo);
        if all.is_empty() {
            println!("  (no variables)");
            continue;
        }
        let mut rows: BTreeMap<String, &str> = BTreeMap::new();
        for name in all {
            let l = local.contains(name);
            let r = remote.contains(name);
            let status = match (l, r) {
                (true, true) => "ok",
                (true, false) => "missing remote",
                (false, true) => "remote-only (orphan)",
                (false, false) => "-",
            };
            rows.insert(name.clone(), status);
        }
        for (name, status) in rows {
            println!("  {:32}  {}", name, status);
        }
    }
    Ok(())
}

fn fetch_remote_var_names(owner: &str, repo: &str) -> Result<BTreeSet<String>, Box<dyn Error>> {
    let json = run_command_output_string(
        "gh",
        &[
            "variable",
            "list",
            "--repo",
            &format!("{}/{}", owner, repo),
            "--json",
            "name",
        ],
    )?;
    let parsed: Vec<GhVariable> = serde_json::from_str(&json)?;
    Ok(parsed.into_iter().map(|v| v.name).collect())
}
