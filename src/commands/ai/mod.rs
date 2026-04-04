mod claude;
mod codex;

use clap::{Args, Subcommand};
use std::error::Error;

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
