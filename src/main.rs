use clap::Parser;
use pafd::cli::Cli;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    if let Err(e) = Cli::parse().run().await {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
