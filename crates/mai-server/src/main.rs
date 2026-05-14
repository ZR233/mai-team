use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    mai_server::run().await
}
