use super::run_cmd;
use std::error::Error;

pub fn run() -> Result<(), Box<dyn Error>> {
    log::info!("Installing Colima via Homebrew...");
    run_cmd("brew", &["install", "colima"])?;
    log::info!("Colima installed successfully");
    Ok(())
}
