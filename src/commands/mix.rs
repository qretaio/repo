//! Mix command — pack repository via repomix.

use clap::Args;
use colored::Colorize;

use crate::run::{run_command, RunOptions, Task};

#[derive(Args)]
pub struct MixArgs {
    /// Arguments passed through to repomix
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

pub fn run(args: &MixArgs) -> i32 {
    let mut cmd = vec!["repomix".to_string()];
    cmd.extend_from_slice(&args.args);

    let task = Task::new("repomix", &cmd);
    let opts = RunOptions {
        verbose: true,
        ..Default::default()
    };
    let result = run_command(&task, &opts);
    if !result.success {
        eprintln!("{}", "Failed to run repomix".red());
        return result.exit_code;
    }
    0
}
