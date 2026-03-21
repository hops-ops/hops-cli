use super::run_cmd;
use std::error::Error;

pub fn run() -> Result<(), Box<dyn Error>> {
    log::info!("Destroying Colima VM...");
    run_cmd("colima", &["delete", "--force"])?;
    log::info!("Colima VM destroyed");
    Ok(())
}
