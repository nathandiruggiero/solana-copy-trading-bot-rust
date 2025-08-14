use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_request::TokenAccountsFilter;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use crate::common::config::Config;

/// Calculate lamports to spend for a buy based on wallet balance and trade percentage
pub async fn calculate_buy_lamports(
    rpc: &RpcClient,
    wallet_pubkey: &Pubkey,
    config: &Config,
) -> Result<u64> {
    let balance = rpc.get_balance(wallet_pubkey).await?; // in lamports
    let mut to_spend = (balance as f64 * config.trade_percentage) as u64;
    // keep small buffer for fees
    let fee_buffer: u64 = 5_000; // ~0.000005 SOL
    if to_spend > fee_buffer {
        to_spend -= fee_buffer;
    }
    Ok(to_spend)
}

/// Calculate token amount to sell = full balance for the given mint
pub async fn calculate_full_sell_amount(
    rpc: &RpcClient,
    owner: &Pubkey,
    mint: &Pubkey,
) -> Result<u64> {
    // Fetch all token accounts for this mint
    let accounts = rpc
        .get_token_accounts_by_owner(owner, TokenAccountsFilter::Mint(*mint))
        .await?;

    let mut total: u64 = 0;
    for keyed in accounts {
        let acc_pubkey = Pubkey::from_str(&keyed.pubkey)?;
        let bal = rpc.get_token_account_balance(&acc_pubkey).await?;
        if let Ok(raw) = bal.amount.parse::<u64>() {
            total = total.saturating_add(raw);
        }
    }
    Ok(total)
} 