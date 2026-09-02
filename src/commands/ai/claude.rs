use clap::Args;
use std::error::Error;
use std::path::Path;

use super::{install_files, print_summary};

const SKILL_MD: &str = include_str!("../../../skills/claude/SKILL.md");
const REF_CONFIG_INSTALL: &str =
    include_str!("../../../skills/claude/references/config-install.md");
const REF_XR_WORKFLOW: &str = include_str!("../../../skills/claude/references/xr-workflow.md");
const REF_SECRETS: &str = include_str!("../../../skills/claude/references/secrets.md");
const REF_LOCAL_SETUP: &str = include_str!("../../../skills/claude/references/local-setup.md");
const REF_STACKS_AND_XRS: &str =
    include_str!("../../../skills/claude/references/stacks-and-xrs.md");
const REF_DEBUGGING: &str = include_str!("../../../skills/claude/references/debugging.md");
const REF_VARS: &str = include_str!("../../../skills/claude/references/vars.md");
const REF_LOCAL_SOURCE_PACKAGES: &str =
    include_str!("../../../skills/claude/references/local-source-packages.md");
const REF_LOCAL_WORKBENCH: &str =
    include_str!("../../../skills/claude/references/local-workbench.md");
const IMPORT_SKILL_MD: &str = include_str!("../../../skills/hops-import/SKILL.md");

#[derive(Args, Debug)]
pub struct ClaudeArgs {
    /// Overwrite existing files
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: &ClaudeArgs) -> Result<(), Box<dyn Error>> {
    let files = [
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
        (
            ".claude/skills/hops/references/stacks-and-xrs.md",
            REF_STACKS_AND_XRS,
        ),
        (".claude/skills/hops/references/debugging.md", REF_DEBUGGING),
        (".claude/skills/hops/references/vars.md", REF_VARS),
        (
            ".claude/skills/hops/references/local-source-packages.md",
            REF_LOCAL_SOURCE_PACKAGES,
        ),
        (
            ".claude/skills/hops/references/local-workbench.md",
            REF_LOCAL_WORKBENCH,
        ),
        (".claude/skills/hops-import/SKILL.md", IMPORT_SKILL_MD),
    ];

    let summary = install_files(Path::new("."), &files, args.force)?;
    print_summary("Claude Code", &summary);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundles_the_focused_import_skill() {
        assert!(IMPORT_SKILL_MD.contains("name: hops-import"));
        assert!(IMPORT_SKILL_MD.contains("`--dry-run`"));
    }
}
