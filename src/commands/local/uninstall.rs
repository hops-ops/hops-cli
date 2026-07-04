use super::backend::Backend;
use std::error::Error;
use std::io::{self, Write};

pub fn run(backend: Backend) -> Result<(), Box<dyn Error>> {
    print!(
        "Uninstall {}? This will remove the binary. [y/N] ",
        backend.name()
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    if input.trim().eq_ignore_ascii_case("y") {
        backend.uninstall()?;
    } else {
        log::info!("Uninstall cancelled");
    }

    Ok(())
}
