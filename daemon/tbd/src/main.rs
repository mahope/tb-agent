use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

/// tb-agent daemon. Listens on loopback only and writes host, port and a random session token
/// to the connection file so the extension and `tb` can find it.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Address to listen on (loopback only). Port 0 picks a free port.
    #[arg(long, env = "TBD_BIND", default_value = "127.0.0.1:0")]
    bind: SocketAddr,
    /// Connection file to write. Defaults to the platform runtime directory.
    #[arg(long, env = "TB_CONNECTION_FILE")]
    connection_file: Option<PathBuf>,
    /// Seconds an operation may take before the client gets a timeout error.
    #[arg(long, default_value_t = 120)]
    request_timeout: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tbd=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let mut server = tbd::start(tbd::Config {
        bind: args.bind,
        connection_file: args.connection_file,
        request_timeout: Duration::from_secs(args.request_timeout),
    })
    .await?;
    tracing::info!(
        "listening on ws://{}:{}, connection file {}",
        server.info.host,
        server.info.port,
        server.connection_file.display()
    );
    tokio::select! {
        () = shutdown_signal() => tracing::info!("shutting down"),
        () = server.wait() => {}
    }
    server.shutdown().await;
    Ok(())
}

/// Ctrl-C everywhere, plus SIGTERM on Unix so the connection file is removed on `kill`.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
