use anyhow::{anyhow, Result};
use chrono::Utc;
use log::{error, info};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_sdk::{
	commitment_config::CommitmentConfig,
	pubkey::Pubkey,
	signature::{Keypair, Signature, Signer},
};
use solana_transaction_status::{
	option_serializer::OptionSerializer, UiTransactionEncoding, UiTransactionStatusMeta,
};
use std::{
	collections::VecDeque,
	str::FromStr,
	sync::{Arc, RwLock},
	time::{Duration, Instant},
};
use tokio::time::sleep;

use crate::common::config::Config;

#[derive(Debug, Clone)]
pub struct TransactionMetadata {
	pub signature: String,
	pub instruction_type: Option<String>,
	pub mint: Option<String>,
	pub direction: Option<String>,
	pub input_sol: Option<f64>,
	pub output_tokens: Option<f64>,
	pub tx_fee_lamports: u64,
	pub priority_lamports: u64,
	pub compute_units_consumed: u64,
}

pub async fn copytrader(config: &Config) {
	info!("Starting Solana Copy Trading Bot");

	let rpc_client = RpcClient::new_with_commitment(
		config.rpc_https.clone(),
		CommitmentConfig::confirmed(),
	);

	let target_wallets: Arc<Vec<Pubkey>> = Arc::new(config.target_wallets.clone());
	let recent_signatures: Arc<RwLock<VecDeque<(String, Instant)>>> =
		Arc::new(RwLock::new(VecDeque::with_capacity(20_000)));

	// Derive our wallet pubkey from PRIVATE_KEY once for sizing decisions
	let wallet_keypair = Keypair::from_base58_string(&config.private_key);
	let our_wallet_pubkey = wallet_keypair.pubkey();

	info!("Polling signatures for {} wallets (confirmed)", target_wallets.len());

	loop {
		for wallet in target_wallets.iter() {
			if let Err(e) = poll_wallet_push(&rpc_client, wallet, &recent_signatures, config, &our_wallet_pubkey).await {
				error!("poll error {}: {}", wallet, e);
			}
			// brief pause between wallets to avoid rate limits
			sleep(Duration::from_millis(150)).await;
		}
		// short cycle to keep latency low
		sleep(Duration::from_millis(400)).await;
	}
}

async fn poll_wallet_push(
	rpc_client: &RpcClient,
	wallet: &Pubkey,
	recent: &Arc<RwLock<VecDeque<(String, Instant)>>>,
	config: &Config,
	our_wallet_pubkey: &Pubkey,
) -> Result<()> {
	let cfg = GetConfirmedSignaturesForAddress2Config {
		before: None,
		until: None,
		limit: Some(50),
		commitment: Some(CommitmentConfig::confirmed()),
	};
	let sigs = rpc_client
		.get_signatures_for_address_with_config(wallet, cfg)
		.await?;
	for s in sigs {
		let sig = s.signature;
		if insert_recent(recent, &sig) {
			let sig_parsed = Signature::from_str(&sig)?;
			match fetch_and_parse(rpc_client, &sig_parsed).await {
				Ok(md) => {
					// Base classification log
					log_transaction_metadata(&md);

					// Fee-greater-or-equal-than-input guard (Buy: input SOL must be > total fees)
					let total_fee_sol = (md.tx_fee_lamports + md.priority_lamports) as f64 / 1_000_000_000.0;
					let fee_ge_input = match (md.direction.as_deref(), md.input_sol) {
						(Some("Buy"), Some(inp)) => inp <= total_fee_sol + 1e-9,
						_ => false,
					};
					if fee_ge_input {
						info!(
							"[DECISION] action=pass reason=fee_ge_input signature={} input_sol={:.9} total_fee_sol={:.9}",
							md.signature,
							md.input_sol.unwrap_or(0.0),
							total_fee_sol
						);
						continue;
					}

					// Decide to copy or pass: require mint and direction
					let should_copy = md.mint.is_some() && md.direction.is_some();
					if should_copy {
						if config.dry_run {
							info!(
								"[DECISION] action=copy dry_run=true signature={} mint={:?} direction={:?}",
								md.signature, md.mint, md.direction
							);
							// Compute planned amounts
							if let Some(dir) = md.direction.as_deref() {
								match dir {
									"Buy" => {
										let planned = crate::engine::swap::calculate_buy_lamports(&rpc_client, our_wallet_pubkey, config).await.unwrap_or(0);
										info!("[COPY-PLAN] buy_lamports={} (~{:.6} SOL)", planned, planned as f64 / 1_000_000_000.0);
									}
									"Sell" => {
										if let Some(mint_str) = &md.mint { if let Ok(m) = Pubkey::from_str(mint_str) {
											let full_amt = crate::engine::swap::calculate_full_sell_amount(&rpc_client, our_wallet_pubkey, &m).await.unwrap_or(0);
											info!("[COPY-PLAN] sell_amount_raw={} (full token balance)", full_amt);
										}}
									}
									_ => {}
								}
							}
						} else {
							info!(
								"[DECISION] action=copy dry_run=false signature={} mint={:?} direction={:?}",
								md.signature, md.mint, md.direction
							);
							// execute_copy_trade(...) would be called here in live mode
						}
					} else {
						info!(
							"[DECISION] action=pass reason=missing_required_fields signature={} mint={:?} direction={:?}",
							md.signature, md.mint, md.direction
						);
					}
				}
				Err(e) => {
					let msg = e.to_string();
					if msg.starts_with("filtered:") {
						info!("[DECISION] action=pass reason={} signature={}", msg, sig);
					} else {
						error!("parse error for {}: {}", sig, msg);
					}
				}
			}
		}
	}
	Ok(())
}

fn insert_recent(recent: &Arc<RwLock<VecDeque<(String, Instant)>>>, sig: &str) -> bool {
	let mut guard = recent.write().unwrap();
	let now = Instant::now();
	while let Some((_, ts)) = guard.front() {
		if now.duration_since(*ts) > Duration::from_secs(120) {
			guard.pop_front();
		} else {
			break;
		}
	}
	if guard.iter().any(|(s, _)| s == sig) {
		return false;
	}
	if guard.len() >= 20_000 {
		guard.pop_front();
	}
	guard.push_back((sig.to_string(), now));
	true
}

async fn fetch_and_parse(
	rpc_client: &RpcClient,
	signature: &Signature,
) -> Result<TransactionMetadata> {
	let cfg = RpcTransactionConfig {
		encoding: Some(UiTransactionEncoding::JsonParsed),
		max_supported_transaction_version: Some(0),
		commitment: Some(CommitmentConfig::confirmed()),
	};
	let tx = rpc_client
		.get_transaction_with_config(signature, cfg)
		.await?;

	let meta: &UiTransactionStatusMeta = tx
		.transaction
		.meta
		.as_ref()
		.ok_or_else(|| anyhow!("missing meta"))?;

	let tx_fee_lamports = meta.fee;
	let compute_units_consumed = match &meta.compute_units_consumed {
		OptionSerializer::Some(v) => *v,
		_ => 0,
	};

	let logs: Vec<String> = match &meta.log_messages {
		OptionSerializer::Some(v) => v.clone(),
		_ => Vec::new(),
	};
	let instruction_type = logs
		.iter()
		.find_map(|l| l.strip_prefix("Program log: Instruction: "))
		.map(|s| s.to_string());

	// Only accept Buy*/Sell* classes when present
	let is_buy_sell = instruction_type
		.as_deref()
		.map(|s| s.starts_with("Buy") || s.starts_with("Sell"))
		.unwrap_or(false);

	// Priority fee from ComputeBudget logs (CU price × CU consumed)
	let cu_price_micro = extract_cu_price_micro_from_logs(&logs);
	let priority_lamports = cu_price_micro
		.map(|micro| micro.saturating_mul(compute_units_consumed))
		.map(|micro_total| micro_total / 1_000_000)
		.unwrap_or(0);

	// Mint from token balance changes
	let mint = extract_mint_from_balances(meta);

	// Direction and amounts from balances
	let pre_sol = meta.pre_balances.first().copied().unwrap_or(0) as i128;
	let post_sol = meta.post_balances.first().copied().unwrap_or(0) as i128;
	let input_sol = if post_sol < pre_sol {
		Some((pre_sol - post_sol) as f64 / 1_000_000_000.0)
	} else {
		None
	};

	let (output_tokens, direction_token) = extract_token_delta(meta);
	let direction = if let Some(dir) = direction_token {
		Some(dir)
	} else if post_sol < pre_sol {
		Some("Buy".to_string())
	} else if post_sol > pre_sol {
		Some("Sell".to_string())
	} else {
		None
	};

	// Final filter: require either explicit Buy*/Sell* instruction or a resolved direction
	if !is_buy_sell && direction.is_none() {
		return Err(anyhow!("filtered: not a buy/sell instruction"));
	}

	Ok(TransactionMetadata {
		signature: signature.to_string(),
		instruction_type,
		mint,
		direction,
		input_sol,
		output_tokens,
		tx_fee_lamports,
		priority_lamports,
		compute_units_consumed,
	})
}

fn extract_cu_price_micro_from_logs(logs: &[String]) -> Option<u64> {
	for l in logs {
		if l.contains("SetComputeUnitPrice") {
			let digits: String = l.chars().filter(|c| c.is_ascii_digit()).collect();
			if let Ok(v) = digits.parse::<u64>() { return Some(v); }
		}
	}
	None
}

fn extract_mint_from_balances(meta: &UiTransactionStatusMeta) -> Option<String> {
	match (&meta.pre_token_balances, &meta.post_token_balances) {
		(OptionSerializer::Some(pre_arr), OptionSerializer::Some(post_arr)) => {
			for (a, b) in pre_arr.iter().zip(post_arr.iter()) {
				let a_amt = a.ui_token_amount.ui_amount.unwrap_or(0.0);
				let b_amt = b.ui_token_amount.ui_amount.unwrap_or(0.0);
				if b_amt > a_amt {
					return Some(b.mint.clone());
				}
			}
			post_arr.first().map(|x| x.mint.clone())
		}
		_ => None,
	}
}

fn extract_token_delta(meta: &UiTransactionStatusMeta) -> (Option<f64>, Option<String>) {
	match (&meta.pre_token_balances, &meta.post_token_balances) {
		(OptionSerializer::Some(pre_arr), OptionSerializer::Some(post_arr)) => {
			for (a, b) in pre_arr.iter().zip(post_arr.iter()) {
				let pre_amt = a.ui_token_amount.ui_amount.unwrap_or(0.0);
				let post_amt = b.ui_token_amount.ui_amount.unwrap_or(0.0);
				if post_amt > pre_amt {
					return (Some(post_amt - pre_amt), Some("Buy".to_string()));
				}
				if pre_amt > post_amt {
					return (Some(pre_amt - post_amt), Some("Sell".to_string()));
				}
			}
			(None, None)
		}
		_ => (None, None),
	}
}

fn log_transaction_metadata(md: &TransactionMetadata) {
	let ts = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
	let instr = md
		.instruction_type
		.as_ref()
		.map(|s| s.as_str())
		.unwrap_or("Unknown");
	let icon = if instr.contains("Buy") { ":grand_cercle_vert:" } else if instr.contains("Sell") { ":grand_cercle_rouge:" } else { ":point_d_interrogation:" };

	info!("[{}] [COPY-BOT] => {} Detected {}", ts, icon, instr);
	if let Some(m) = &md.mint { info!("   ↳ Mint: {}", m); }
	if let Some(sol) = md.input_sol { info!("   ↳ Input: {:.6} SOL", sol); }
	if let Some(tok) = md.output_tokens { info!("   ↳ Tokens: {:.6}", tok); }
	if let (Some(sol), Some(tok)) = (md.input_sol, md.output_tokens) { if tok > 0.0 { info!("   ↳ Price: {:.8} SOL/token", sol / tok); } }
	let fee_sol = md.tx_fee_lamports as f64 / 1_000_000_000.0;
	let pri_sol = md.priority_lamports as f64 / 1_000_000_000.0;
	info!("   ↳ Fee: {:.6} SOL | Priority: {:.6} SOL | Total: {:.6} SOL", fee_sol, pri_sol, fee_sol + pri_sol);
} 