pub mod config;
pub mod server;
pub mod upstream;

use config::AppConfig;
use jev_core::idempotency::IdempotencyCache;
use server::AppState;
use std::{future::Future, sync::Arc};
use tokio::net::TcpListener;
use upstream::UpstreamClient;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Serves the gateway on an already bound listener until `shutdown` resolves.
pub async fn serve(
    listener: TcpListener,
    config: AppConfig,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), BoxError> {
    let config = Arc::new(config);
    let upstream = UpstreamClient::new(&config);
    let idempotency = Arc::new(IdempotencyCache::new(
        config.idempotency_ttl,
        config.idempotency_max_entries,
    ));
    server::run(
        listener,
        AppState {
            config,
            upstream,
            idempotency,
        },
        shutdown,
    )
    .await
}

/// Loads configuration from the environment, binds `BIND_ADDR` and serves
/// until Ctrl-C (or SIGTERM on Unix).
pub async fn run_from_env() -> Result<(), BoxError> {
    let config = AppConfig::from_env()?;
    let listener = TcpListener::bind(&config.bind).await?;
    eprintln!("jev-provider listening on {}", config.bind);
    serve(listener, config, shutdown_signal()).await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => ctrl_c.await,
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}
