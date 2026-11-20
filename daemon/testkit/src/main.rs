//! Run a fake extension against a running `tbd`, for trying out `tb` without Thunderbird.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tb_protocol::ConnectionInfo;
use tb_protocol::connection::default_path;
use tb_testkit::{FakeConfig, FakeExtension};

#[derive(Parser)]
struct Args {
    /// Connection file written by tbd.
    #[arg(long, env = "TB_CONNECTION_FILE")]
    connection_file: Option<PathBuf>,
    /// Also announce the optional experiment modules.
    #[arg(long)]
    experiment_modules: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let path = args.connection_file.unwrap_or_else(default_path);
    let info = ConnectionInfo::read(&path)?;
    let mut config = FakeConfig::default();
    if args.experiment_modules {
        config.modules.push("experiment.attachments".into());
        config.modules.push("experiment.folder-sync".into());
    }
    let _extension = FakeExtension::connect(&info, config).await?;
    eprintln!(
        "fake extension connected to ws://{}:{}",
        info.host, info.port
    );
    tokio::signal::ctrl_c().await?;
    Ok(())
}
