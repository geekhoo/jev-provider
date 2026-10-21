#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), jev_provider::BoxError> {
    jev_provider::run_from_env().await
}
