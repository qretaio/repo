//! Run command — alias for dev (start dev servers).
//! Kept as a separate subcommand for discoverability; may grow setup logic later.

use crate::detect::Detector;
use crate::Globals;
use clap::Args;

#[derive(Args)]
pub struct RunArgs {
    /// List dev commands without running
    #[arg(long)]
    pub list: bool,
}

pub fn run(d: &Detector, g: &Globals, args: &RunArgs) -> i32 {
    crate::commands::dev::run(d, g, &crate::commands::dev::DevArgs { list: args.list })
}
