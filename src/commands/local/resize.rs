use super::start::{resize_colima, ColimaSizeArgs};
use clap::Args;
use std::error::Error;

#[derive(Args, Debug, Clone)]
pub struct ResizeArgs {
    #[command(flatten)]
    pub size: ColimaSizeArgs,
}

pub fn run(args: &ResizeArgs) -> Result<(), Box<dyn Error>> {
    resize_colima(&args.size)?;
    log::info!("Colima resize complete");
    Ok(())
}
