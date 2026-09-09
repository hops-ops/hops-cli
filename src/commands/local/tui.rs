//! `hops local tui` — catalog view that toggles through the env engine.

use super::env::{self, CatalogEntry};
use super::local_state_dir;
use super::workbench::definition::ClusterOverrides;
use clap::Args;
use dialoguer::{theme::ColorfulTheme, MultiSelect};
use std::error::Error;
use std::io::{self, Write};

#[derive(Args, Debug, Default)]
pub struct TuiArgs {
    /// Print the catalog and exit (no interactive toggle).
    #[arg(long, default_value_t = false)]
    pub once: bool,
}

pub fn run(args: &TuiArgs, overrides: ClusterOverrides<'_>) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    env::print_status_card(&state_dir)?;
    let entries = env::load_entries(&state_dir)?;
    if entries.is_empty() {
        println!("No catalogued Environments. Run `hops local env discover`.");
        return Ok(());
    }
    if args.once || !atty() {
        print_entries(&entries)?;
        return Ok(());
    }
    apply_multiselect(&entries, overrides)
}

fn print_entries(entries: &[CatalogEntry]) -> io::Result<()> {
    writeln!(io::stdout(), "Platform / Environments:")?;
    for entry in entries {
        let flag = if entry.enabled { "x" } else { " " };
        writeln!(
            io::stdout(),
            "  [{flag}] {}  {}",
            entry.name,
            entry.source.display()
        )?;
    }
    Ok(())
}

fn apply_multiselect(
    entries: &[CatalogEntry],
    overrides: ClusterOverrides<'_>,
) -> Result<(), Box<dyn Error>> {
    let items: Vec<String> = entries
        .iter()
        .map(|entry| format!("{} ({})", entry.name, entry.source.display()))
        .collect();
    let defaults: Vec<bool> = entries.iter().map(|entry| entry.enabled).collect();
    let selected = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Enabled Environments (space toggles, enter applies)")
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
                name: entry.name.clone(),
            })
        } else {
            env::EnvCommands::Disable(env::NameArgs {
                name: entry.name.clone(),
            })
        };
        env::run(
            &env::EnvArgs { command },
            ClusterOverrides { ..overrides },
        )?;
    }
    Ok(())
}

fn atty() -> bool {
    use std::io::IsTerminal;
    io::stdin().is_terminal() && io::stdout().is_terminal()
}
