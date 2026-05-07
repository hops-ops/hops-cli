use super::{
    load_config, RepoConfig, CONFIG_FILE, DEFAULT_GITHUB_SHARED_SUBDIR, DEFAULT_GITHUB_SUBDIR,
    DEFAULT_VARS_DIR,
};
use clap::Args;
use std::error::Error;
use std::fs;
use std::path::Path;

#[derive(Args, Debug)]
pub struct InitArgs {
    /// GitHub owner/org for default repo target. Defaults to current `gh` config.
    #[arg(long)]
    pub owner: Option<String>,

    /// Local directory for variable files. Defaults to `vars`.
    #[arg(long)]
    pub dir: Option<String>,
}

pub fn run(args: &InitArgs) -> Result<(), Box<dyn Error>> {
    let mut config = load_config()?;

    // Don't clobber existing config — only fill in missing fields.
    if config.vars.dir.is_none() {
        config.vars.dir = Some(
            args.dir
                .clone()
                .unwrap_or_else(|| DEFAULT_VARS_DIR.to_string()),
        );
    }
    if config.vars.github.path.is_none() {
        config.vars.github.path = Some(DEFAULT_GITHUB_SUBDIR.to_string());
    }
    if config.vars.github.owner.is_none() {
        config.vars.github.owner = args.owner.clone();
    }
    if config.vars.github.shared.path.is_none() {
        config.vars.github.shared.path = Some(DEFAULT_GITHUB_SHARED_SUBDIR.to_string());
    }
    if config.vars.github.shared.repos.is_none() {
        config.vars.github.shared.repos = Some(Vec::new());
    }

    save_config(&config)?;

    let vars_dir = config
        .vars
        .dir
        .clone()
        .unwrap_or_else(|| DEFAULT_VARS_DIR.to_string());
    let github_subdir = config
        .vars
        .github
        .path
        .clone()
        .unwrap_or_else(|| DEFAULT_GITHUB_SUBDIR.to_string());
    let shared_subdir = config
        .vars
        .github
        .shared
        .path
        .clone()
        .unwrap_or_else(|| DEFAULT_GITHUB_SHARED_SUBDIR.to_string());

    let github_root = Path::new(&vars_dir).join(&github_subdir);
    let shared_root = github_root.join(&shared_subdir);
    fs::create_dir_all(&shared_root)?;

    log::info!(
        "Initialized vars config at {} and directories under {}",
        CONFIG_FILE,
        github_root.display()
    );
    log::info!(
        "Add per-repo files at {}/<repo>/<VAR_NAME> or shared files at {}/<VAR_NAME>",
        github_root.display(),
        shared_root.display()
    );
    Ok(())
}

fn save_config(config: &RepoConfig) -> Result<(), Box<dyn Error>> {
    // Read existing content, merge in just the `vars:` block to avoid clobbering
    // other top-level fields (e.g. `secrets:`).
    let path = Path::new(CONFIG_FILE);
    let mut existing: serde_yaml::Value = if path.exists() {
        let content = fs::read_to_string(path)?;
        serde_yaml::from_str(&content).unwrap_or(serde_yaml::Value::Null)
    } else {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    };

    if !existing.is_mapping() {
        existing = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }

    let vars_value = serde_yaml::to_value(&config.vars)?;
    if let Some(map) = existing.as_mapping_mut() {
        map.insert(serde_yaml::Value::String("vars".to_string()), vars_value);
    }

    fs::write(path, serde_yaml::to_string(&existing)?)?;
    Ok(())
}
