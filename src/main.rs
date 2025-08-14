use solana_copytrading_bot::{
    common::{config::Config, logger::init_logger},
    engine::monitor::copytrader,
};

#[tokio::main]
async fn main() {
    // Load config
    let config = Config::new().await;

    // Init logger
    init_logger(&config.log_format);

    println!("Starting Solana Copy Trading Bot");

    // Run bot
    copytrader(&config).await;
}
