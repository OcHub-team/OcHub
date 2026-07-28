use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "ochubd",
    version,
    about = "Local owner daemon for the OcHub CLI"
)]
struct DaemonCli {
    /// Use an alternate OcHub data directory for this process.
    #[arg(long, env = "OCHUB_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Override the local runtime socket.
    #[arg(long, env = "OCHUB_SOCKET")]
    socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = DaemonCli::parse();
    if let Some(data_dir) = &cli.data_dir {
        // SAFETY: first statement of `main` after argument parsing. Tokio's
        // worker threads exist by now but hold no task yet, so nothing else in
        // the process can be reading the environment during the write. The
        // daemon's own readers all run after this point.
        unsafe { std::env::set_var("OCHUB_DATA_DIR", data_dir) };
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match ochcli::daemon::run_foreground(cli.socket, None, false).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ochubd: {error}");
            error.exit_code()
        }
    }
}
