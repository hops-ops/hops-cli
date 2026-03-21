use super::run_cmd;
use std::error::Error;

pub fn run() -> Result<(), Box<dyn Error>> {
    log::info!("Stopping Colima...");
    run_cmd("colima", &["stop"])?;
    log::info!("Colima stopped");
    Ok(())
}
