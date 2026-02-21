use super::{run_cmd, stop_kubefwd};
use std::error::Error;
use std::io::{self, Write};

pub fn run() -> Result<(), Box<dyn Error>> {
    print!("Uninstall Colima? This will remove the binary. [y/N] ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    if input.trim().eq_ignore_ascii_case("y") {
        if let Err(err) = stop_kubefwd() {
            log::warn!("Failed to stop kubefwd cleanly: {}", err);
        }

        log::info!("Uninstalling Colima...");
        run_cmd("brew", &["uninstall", "colima"])?;
        log::info!("Colima uninstalled");
    } else {
        log::info!("Uninstall cancelled");
    }

    Ok(())
}
