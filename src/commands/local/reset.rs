use super::{run_cmd, stop_kubefwd};
use std::error::Error;

pub fn run() -> Result<(), Box<dyn Error>> {
    if let Err(err) = stop_kubefwd() {
        log::warn!("Failed to stop kubefwd cleanly: {}", err);
    }

    log::info!("Resetting Colima Kubernetes...");
    run_cmd("colima", &["kubernetes", "reset"])?;
    log::info!("Colima Kubernetes reset complete");
    Ok(())
}
