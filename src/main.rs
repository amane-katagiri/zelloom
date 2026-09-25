use clap::Parser;
use zelloom::cli::{Cli, dispatch};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    dispatch(cli).await
}
