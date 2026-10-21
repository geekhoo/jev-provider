//! `jev_provider::serve` on an ephemeral port: health, 404 shape, shutdown.

mod common;

use common::{assert_official_error, mock_provider, request, spawn_gateway, spawn_mock};
use hyper::{Method, StatusCode};
use jev_mock::MockOptions;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn serves_health_and_404_then_stops_on_shutdown() {
    let mock = spawn_mock(MockOptions::default());
    let providers = json!({"mock": mock_provider(&mock, "responses-api", "ok")}).to_string();
    let gw = spawn_gateway(&[("PROVIDERS_JSON", &providers)]);
    let addr = gw.addr;

    let health = request(&gw, Method::GET, "/healthz", &[], None).await;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(health.json(), json!({"status": "ok"}));

    let missing = request(&gw, Method::GET, "/no-such-path", &[], None).await;
    assert_official_error(&missing, StatusCode::NOT_FOUND, Some("Not Found"));

    tokio::time::timeout(Duration::from_secs(5), gw.stop())
        .await
        .expect("serve returns after the shutdown future resolves")
        .expect("serve returns Ok");
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err(),
        "listener is closed after shutdown"
    );
}
