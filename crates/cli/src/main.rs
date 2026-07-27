use std::process::ExitCode;

use clap::Parser;
use ochcli::command::{Cli, OutputMode};
use ochcli::output::Output;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(data_dir) = &cli.data_dir {
        std::env::set_var("OCHUB_DATA_DIR", data_dir);
    }
    let locale = cli
        .lang
        .as_deref()
        .and_then(ochub_core::i18n::Locale::from_tag)
        .unwrap_or_else(|| {
            ochub_core::i18n::resolve(ochub_core::settings::get_settings().language.as_deref())
        });
    ochub_core::i18n::install(locale);

    let log_filter = match cli.verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| log_filter.into()),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    let mode = if cli.json {
        OutputMode::Json
    } else {
        cli.output
    };
    let output = Output::new_with_request_id(mode, cli.quiet, cli.trace_id.clone());
    match ochcli::run::execute(cli, &output).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            output.error(&error);
            error.exit_code()
        }
    }
}
