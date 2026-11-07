#[tokio::main]
#[warn(clippy::large_futures)]
async fn main() -> miette::Result<()> {
    lore::run().await
}
