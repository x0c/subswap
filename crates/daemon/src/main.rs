#[tokio::main]
async fn main() -> anyhow::Result<()> {
    subswap_daemon::run().await
}
