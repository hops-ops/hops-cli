use super::{
    collect_dir_vars, configured_github_settings, configured_vars_dir, require_command,
    run_command_output_string, GithubVarsRuntimeConfig,
};
use clap::{Args, Subcommand};
use dialoguer::Confirm;
use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Args, Debug)]
pub struct SyncArgs {
    #[command(subcommand)]
    pub target: SyncTarget,
}

#[derive(Subcommand, Debug)]
pub enum SyncTarget {
    /// Sync vars to GitHub repository variables
    Github(GithubSyncArgs),
}

#[derive(Args, Debug)]
pub struct GithubSyncArgs {
    /// Source path. Defaults to <vars.dir>/<github.path>.
    #[arg(long)]
    pub dir: Option<String>,

    /// Override configured repositories. Repeat to target multiple.
    #[arg(long = "repo")]
    pub repos: Vec<String>,

    /// Override configured GitHub owner.
    #[arg(long)]
    pub owner: Option<String>,

    /// Skip confirmation prompts.
    #[arg(short, long)]
    pub yes: bool,
}

pub fn run(args: &SyncArgs) -> Result<(), Box<dyn Error>> {
    match &args.target {
        SyncTarget::Github(github_args) => run_github(github_args),
    }
}

fn run_github(args: &GithubSyncArgs) -> Result<(), Box<dyn Error>> {
    require_command("gh")?;
    ensure_gh_auth()?;

    let github_settings = configured_github_settings()?;
    let vars_dir = configured_vars_dir()?;
    let default_source = vars_dir.join(&github_settings.path);
    let source_root = args
        .dir
        .clone()
        .map(PathBuf::from)
        .unwrap_or(default_source);
    fs::metadata(&source_root)?;

    let owner = resolve_github_owner(args.owner.as_deref(), github_settings.owner.as_deref())?;
    let repos = resolve_github_repos(&source_root, &github_settings, &args.repos)?;
    if repos.is_empty() {
        return Err("No GitHub repos configured. Add vars.github.shared.repos, pass --repo, or create repo directories under the GitHub vars path.".into());
    }

    let shared_root = source_root.join(&github_settings.shared_path);
    let shared_vars = collect_dir_vars(&shared_root)?;

    let mut synced = 0usize;
    for repo in repos {
        sync_github_repo(
            &owner,
            &repo,
            &source_root,
            &shared_root,
            &shared_vars,
            args.yes,
            &mut synced,
        )?;
    }

    log::info!("GitHub sync complete - {} variables processed", synced);
    Ok(())
}

fn ensure_gh_auth() -> Result<(), Box<dyn Error>> {
    let token = run_command_output_string("gh", &["auth", "token"]).map_err(|err| {
        format!(
            "failed to read GitHub auth token: {}\nRun `gh auth login` first.",
            err
        )
    })?;
    if token.trim().is_empty() {
        return Err("`gh auth token` returned an empty token. Run `gh auth login`.".into());
    }
    Ok(())
}

fn resolve_github_owner(
    cli_owner: Option<&str>,
    configured_owner: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let env_owner = env::var("GH_OWNER").ok();
    let env_github_owner = env::var("GITHUB_OWNER").ok();
    let owner = [
        cli_owner,
        configured_owner,
        env_owner.as_deref(),
        env_github_owner.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|value| !value.is_empty())
    .map(str::to_string);

    match owner {
        Some(owner) => Ok(owner),
        None => Err("GitHub owner is not configured. Set vars.github.owner, pass --owner, or set GH_OWNER/GITHUB_OWNER.".into()),
    }
}

fn resolve_github_repos(
    source_root: &Path,
    settings: &GithubVarsRuntimeConfig,
    cli_repos: &[String],
) -> Result<Vec<String>, Box<dyn Error>> {
    if !cli_repos.is_empty() {
        return Ok(cli_repos.to_vec());
    }
    if !settings.shared_repos.is_empty() {
        return Ok(settings.shared_repos.clone());
    }

    let mut repos = Vec::new();
    for entry in fs::read_dir(source_root)? {
        let path = entry?.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
                if name == settings.shared_path {
                    continue;
                }
                repos.push(name.to_string());
            }
        }
    }
    repos.sort();
    repos.dedup();
    Ok(repos)
}

fn sync_github_repo(
    owner: &str,
    repo: &str,
    source_root: &Path,
    shared_root: &Path,
    shared_vars: &[(String, String, String)],
    yes: bool,
    synced: &mut usize,
) -> Result<(), Box<dyn Error>> {
    let repo_dir = source_root.join(repo);
    // Per-repo overrides shared (repo-specific value wins on name collision).
    let mut merged = BTreeMap::<String, (String, String)>::new();

    for (name, value, source_label) in shared_vars {
        merged.insert(name.clone(), (value.clone(), source_label.clone()));
    }

    if repo_dir.is_dir() {
        for (name, value, source_label) in collect_dir_vars(&repo_dir)? {
            merged.insert(name, (value, source_label));
        }
    } else if shared_root.exists() && !shared_vars.is_empty() {
        log::info!(
            "Applying only shared GitHub vars to '{}/{}' (no repo-specific dir at '{}').",
            owner,
            repo,
            repo_dir.display()
        );
    } else {
        log::warn!(
            "No var source found for GitHub repo '{}'. Expected directory '{}'.",
            repo,
            repo_dir.display()
        );
    }

    if merged.is_empty() {
        log::info!("No variables to sync for '{}/{}'", owner, repo);
        return Ok(());
    }

    for (name, (value, source_label)) in merged {
        set_github_variable(owner, repo, &name, &value, &source_label, yes)?;
        *synced += 1;
    }
    Ok(())
}

fn set_github_variable(
    owner: &str,
    repo: &str,
    var_name: &str,
    var_value: &str,
    source_label: &str,
    yes: bool,
) -> Result<(), Box<dyn Error>> {
    if !yes
        && !Confirm::new()
            .with_prompt(format!(
                "Set GitHub variable '{}' in '{}/{}' from '{}'?",
                var_name, owner, repo, source_label
            ))
            .default(true)
            .interact()?
    {
        return Ok(());
    }

    // `gh variable set` reads the value from stdin when --body is omitted.
    let mut child = Command::new("gh")
        .args([
            "variable",
            "set",
            var_name,
            "--repo",
            &format!("{}/{}", owner, repo),
        ])
        .stdin(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(var_value.as_bytes())?;
    } else {
        return Err("failed to open stdin for `gh variable set`".into());
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(format!("gh variable set exited with {}", status).into());
    }
    log::info!("Set GitHub variable '{}' in '{}/{}'", var_name, owner, repo);
    Ok(())
}
