use clap::Args;
use std::error::Error;
use std::fs;
use std::path::Path;

const SKILL_MD: &str = include_str!("../../../skills/claude/SKILL.md");
const REF_CONFIG_INSTALL: &str = include_str!("../../../skills/claude/references/config-install.md");
const REF_XR_WORKFLOW: &str = include_str!("../../../skills/claude/references/xr-workflow.md");
const REF_SECRETS: &str = include_str!("../../../skills/claude/references/secrets.md");
const REF_LOCAL_SETUP: &str = include_str!("../../../skills/claude/references/local-setup.md");

#[derive(Args, Debug)]
pub struct ClaudeArgs {
    /// Overwrite existing files
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: &ClaudeArgs) -> Result<(), Box<dyn Error>> {
    let files: Vec<(&str, &str)> = vec![
        (".claude/skills/hops/SKILL.md", SKILL_MD),
        (
            ".claude/skills/hops/references/config-install.md",
            REF_CONFIG_INSTALL,
        ),
        (
            ".claude/skills/hops/references/xr-workflow.md",
            REF_XR_WORKFLOW,
        ),
        (".claude/skills/hops/references/secrets.md", REF_SECRETS),
        (
            ".claude/skills/hops/references/local-setup.md",
            REF_LOCAL_SETUP,
        ),
    ];

    let mut wrote = 0usize;
    let mut skipped = 0usize;

    for (path, content) in &files {
        let p = Path::new(path);
        if p.exists() && !args.force {
            log::info!("Skipping {} (exists, use --force to overwrite)", path);
            skipped += 1;
            continue;
        }
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(p, content)?;
        log::info!("Wrote {}", path);
        wrote += 1;
    }

    if wrote > 0 {
        println!(
            "Installed hops skill for Claude Code ({} files written, {} skipped)",
            wrote, skipped
        );
    } else {
        println!(
            "All files already exist ({} skipped). Use --force to overwrite.",
            skipped
        );
    }

    Ok(())
}
