//! `kanade self-update` — explicitly update the CLI from GitHub
//! Releases (kaishin). Unlike the background notify/install modes
//! (see `crate::updater` + `crate::cli_config`), this is the
//! interactive, on-demand path and works regardless of the configured
//! mode.

use anyhow::Result;
use clap::Args;

#[derive(Args, Debug)]
pub struct SelfUpdateArgs {
    /// Skip the confirmation prompt and install immediately.
    #[arg(long)]
    pub yes: bool,

    /// Check whether a newer release exists without installing.
    #[arg(long)]
    pub check: bool,

    /// Never prompt (CI / scripts). Exits without installing unless
    /// --yes is also given.
    #[arg(long)]
    pub non_interactive: bool,
}

pub async fn execute(args: SelfUpdateArgs) -> Result<()> {
    let opts = crate::updater::options();
    let upd = kaishin::UpdateOptions::new()
        .yes(args.yes)
        .check_only(args.check)
        .non_interactive(args.non_interactive);
    kaishin::run_self_update(&opts, upd).await
}
