use anyhow::{anyhow, Result};
use base64::{engine::general_purpose, Engine as _};
use bincode;
use log::{error, info};
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_client::rpc_request::TokenAccountsFilter;
use solana_sdk::{
    commitment_config::{CommitmentConfig, CommitmentLevel},
    message::VersionedMessage,
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::VersionedTransaction,
};
use std::str::FromStr;

use crate::common::config::Config;
use serde_json::Value;

/// Calculate lamports to spend for a buy based on wallet balance and trade percentage
pub async fn calculate_buy_lamports(
    rpc: &RpcClient,
    wallet_pubkey: &Pubkey,
    config: &Config,
) -> Result<u64> {
    let balance = rpc.get_balance(wallet_pubkey).await?; // in lamports
    let mut to_spend = (balance as f64 * config.trade_percentage) as u64;
    // Enforce minimum buy in lamports
    let min_lamports = (config.min_buy_sol * 1_000_000_000.0) as u64;
    if to_spend < min_lamports { to_spend = min_lamports.min(balance); }
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
    info!(
        "[COPY-REQ] endpoint={} type={} wallet={} mint={} in_amount_raw={} slippage_bps={} priority={}",
        endpoint,
        trade_type,
        wallet,
        mint,
        in_amount_raw,
        slippage_bps,
        priority_fee_level.unwrap_or("none")
    );
    let resp = http_client
        .post(endpoint)
        .json(&req)
        .send()
        .await?;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        let body_text = String::from_utf8_lossy(&bytes);
        error!(
            "[COPY-ERR] endpoint={} status={} body={}",
            endpoint,
            status,
            body_text
        );
        return Err(anyhow!(format!("swap endpoint error: {}", status)));
    }
    let body: PumpFunSwapResponse = serde_json::from_slice(&bytes)?;
    let data = general_purpose::STANDARD.decode(&body.transaction)?;
    info!("[COPY-RX] tx_bytes={} b64_len={}", data.len(), body.transaction.len());
    let tx: VersionedTransaction = bincode::deserialize(&data)?;
    // Brief introspection for debugging
    let version = match &tx.message {
        VersionedMessage::Legacy(_) => "legacy",
        VersionedMessage::V0(_) => "v0",
    };
    info!("[COPY-TX] version={} sigs_present={}", version, tx.signatures.len());
    Ok(tx)
}

/// Request a Jupiter swap tx (supports Raydium/other routes). Handles WSOL wrapping via API.
pub async fn fetch_jupiter_swap_tx(
    http_client: &reqwest::Client,
    config: &Config,
    wallet: &str,
    input_mint: &str,
    output_mint: &str,
    in_amount_raw: u64,
    slippage_bps: u32,
) -> Result<VersionedTransaction> {
    // 1) Quote
    let url = format!(
        "{}?inputMint={}&outputMint={}&amount={}&slippageBps={}&onlyDirectRoutes=true&asLegacyTransaction=false",
        config.jupiter_quote_url,
        input_mint,
        output_mint,
        in_amount_raw,
        slippage_bps
    );
    info!(
        "[JUP-QUOTE] {} input={} output={} amount={} bps={}",
        config.jupiter_quote_url, input_mint, output_mint, in_amount_raw, slippage_bps
    );
    let quote_resp = http_client.get(&url).send().await;
    let quote_resp = match quote_resp {
        Ok(r) => r,
        Err(e) => {
            error!("[JUP-QUOTE-ERR] http error: {}", e);
            return Err(anyhow!(e));
        }
    };
    if !quote_resp.status().is_success() {
        let status = quote_resp.status();
        let body = quote_resp.text().await.unwrap_or_default();
        error!("[JUP-QUOTE-ERR] status={} body={}", status, body);
        return Err(anyhow!(format!("jupiter quote error: {}", status)));
    }
    let quote_json: Value = match quote_resp.json().await {
        Ok(j) => j,
        Err(e) => {
            error!("[JUP-QUOTE-ERR] json parse: {}", e);
            return Err(anyhow!(e));
        }
    };
    let first_route = quote_json
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|arr| arr.first())
        .cloned()
        .ok_or_else(|| anyhow!("no jupiter route"))?;
    let routes_len = quote_json.get("data").and_then(|d| d.as_array()).map(|a| a.len()).unwrap_or(0);
    info!("[JUP-ROUTES] count={}", routes_len);

    // 2) Swap
    let mut swap_body = serde_json::Map::new();
    swap_body.insert("quoteResponse".to_string(), first_route);
    swap_body.insert("userPublicKey".to_string(), Value::String(wallet.to_string()));
    swap_body.insert("wrapAndUnwrapSol".to_string(), Value::Bool(true));
    swap_body.insert("asLegacyTransaction".to_string(), Value::Bool(false));
    swap_body.insert("useSharedAccounts".to_string(), Value::Bool(false));
    swap_body.insert("dynamicComputeUnitLimit".to_string(), Value::Bool(true));
    // Optional: carry slippage explicitly
    swap_body.insert("slippageBps".to_string(), Value::Number(slippage_bps.into()));
    // Optional: priority fee hint
    let pri_lamports: u64 = match config.priority_fee_level.as_str() {
        "low" => 0,
        "medium" => 150_000,
        "high" => 400_000,
        "extreme" => 1_000_000,
        _ => 0,
    };
    if pri_lamports > 0 {
        swap_body.insert("prioritizationFeeLamports".to_string(), Value::Number((pri_lamports as u64).into()));
    }

    info!(
        "[JUP-SWAP] {} wallet={} input={} output={} amount={}",
        config.jupiter_swap_url, wallet, input_mint, output_mint, in_amount_raw
    );
    info!("[JUP-SWAP] POST {}", config.jupiter_swap_url);
    let swap_resp = match http_client
        .post(&config.jupiter_swap_url)
        .json(&Value::Object(swap_body))
        .send()
        .await {
            Ok(r) => r,
            Err(e) => { error!("[JUP-SWAP-ERR] http error: {}", e); return Err(anyhow!(e)); }
        };
    if !swap_resp.status().is_success() {
        let status = swap_resp.status();
        let body = swap_resp.text().await.unwrap_or_default();
        error!("[JUP-SWAP-ERR] status={} body={}", status, body);
        return Err(anyhow!(format!("jupiter swap error: {}", status)));
    }
    #[derive(Deserialize)]
    struct JupiterSwapResponse { #[serde(rename = "swapTransaction")] swap_tx: String }
    let body: JupiterSwapResponse = match swap_resp.json().await {
        Ok(j) => j,
        Err(e) => { error!("[JUP-SWAP-ERR] json parse: {}", e); return Err(anyhow!(e)); }
    };
    let data = general_purpose::STANDARD.decode(&body.swap_tx)?;
    info!("[JUP-RX] tx_bytes={} b64_len={}", data.len(), body.swap_tx.len());
    let tx: VersionedTransaction = bincode::deserialize(&data)?;
    Ok(tx)
}

/// Sign and send a VersionedTransaction
pub async fn sign_and_send(
    rpc: &RpcClient,
    tx: &mut VersionedTransaction,
    payer: &Keypair,
) -> Result<Signature> {
    // Log message size pre-sign
    let msg_bytes = tx.message.serialize();
    info!(
        "[COPY-SEND] msg_len={} sigs_before={}",
        msg_bytes.len(),
        tx.signatures.len()
    );
    // Fill the first required signature with payer's signature over the message
    let payer_sig = payer.sign_message(&msg_bytes);
    if tx.signatures.is_empty() {
        tx.signatures.push(payer_sig);
    } else {
        tx.signatures[0] = payer_sig;
    }
    // Allow skipping preflight based on config through an env-derived flag
    // We don't have config here; detect from env for simplicity
    let skip = std::env::var("SKIP_PREFLIGHT").unwrap_or_else(|_| "false".to_string()).to_lowercase() == "true";
    let cfg = RpcSendTransactionConfig {
        skip_preflight: skip,
        preflight_commitment: Some(CommitmentLevel::Processed),
        ..RpcSendTransactionConfig::default()
    };
    let sig = rpc.send_transaction_with_config(tx, cfg).await?;
    info!("[COPY-SUBMIT] signature={}", sig);
    // best-effort confirmation
    let _ = rpc
        .confirm_transaction_with_commitment(&sig, CommitmentConfig::confirmed())
        .await;
    Ok(sig)
} 