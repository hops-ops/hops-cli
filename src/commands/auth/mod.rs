//! `hops auth` subcommands.
//!
//! Right now the only command is `bootstrap`, which generates the
//! durable AWS Secrets Manager values an `AuthStack` install needs
//! before its in-cluster reconciler can do anything (masterkey,
//! admin-password). The CLI deliberately stops there — applying the
//! `AuthStack` manifest stays a separate `kubectl apply` so the
//! GitOps boundary isn't crossed by automation.

mod bootstrap;

use clap::{Args, Subcommand};
use std::error::Error;

#[derive(Args, Debug)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommands,
}

#[derive(Subcommand, Debug)]
pub enum AuthCommands {
    /// Generate the durable AWS Secrets Manager values an AuthStack install
    /// requires (masterkey + admin-password). Does NOT apply any Kubernetes
    /// manifests — run `kubectl apply -f local/` (or your GitOps flow) after.
    Bootstrap(bootstrap::BootstrapArgs),
}

pub fn run(args: &AuthArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        AuthCommands::Bootstrap(a) => bootstrap::run(a),
    }
}
