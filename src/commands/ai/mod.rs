mod claude;
mod codex;

use clap::{Args, Subcommand};
use std::error::Error;
use std::fs;
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
struct InstallSummary {
    written: usize,
    skipped: usize,
}

fn install_files(
    root: &Path,
    files: &[(&str, &str)],
    force: bool,
) -> Result<InstallSummary, Box<dyn Error>> {
    let mut summary = InstallSummary {
        written: 0,
        skipped: 0,
    };

    for (relative_path, content) in files {
        let destination = root.join(relative_path);
        if destination.exists() && !force {
            log::info!(
                "Skipping {} (exists, use --force to overwrite)",
                relative_path
            );
            summary.skipped += 1;
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&destination, content)?;
        log::info!("Wrote {}", relative_path);
        summary.written += 1;
    }

    Ok(summary)
}

fn print_summary(agent: &str, summary: &InstallSummary) {
    if summary.written > 0 {
        println!(
            "Installed Hops skills for {agent} ({} files written, {} skipped)",
            summary.written, summary.skipped
        );
    } else {
        println!(
            "All files already exist ({} skipped). Use --force to overwrite.",
            summary.skipped
        );
    }
}

#[derive(Args, Debug)]
pub struct AiArgs {
    #[command(subcommand)]
    pub command: AiCommands,
}

#[derive(Subcommand, Debug)]
pub enum AiCommands {
    /// Install Claude Code skills and configuration for hops
    Claude(claude::ClaudeArgs),
    /// Install Codex CLI agent configuration for hops
    Codex(codex::CodexArgs),
}

pub fn run(args: &AiArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        AiCommands::Claude(a) => claude::run(a),
        AiCommands::Codex(a) => codex::run(a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("hops-ai-install-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn installer_preserves_existing_files_until_forced() {
        let root = TestDir::new();
        let files = [(".agents/skills/example/SKILL.md", "bundled\n")];

        let initial = install_files(&root.0, &files, false).expect("initial install");
        assert_eq!(
            initial,
            InstallSummary {
                written: 1,
                skipped: 0
            }
        );

        let destination = root.0.join(files[0].0);
        fs::write(&destination, "user version\n").expect("customize installed skill");
        let skipped = install_files(&root.0, &files, false).expect("safe reinstall");
        assert_eq!(
            skipped,
            InstallSummary {
                written: 0,
                skipped: 1
            }
        );
        assert_eq!(
            fs::read_to_string(&destination).expect("read preserved skill"),
            "user version\n"
        );

        let forced = install_files(&root.0, &files, true).expect("forced reinstall");
        assert_eq!(
            forced,
            InstallSummary {
                written: 1,
                skipped: 0
            }
        );
        assert_eq!(
            fs::read_to_string(destination).expect("read overwritten skill"),
            "bundled\n"
        );
    }
}
