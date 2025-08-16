use anyhow::{anyhow, Result};
use base64::{engine::general_purpose, Engine as _};
use bincode;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_request::TokenAccountsFilter;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::VersionedTransaction,
};
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

#[derive(Debug, Serialize)]
struct PumpFunSwapRequest<'a> {
    wallet: &'a str,
    #[serde(rename = "type")] type_: &'a str, // "BUY" or "SELL"
    mint: &'a str,
    #[serde(rename = "inAmount")] in_amount: String, // raw units as string
    #[serde(rename = "priorityFeeLevel", skip_serializing_if = "Option::is_none")] priority_fee_level: Option<&'a str>,
    #[serde(rename = "slippageBps", skip_serializing_if = "Option::is_none")] slippage_bps: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct PumpFunSwapResponse {
    // base64 encoded (bincode) VersionedTransaction
    transaction: String,
}

/// Request a prebuilt Pump.fun swap tx from Metis/Jupiter-like endpoint
pub async fn fetch_pumpfun_swap_tx(
    http_client: &reqwest::Client,
    endpoint: &str,
    wallet: &str,
    trade_type: &str, // BUY | SELL
    mint: &str,
    in_amount_raw: u64,
    slippage_bps: u32,
    priority_fee_level: Option<&str>,
) -> Result<VersionedTransaction> {
    let req = PumpFunSwapRequest {
        wallet,
        type_: trade_type,
        mint,
        in_amount: in_amount_raw.to_string(),
        priority_fee_level: priority_fee_level,
        slippage_bps: Some(slippage_bps),
    };
    let resp = http_client
        .post(endpoint)
        .json(&req)
        .send()
        .await?
        .error_for_status()?;
    let body: PumpFunSwapResponse = resp.json().await?;
    let data = general_purpose::STANDARD.decode(&body.transaction)?;
    let tx: VersionedTransaction = bincode::deserialize(&data)?;
    Ok(tx)
}

/// Sign and send a VersionedTransaction
pub async fn sign_and_send(
    rpc: &RpcClient,
    tx: &mut VersionedTransaction,
    payer: &Keypair,
) -> Result<Signature> {
    // Fill the first required signature with payer's signature over the message
    let msg_bytes = tx.message.serialize();
    let payer_sig = payer.sign_message(&msg_bytes);
    if tx.signatures.is_empty() {
        tx.signatures.push(payer_sig);
    } else {
        tx.signatures[0] = payer_sig;
    }
    let sig = rpc.send_transaction(tx).await?;
    // best-effort confirmation
    let _ = rpc
        .confirm_transaction_with_commitment(&sig, CommitmentConfig::confirmed())
        .await;
    Ok(sig)
} 