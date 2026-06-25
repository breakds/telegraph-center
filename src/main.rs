//! Telegraph Center binary entry point.
//!
//! The binary edge stays thin: it resolves the config path from the first
//! argument or the [`runtime::CONFIG_PATH_ENV`] environment variable, loads the
//! config, and hands off to [`runtime::run`]. All wiring and serving logic lives
//! in [`runtime`] so it is testable without `std::env` or `std::process`.

use std::process::ExitCode;

use telegraph_center::runtime::{self, RuntimeError};

#[tokio::main]
async fn main() -> ExitCode {
    match start().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("telegraph-center: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn start() -> Result<(), RuntimeError> {
    let args: Vec<String> = std::env::args().collect();
    let path = runtime::resolve_config_path(
        args.get(1).map(String::as_str),
        std::env::var(runtime::CONFIG_PATH_ENV).ok(),
    )?;
    let config = runtime::load_config(&path)?;
    runtime::run(config).await
}
