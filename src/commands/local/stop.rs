use super::backend::Backend;
use std::error::Error;

pub fn run(backend: Backend) -> Result<(), Box<dyn Error>> {
    backend.stop()
}
