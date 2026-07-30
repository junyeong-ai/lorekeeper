#[tokio::main]
async fn main() -> miette::Result<()> {
    lore::run().await
}
