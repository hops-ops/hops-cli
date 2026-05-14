use super::{default_secret_paths, mirror_tree_with_sops};
use clap::Args;
use std::error::Error;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct DecryptArgs {
    /// Source directory containing encrypted secrets
    #[arg(long, default_value = "secrets-encrypted")]
    pub source: PathBuf,

    /// Destination directory for decrypted secrets
    #[arg(long, default_value = "secrets")]
    pub destination: PathBuf,

    /// Scope decryption to a single file or subdirectory within `source`.
    /// Useful for partial inspection without overwriting all local
    /// plaintexts. Path may be absolute or relative to cwd; must resolve
    /// inside `source`.
    #[arg(long)]
    pub secret_path: Option<PathBuf>,

    /// Overwrite destination files if they already exist
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: &DecryptArgs) -> Result<(), Box<dyn Error>> {
    let (default_destination, default_source) = default_secret_paths()?;
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

    let start_path = args.secret_path.as_deref();
    if let Some(scope) = start_path {
        log::info!(
            "Decrypting {} → {} (scope: {})",
            source.display(),
            destination.display(),
            scope.display()
        );
    } else {
        log::info!(
            "Decrypting {} → {}",
            source.display(),
            destination.display()
        );
    }
    mirror_tree_with_sops(&source, &destination, "decrypt", args.force, start_path)
}
