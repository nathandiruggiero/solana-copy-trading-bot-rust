use solana_sdk::pubkey::Pubkey;
use std::{
    env,
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    str::FromStr,
};
use anyhow::Result;
use log::{info, warn};
use dotenv::dotenv;

/// Configuration struct for the copy trading bot
#[derive(Debug, Clone)]
pub struct Config {
    pub rpc_https: String,
    pub rpc_wss: String,
    pub private_key: String,
    pub target_wallets: Vec<Pubkey>,
    pub trade_percentage: f64,
    pub slippage: f64,
    pub log_format: String,
    pub dry_run: bool,
    pub metis_endpoint: String,
    pub priority_fee_level: String,
}

impl Config {
    pub async fn new() -> Self {
        // Load environment variables from .env file if present
        dotenv().ok();
        
        // Load environment variables
        let rpc_https = env::var("RPC_HTTPS").unwrap_or_else(|_| "https://mainnet.helius-rpc.com/?api-key=default".to_string());
        let rpc_wss = env::var("RPC_WSS").unwrap_or_else(|_| "wss://atlas-mainnet.helius-rpc.com/?api-key=default".to_string());
        let private_key = env::var("PRIVATE_KEY").unwrap_or_else(|_| "default_private_key".to_string());
        let trade_percentage_str = env::var("TOKEN_PERCENTAGE").unwrap_or_else(|_| "1".to_string());
        let slippage_str = env::var("SLIPPAGE").unwrap_or_else(|_| "10".to_string());
        let log_format = env::var("LOG_FORMAT").unwrap_or_else(|_| "pretty".to_string());
        let dry_run = env::var("DRY_RUN").unwrap_or_else(|_| "true".to_string()).to_lowercase() == "true";
        let metis_endpoint = env::var("METIS_ENDPOINT").unwrap_or_else(|_| "".to_string());
        let priority_fee_level = env::var("PRIORITY_FEE_LEVEL").unwrap_or_else(|_| "low".to_string());
        
        let trade_percentage = trade_percentage_str.parse::<f64>().unwrap_or(1.0) / 100.0; // Convert percentage to decimal
        let slippage = slippage_str.parse::<f64>().unwrap_or(10.0) / 100.0; // Convert percentage to decimal
        
        // Load target wallets from file or environment
        let target_wallets = load_target_wallets_from_file().unwrap_or_else(|_| {
            info!("Failed to load target wallets from file; using default or env");
            vec![Pubkey::from_str("8uvia8bNfEHFaxcEpg5uLJoTXJoZ9frsfgBU6JemUgNt").unwrap()]
        });
        
        info!("Configuration loaded: RPC HTTPS={}, WSS={}, Trade Percentage={}%, Slippage={}%, LogFormat={}, DryRun={}", rpc_https, rpc_wss, trade_percentage * 100.0, slippage * 100.0, &log_format, dry_run);
        
        Config {
            rpc_https,
            rpc_wss,
            private_key,
            target_wallets,
            trade_percentage,
            slippage,
            log_format,
            dry_run,
            metis_endpoint,
            priority_fee_level,
        }
    }
}

fn load_target_wallets_from_file() -> Result<Vec<Pubkey>> {
    let path = Path::new("targetlist.txt");
    if !path.exists() {
        return Err(anyhow::anyhow!("targetlist.txt not found"));
    }
    
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut wallets = Vec::new();
    
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            if let Ok(pubkey) = Pubkey::from_str(trimmed) {
                wallets.push(pubkey);
            } else {
                warn!("Invalid Pubkey format in targetlist.txt: {}", trimmed);
            }
        }
    }
    
    if wallets.is_empty() {
        return Err(anyhow::anyhow!("No valid wallets found in targetlist.txt"));
    }
    
    info!("Loaded {} target wallets from targetlist.txt", wallets.len());
    Ok(wallets)
}