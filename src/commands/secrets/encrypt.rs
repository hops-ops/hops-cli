use super::{default_secret_paths, mirror_tree_with_sops};
use clap::Args;
use std::error::Error;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct EncryptArgs {
    /// Source directory containing plaintext secrets
    #[arg(long, default_value = "secrets")]
    pub source: PathBuf,

    /// Destination directory for encrypted secrets
    #[arg(long, default_value = "secrets-encrypted")]
    pub destination: PathBuf,

    /// Overwrite destination files if they already exist
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: &EncryptArgs) -> Result<(), Box<dyn Error>> {
    let (default_source, default_destination) = default_secret_paths()?;
    let source = if args.source.as_os_str().is_empty() {
        default_source
    } else {
        args.source.clone()
    };
    let destination = if args.destination.as_os_str().is_empty() {
        default_destination
    } else {
        args.destination.clone()
    };

    log::info!(
        "Encrypting secrets from {} to {}",
        source.display(),
        destination.display()
    );
    mirror_tree_with_sops(&source, &destination, "encrypt", args.force)
}
