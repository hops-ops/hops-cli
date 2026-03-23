mod adopt;
mod helpers;
mod manage;
mod observe;
mod orphan;
mod reconcile;

#[cfg(test)]
mod tests;

pub use helpers::types::XrArgs;
use helpers::types::XrCommand;
use std::error::Error;

pub fn run(args: &XrArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        XrCommand::Observe(observe_args) => observe::run(observe_args),
        XrCommand::Reconcile(reconcile_args) => reconcile::run(reconcile_args),
        XrCommand::Manage(manage_args) => manage::run(manage_args),
        XrCommand::Adopt(adopt_args) => adopt::run(adopt_args),
        XrCommand::Orphan(orphan_args) => orphan::run(orphan_args),
    }
}
