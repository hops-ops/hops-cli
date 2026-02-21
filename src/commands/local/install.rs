use super::run_cmd;
use std::error::Error;

pub fn run() -> Result<(), Box<dyn Error>> {
    log::info!("Installing Colima and kubefwd via Homebrew...");
    run_cmd("brew", &["install", "colima", "kubefwd"])?;
    log::info!("Colima and kubefwd installed successfully");
    Ok(())
}
