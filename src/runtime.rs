//! Production runtime wiring.
//!
//! This module turns a validated [`AppConfig`] into a running service: it opens
//! storage, builds the HTTP app and worker integrations, and drives the HTTP
//! server alongside the Transcription, Routing, and Delivery worker loops until
//! shutdown. The binary edge (`src/main.rs`) only resolves the config path and
//! reports errors; everything testable lives here.
//!
//! Secrets are read from the environment when the integrations are constructed
//! ([`SonioxTranscriber::from_config`], [`WebhookSinkClient::from_config`]), so a
//! missing or blank secret is a startup error rather than a first-request
//! failure.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::blob::BlobStore;
use crate::config::{AppConfig, ConfigError};
use crate::delivery::{WebhookBuildError, WebhookSinkClient};
use crate::http::{self, AppState};
use crate::monitor::auth::AuthService;
use crate::monitor::{MonitorState, TokioSleeper};
use crate::routing::ConfigRouter;
use crate::seam::{Clock, IdGenerator, SystemClock, UlidGenerator};
use crate::soniox::{SonioxBuildError, SonioxTranscriber};
use crate::storage::{SqliteStore, StorageError};
use crate::workers::{WorkerContext, delivery, routing, run_worker_loop, transcription};

/// Environment variable that names the config file when no argument is given.
pub const CONFIG_PATH_ENV: &str = "TELEGRAPH_CENTER_CONFIG";

/// SQLite database file name inside the data directory.
const DATABASE_FILE: &str = "telegraph.db";

/// How long an idle worker loop waits before polling the queue again.
const WORKER_IDLE_INTERVAL: Duration = Duration::from_secs(5);

/// After a shutdown signal, how long to let the server drain and worker loops
/// finish their current tick before exiting anyway. Anything still in flight is
/// reclaimed at the next startup by [`SqliteStore::recover_abandoned_attempts`],
/// so this stays well under systemd's default stop timeout.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);

/// A reason the service could not start or run.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// Neither a config-path argument nor [`CONFIG_PATH_ENV`] was provided.
    #[error(
        "no config path: pass it as the first argument or set the {CONFIG_PATH_ENV} environment variable"
    )]
    MissingConfigPath,
    /// The config file could not be read.
    #[error("failed to read config file {path}: {source}")]
    ReadConfig {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// The config file failed to parse or validate.
    #[error("invalid config: {0}")]
    Config(#[from] ConfigError),
    /// Storage could not be opened or migrated.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    /// The blob store directories could not be prepared.
    #[error("blob storage error: {0}")]
    Blob(io::Error),
    /// The Soniox integration could not be constructed (missing/blank secret).
    #[error("soniox setup error: {0}")]
    Soniox(#[from] SonioxBuildError),
    /// A Webhook Sink could not be constructed (missing/blank secret).
    #[error("webhook setup error: {0}")]
    Webhook(#[from] WebhookBuildError),
    /// The listen address could not be bound.
    #[error("failed to bind {listen}: {source}")]
    Bind {
        /// The address that could not be bound.
        listen: String,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// The HTTP server failed while serving.
    #[error("server error: {0}")]
    Serve(io::Error),
}

/// Resolve the config path from the optional first CLI argument, falling back to
/// the [`CONFIG_PATH_ENV`] environment variable. The argument takes precedence.
pub fn resolve_config_path(
    arg: Option<&str>,
    env: Option<String>,
) -> Result<PathBuf, RuntimeError> {
    arg.map(PathBuf::from)
        .or_else(|| env.map(PathBuf::from))
        .ok_or(RuntimeError::MissingConfigPath)
}

/// Read and validate the config file at `path`.
pub fn load_config(path: &Path) -> Result<AppConfig, RuntimeError> {
    let raw = std::fs::read_to_string(path).map_err(|source| RuntimeError::ReadConfig {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(AppConfig::from_toml_str(&raw)?)
}

/// Build the API and monitor states that share storage but expose distinct
/// surfaces. Pure wiring: no environment is read here (the Operator password
/// hash is read lazily on login), so this is straightforward to test.
pub fn build_states(
    config: Arc<AppConfig>,
    store: SqliteStore,
    blobs: Arc<BlobStore>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
) -> (AppState, MonitorState) {
    let api = AppState {
        config: config.clone(),
        store: store.clone(),
        blobs,
        clock: clock.clone(),
        ids: ids.clone(),
    };
    let monitor = MonitorState {
        auth: Arc::new(AuthService::from_config(&config.operator)),
        config,
        store,
        clock,
        ids,
        sleeper: Arc::new(TokioSleeper),
    };
    (api, monitor)
}

/// Open storage, build the app and worker integrations, and serve until a
/// shutdown signal (`SIGINT`/`Ctrl-C` or `SIGTERM`). Returns an error if startup
/// fails or if any worker loop or the HTTP server fails while running, so the
/// process exits non-zero rather than running degraded.
pub async fn run(config: AppConfig) -> Result<(), RuntimeError> {
    let config = Arc::new(config);

    let store = SqliteStore::connect(config.data.dir.join(DATABASE_FILE)).await?;
    let blobs = Arc::new(
        BlobStore::new(config.data.dir.clone())
            .await
            .map_err(RuntimeError::Blob)?,
    );
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids: Arc<dyn IdGenerator> = Arc::new(UlidGenerator);

    // Reclaim work a previous process left in flight before starting workers.
    let recovery = store.recover_abandoned_attempts(clock.now()).await?;
    if recovery.has_changes() {
        println!(
            "telegraph-center recovered abandoned work: {} transcription attempt(s), {} reset recording(s), {} delivery attempt(s)",
            recovery.transcription_attempts,
            recovery.reverted_recordings,
            recovery.delivery_attempts,
        );
    }

    // Construct integrations first so missing/blank secrets fail before binding.
    let http_client = reqwest::Client::new();
    let transcriber = SonioxTranscriber::from_config(&config.soniox, http_client.clone())?;
    let router = ConfigRouter::from_config(&config);
    let sink_client = WebhookSinkClient::from_config(&config.sinks, http_client)?;

    let (api_state, monitor_state) = build_states(
        config.clone(),
        store.clone(),
        blobs.clone(),
        clock.clone(),
        ids.clone(),
    );
    let app = http::app(api_state, monitor_state);

    let listener = tokio::net::TcpListener::bind(&config.server.listen)
        .await
        .map_err(|source| RuntimeError::Bind {
            listen: config.server.listen.clone(),
            source,
        })?;
    println!("telegraph-center listening on {}", config.server.listen);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Worker loops share one context borrowing storage, blobs, and the id
    // generator, all kept alive for the duration of `run`.
    let ctx = WorkerContext {
        store: &store,
        blobs: blobs.as_ref(),
        ids: ids.as_ref(),
    };
    let clock_ref = clock.as_ref();

    let transcription_loop = run_worker_loop(
        || transcription::tick_once(&ctx, &transcriber, clock_ref),
        WORKER_IDLE_INTERVAL,
        shutdown_rx.clone(),
    );
    let routing_loop = run_worker_loop(
        || routing::tick_once(&ctx, &router, clock_ref),
        WORKER_IDLE_INTERVAL,
        shutdown_rx.clone(),
    );
    let delivery_loop = run_worker_loop(
        || delivery::tick_once(&ctx, &sink_client, clock_ref),
        WORKER_IDLE_INTERVAL,
        shutdown_rx.clone(),
    );

    let server = async {
        let mut rx = shutdown_rx.clone();
        axum::serve(listener, app.into_make_service())
            .with_graceful_shutdown(async move {
                let _ = rx.wait_for(|stop| *stop).await;
            })
            .await
            .map_err(RuntimeError::Serve)
    };

    let transcription = async { transcription_loop.await.map_err(RuntimeError::from) };
    let routing = async { routing_loop.await.map_err(RuntimeError::from) };
    let delivery = async { delivery_loop.await.map_err(RuntimeError::from) };

    // The server and the three worker loops run until one fails or shutdown is
    // requested. A failure exits non-zero; a signal flips the shutdown flag, then
    // the same flag lets each component finish gracefully within `SHUTDOWN_GRACE`.
    let services = async {
        tokio::try_join!(server, transcription, routing, delivery)?;
        Ok::<(), RuntimeError>(())
    };
    tokio::pin!(services);

    tokio::select! {
        result = &mut services => result,
        _ = shutdown_signal() => {
            println!("telegraph-center shutting down");
            let _ = shutdown_tx.send(true);
            match tokio::time::timeout(SHUTDOWN_GRACE, services).await {
                Ok(result) => result,
                Err(_) => {
                    println!("telegraph-center shutdown grace elapsed; exiting");
                    Ok(())
                }
            }
        }
    }
}

/// Resolve when the process is asked to stop: `SIGINT` (`Ctrl-C`) or, on Unix,
/// `SIGTERM` (how systemd stops the service). Whichever arrives first wins.
async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            // If the handler cannot be installed, never resolve on this arm.
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_prefers_argument() {
        let path =
            resolve_config_path(Some("/from/arg.toml"), Some("/from/env.toml".into())).unwrap();
        assert_eq!(path, PathBuf::from("/from/arg.toml"));
    }

    #[test]
    fn config_path_falls_back_to_env() {
        let path = resolve_config_path(None, Some("/from/env.toml".into())).unwrap();
        assert_eq!(path, PathBuf::from("/from/env.toml"));
    }

    #[test]
    fn config_path_missing_is_an_error() {
        assert!(matches!(
            resolve_config_path(None, None),
            Err(RuntimeError::MissingConfigPath)
        ));
    }

    #[test]
    fn load_config_reports_a_missing_file() {
        let err = load_config(Path::new("/nonexistent/telegraph/config.toml")).unwrap_err();
        assert!(matches!(err, RuntimeError::ReadConfig { .. }));
    }
}
