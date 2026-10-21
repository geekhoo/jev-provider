use jev_mock::{BoxError, MockOptions, DEFAULT_BIND};
use tokio::net::TcpListener;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), BoxError> {
    let opts = MockOptions::from_env()?;
    let bind = std::env::var("MOCK_BIND")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BIND.to_owned());
    let listener = TcpListener::bind(&bind).await?;
    eprintln!("jev-mock listening on {}", listener.local_addr()?);
    jev_mock::serve(listener, opts, shutdown_signal()).await
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
