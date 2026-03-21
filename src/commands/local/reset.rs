use super::run_cmd;
use std::error::Error;

pub fn run() -> Result<(), Box<dyn Error>> {
    log::info!("Resetting Colima Kubernetes...");
    run_cmd("colima", &["kubernetes", "reset"])?;
    log::info!("Colima Kubernetes reset complete");
    Ok(())
}
