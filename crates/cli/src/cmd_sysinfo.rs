//! `optctl sysinfo`: read-only summary of the hardware and Windows configuration.
//!
//! Nothing is written and the journal is not opened. The plain text leaves out the
//! computer name; the JSON form carries every field, the computer name included.

use clap::Args;
use optimizer_core::sysinfo;

#[derive(Args, Debug)]
pub(crate) struct SysinfoArgs {
    /// Print the snapshot as JSON (the raw data, the rendered sections and the text).
    #[arg(long)]
    json: bool,
}

pub(crate) fn run(args: &SysinfoArgs) -> anyhow::Result<()> {
    let snap = sysinfo::snapshot()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&snap)?);
    } else {
        print!("{}", snap.text);
    }
    Ok(())
}
