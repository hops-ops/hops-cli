//! `hops local envs` — catalog view that toggles through the env engine.

use super::env::{self, CatalogEntry};
use super::local_state_dir;
use super::workbench::definition::ClusterOverrides;
use clap::Args;
use dialoguer::{theme::ColorfulTheme, MultiSelect};
use std::error::Error;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

#[derive(Args, Debug, Default)]
pub struct TuiArgs {
    /// Print the catalog and exit (no interactive toggle).
    #[arg(long, default_value_t = false)]
    pub once: bool,
}

pub fn run(args: &TuiArgs, overrides: ClusterOverrides<'_>) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    let entries = env::load_entries(&state_dir)?;
    if entries.is_empty() {
        println!("No catalogued Environments. Run `hops local env discover`.");
        return Ok(());
    }
    let mount_root = host_path();
    if args.once || !atty() {
        print_entries(&entries, &mount_root)?;
        return Ok(());
    }
    apply_multiselect(&entries, &mount_root, overrides)
}

fn host_path() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .and_then(|home| PathBuf::from(home).canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn worktree_label(source: &Path, mount_root: &Path) -> String {
    let checkout = source
        .ancestors()
        .nth(3)
        .filter(|_| {
            source
                .components()
                .rev()
                .nth(1)
                .is_some_and(|c| c.as_os_str() == "local")
                && source
                    .components()
                    .rev()
                    .nth(2)
                    .is_some_and(|c| c.as_os_str() == ".gitops")
        })
        .unwrap_or_else(|| source.parent().unwrap_or(source));
    let rel = checkout
        .strip_prefix(mount_root)
        .unwrap_or(checkout)
        .display()
        .to_string();
    match source.file_stem().and_then(|s| s.to_str()) {
        Some("environment") | None => rel,
        Some(stem) => format!("{rel}  {stem}"),
    }
}

fn styled_label(entry: &CatalogEntry, mount_root: &Path) -> String {
    let path = worktree_label(&entry.source, mount_root);
    if entry.enabled {
        format!("{BOLD}{path}{RESET}")
    } else {
        path
    }
}

fn print_entries(entries: &[CatalogEntry], mount_root: &Path) -> io::Result<()> {
    for entry in entries {
        writeln!(io::stdout(), "{}", styled_label(entry, mount_root))?;
    }
    Ok(())
}

fn apply_multiselect(
    entries: &[CatalogEntry],
    mount_root: &Path,
    overrides: ClusterOverrides<'_>,
) -> Result<(), Box<dyn Error>> {
    let items: Vec<String> = entries
        .iter()
        .map(|entry| styled_label(entry, mount_root))
        .collect();
    let defaults: Vec<bool> = entries.iter().map(|entry| entry.enabled).collect();
    let selected = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Environments (space toggles, enter applies)")
        .items(&items)
        .defaults(&defaults)
        .interact()?;
    let selected: std::collections::BTreeSet<usize> = selected.into_iter().collect();
    for (index, entry) in entries.iter().enumerate() {
        let want = selected.contains(&index);
        if want == entry.enabled {
            continue;
        }
        let command = if want {
            env::EnvCommands::Enable(env::NameArgs {
                name: entry.runtime_name.clone(),
            })
        } else {
            env::EnvCommands::Disable(env::NameArgs {
                name: entry.runtime_name.clone(),
            })
        };
        env::run(&env::EnvArgs { command }, ClusterOverrides { ..overrides })?;
    }
    Ok(())
}

fn atty() -> bool {
    use std::io::IsTerminal;
    io::stdin().is_terminal() && io::stdout().is_terminal()
}
