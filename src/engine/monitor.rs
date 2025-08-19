use anyhow::{anyhow, Result};
use log::{error, info};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::pubsub_client::PubsubClient as BlockingPubsubClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_config::{RpcTransactionConfig, RpcTransactionLogsConfig, RpcTransactionLogsFilter};
use solana_client::rpc_response::Response;
use solana_sdk::{
	commitment_config::CommitmentConfig,
	pubkey::Pubkey,
	signature::{Keypair, Signature, Signer},
};
use solana_transaction_status::{
	option_serializer::OptionSerializer, UiTransactionEncoding, UiTransactionStatusMeta,
};
use std::{
	collections::{HashMap, VecDeque},
	str::FromStr,
	sync::{Arc, RwLock},
	time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::spawn_blocking, time::sleep};

use crate::common::config::Config;
use crate::engine::swap::{calculate_buy_lamports, calculate_full_sell_amount};

const PUMP_FUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const PUMP_FUN_AMM_PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const METEORA_DBC_PROGRAM_ID: &str = "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

#[derive(Debug, Clone)]
pub struct TransactionMetadata {
	pub signature: String,
	pub instruction_type: Option<String>,
	pub mint: Option<String>,
	pub direction: Option<String>,
	pub input_sol: Option<f64>,
	pub output_tokens: Option<f64>,
	pub output_sol: Option<f64>,
	pub tx_fee_lamports: u64,
	pub priority_lamports: u64,
	pub compute_units_consumed: u64,
	pub slot: u64,
	pub block_time: Option<i64>,
	pub elapsed_ms: u128,
}

#[derive(Debug, Default)]
struct BotPnlState {
	// Dry-run holdings (estimated token quantities) and cost basis per mint
	holdings_tokens_by_mint: HashMap<String, f64>,
	invested_sol_by_mint: HashMap<String, f64>,
	realized_pnl_sol: f64,
	start_balance_sol: Option<f64>,
	buy_count: u64,
	sell_count: u64,
	total_buy_sol_planned: f64,
	total_sell_sol_observed: f64,
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
	let last_seen: Arc<RwLock<HashMap<Pubkey, String>>> = Arc::new(RwLock::new(HashMap::new()));

	// Derive our wallet pubkey from PRIVATE_KEY once for sizing decisions
	let wallet_keypair = Keypair::from_base58_string(&config.private_key);
	let our_wallet_pubkey = wallet_keypair.pubkey();

	// Startup parameters resume
	log_startup_summary(config, &our_wallet_pubkey);
	log_mode(config.dry_run);

	// PnL state and starting balance
	let pnl_state: Arc<RwLock<BotPnlState>> = Arc::new(RwLock::new(BotPnlState::default()));
	if let Ok(bal) = rpc_client.get_balance(&our_wallet_pubkey).await {
		pnl_state.write().unwrap().start_balance_sol = Some(bal as f64 / 1_000_000_000.0);
		info!("[PNL] start_balance_sol={:.9}", (bal as f64 / 1_000_000_000.0));
	}
	// Start lightweight periodic PnL summary logger (every 30s)
	start_pnl_logger(config.rpc_https.clone(), our_wallet_pubkey, Arc::clone(&pnl_state));

	// WS channel and consumer for low latency
	let (tx_ws, mut rx_ws) = mpsc::unbounded_channel::<(String, u64)>();
	let recent_for_ws = Arc::clone(&recent_signatures);
	let rpc_for_ws = RpcClient::new_with_commitment(config.rpc_https.clone(), CommitmentConfig::confirmed());
	let config_ws = config.clone();
	let pnl_state_ws = Arc::clone(&pnl_state);
	let our_wallet_ws = our_wallet_pubkey;
	tokio::spawn(async move {
		let mut highest_ws_slot: u64 = rpc_for_ws.get_slot().await.unwrap_or(0);
		while let Some((sig, slot)) = rx_ws.recv().await {
			if slot <= highest_ws_slot { continue; }
			highest_ws_slot = slot;
			if !insert_recent(&recent_for_ws, &sig) { continue; }
			if let Ok(parsed) = Signature::from_str(&sig) {
				match fetch_and_parse(&rpc_for_ws, &parsed).await {
					Ok(md) => {
						log_transaction_metadata_with_format(&md, &config_ws);
						let total_fee_sol = (md.tx_fee_lamports + md.priority_lamports) as f64 / 1_000_000_000.0;
						let fee_ge_input = match (md.direction.as_deref(), md.input_sol) { (Some("Buy"), Some(inp)) => inp <= total_fee_sol + 1e-9, _ => false };
						if fee_ge_input {
							info!("[DECISION] action=pass reason=fee_ge_input signature={} input_sol={:.9} total_fee_sol={:.9}", md.signature, md.input_sol.unwrap_or(0.0), total_fee_sol);
							continue;
						}
						let should_copy = md.mint.is_some() && md.direction.is_some();
						if should_copy {
							let mint_key = md.mint.clone().unwrap_or_default();
							match md.direction.as_deref() {
								Some("Buy") => {
									let planned_lamports = calculate_buy_lamports(&rpc_for_ws, &our_wallet_ws, &config_ws).await.unwrap_or(0);
									let planned_sol = planned_lamports as f64 / 1_000_000_000.0;
									// Estimate tokens: use target trade rate tokens_per_sol if available from output_tokens/input_sol
									let est_tokens = match (md.input_sol, md.output_tokens) {
										(Some(in_sol), Some(out_tok)) if in_sol > 0.0 => {
											let tokens_per_sol = out_tok / in_sol;
											tokens_per_sol * planned_sol
										}
										_ => 0.0,
									};
									let mut st = pnl_state_ws.write().unwrap();
									*st.invested_sol_by_mint.entry(mint_key.clone()).or_insert(0.0) += planned_sol;
									*st.holdings_tokens_by_mint.entry(mint_key.clone()).or_insert(0.0) += est_tokens;
									st.buy_count += 1;
									st.total_buy_sol_planned += planned_sol;
									let unit_price_micro = if md.compute_units_consumed > 0 { (md.priority_lamports.saturating_mul(1_000_000)) / md.compute_units_consumed } else { 0 };
									let est_priority_lamports = if md.compute_units_consumed > 0 { (unit_price_micro.saturating_mul(md.compute_units_consumed)) / 1_000_000 } else { 0 };
									let est_total_fee_sol = (md.tx_fee_lamports + est_priority_lamports) as f64 / 1_000_000_000.0;
									info!(
										"[DECISION] action=copy dry_run={} wallet={} dir=Buy mint={} planned={:.6} SOL est_tokens={:.0} fees_est={:.9} SOL cu_price_micro={} sig={}",
										config_ws.dry_run,
										our_wallet_ws,
										mint_key,
										planned_sol,
										est_tokens,
										est_total_fee_sol,
										unit_price_micro,
										md.signature,
									);
								}
								Some("Sell") => {
									// Only sell if we currently have a position in this mint (one-shot sell until next buy)
									let has_position = { pnl_state_ws.read().unwrap().invested_sol_by_mint.contains_key(&mint_key) };
									if !has_position {
										info!("[DECISION] action=pass reason=no_position mint={} sig={}", mint_key, md.signature);
										continue;
									}
									// Fetch our current token raw balance (sum of accounts)
									let token_raw = if let Ok(mint_pk) = Pubkey::from_str(&mint_key) {
										calculate_full_sell_amount(&rpc_for_ws, &our_wallet_ws, &mint_pk).await.unwrap_or(0)
									} else { 0 };
									let st = pnl_state_ws.read().unwrap();
									let cost = st.invested_sol_by_mint.get(&mint_key).copied().unwrap_or(0.0);
									let unit_price_micro = if md.compute_units_consumed > 0 { (md.priority_lamports.saturating_mul(1_000_000)) / md.compute_units_consumed } else { 0 };
									let est_priority_lamports = if md.compute_units_consumed > 0 { (unit_price_micro.saturating_mul(md.compute_units_consumed)) / 1_000_000 } else { 0 };
									let est_total_fee_sol = (md.tx_fee_lamports + est_priority_lamports) as f64 / 1_000_000_000.0;
									if let Some(out_sol) = md.output_sol {
										let mut st = pnl_state_ws.write().unwrap();
										let cost = st.invested_sol_by_mint.remove(&mint_key).unwrap_or(0.0);
										let our_tokens = st.holdings_tokens_by_mint.remove(&mint_key).unwrap_or(0.0);
										let est_price_per_token = match md.output_tokens { Some(tok) if tok > 0.0 => out_sol / tok, _ => 0.0 };
										let est_proceeds = if est_price_per_token > 0.0 { our_tokens * est_price_per_token } else { out_sol };
										let change = est_proceeds - cost;
										st.realized_pnl_sol += change;
										st.sell_count += 1;
										st.total_sell_sol_observed += est_proceeds;
										let start = st.start_balance_sol.unwrap_or(0.0);
										let sim_balance = start + st.realized_pnl_sol;
										info!("[PNL] after_sell (planned) mint={} est_proceeds={:.6} SOL cost={:.6} SOL realized_change={:.6} SOL total_realized={:.6} SOL sim_balance={:.6} SOL", mint_key, est_proceeds, cost, change, st.realized_pnl_sol, sim_balance);
									}
									info!(
										"[DECISION] action=copy dry_run={} wallet={} dir=Sell mint={} token_raw={} cost_basis={:.6} SOL fees_est={:.9} SOL cu_price_micro={} sig={}",
										config_ws.dry_run,
										our_wallet_ws,
										mint_key,
										token_raw,
										cost,
										est_total_fee_sol,
										unit_price_micro,
										md.signature,
									);
								}
								_ => {}
							}
						} else {
							info!("[DECISION] action=pass reason=missing_required_fields signature={} mint={:?} direction={:?}", md.signature, md.mint, md.direction);
						}
					}
					Err(e) => {
						let msg = e.to_string();
						if msg.starts_with("filtered:") { info!("[DECISION] action=pass reason={} signature={}", msg, sig); } else { error!("parse error for {}: {}", sig, msg); }
					}
				}
			}
		}
	});

	// Start WS subscriber using blocking PubsubClient inside spawn_blocking
	let wss = config.rpc_wss.clone();
	if wss.starts_with("ws") {
		info!("Initializing WebSocket logs_subscribe on {}", wss);
		for w in target_wallets.iter() { info!("Subscribing to wallet logs via WS: {}", w); }
		let wallets_for_ws = Arc::clone(&target_wallets);
		let tx_ws_clone = tx_ws.clone();
		tokio::spawn(async move {
			if let Err(e) = start_ws(wss, wallets_for_ws, tx_ws_clone).await { error!("ws error: {}", e); }
		});
	}

	info!("Polling signatures for {} wallets (confirmed)", target_wallets.len());

	loop {
		for wallet in target_wallets.iter() {
			if let Err(e) = poll_wallet_push(
				&rpc_client,
				wallet,
				&recent_signatures,
				config,
				&our_wallet_pubkey,
				&last_seen,
				&pnl_state,
			).await {
				error!("poll error {}: {}", wallet, e);
			}
			sleep(Duration::from_millis(150)).await;
		}
		sleep(Duration::from_millis(400)).await;
	}
}

// Start WS subscriber using static logs_subscribe API (no constructor)
async fn start_ws(wss_url: String, wallets: Arc<Vec<Pubkey>>, tx: mpsc::UnboundedSender<(String, u64)>) -> Result<()> {
	spawn_blocking(move || {
		let mut _subs = Vec::new();
		let cfg = RpcTransactionLogsConfig { commitment: Some(CommitmentConfig::processed()) };
		// Wallet subscriptions only (focus strictly on target wallets)
		for w in wallets.iter() {
			match BlockingPubsubClient::logs_subscribe(
				&wss_url,
				RpcTransactionLogsFilter::Mentions(vec![w.to_string()]),
				cfg.clone(),
			) {
				Ok((sub, recv)) => {
					info!("WS subscribed: wallet {}", w);
					_subs.push(sub);
					let tx_w = tx.clone();
					std::thread::spawn(move || {
						while let Ok(res) = recv.recv() {
							let Response { context, value } = res;
							let _ = tx_w.send((value.signature.clone(), context.slot));
						}
					});
				}
				Err(e) => error!("ws subscribe wallet error: {}", e),
			}
		}

		// Keep subscriptions alive
		loop { std::thread::sleep(Duration::from_secs(3600)); }
	}).await.ok();
	Ok(())
}

async fn poll_wallet_push(
	rpc_client: &RpcClient,
	wallet: &Pubkey,
	recent: &Arc<RwLock<VecDeque<(String, Instant)>>>,
	config: &Config,
	our_wallet_pubkey: &Pubkey,
	last_seen: &Arc<RwLock<HashMap<Pubkey, String>>>,
	pnl_state: &Arc<RwLock<BotPnlState>>,
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

	// Determine already-seen boundary
	let mut map = last_seen.write().unwrap();
	if !map.contains_key(wallet) {
		if let Some(newest) = sigs.first() { map.insert(*wallet, newest.signature.clone()); }
		return Ok(()); // seed only, skip processing old history
	}
	let boundary = map.get(wallet).cloned();
	drop(map);

	// Find new signatures strictly newer than boundary
	let mut found_boundary = boundary.is_none();
	let boundary_sig = boundary.unwrap_or_default();
	let mut to_process: Vec<String> = Vec::new();
	for s in sigs.iter().rev() { // oldest -> newest
		if s.signature == boundary_sig { found_boundary = true; continue; }
		if found_boundary { to_process.push(s.signature.clone()); }
	}

	// Update last_seen to newest fetched
	if let Some(newest) = sigs.first() {
		last_seen.write().unwrap().insert(*wallet, newest.signature.clone());
	}

	for sig in to_process {
		if insert_recent(recent, &sig) {
			let sig_parsed = Signature::from_str(&sig)?;
			match fetch_and_parse(rpc_client, &sig_parsed).await {
				Ok(md) => {
					// Base classification log
					log_transaction_metadata_with_format(&md, config);

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
						// Compute planned amounts and estimated fees for our copy
						let (planned_info, ata_info) = if let Some(dir) = md.direction.as_deref() {
							match dir {
								"Buy" => {
									let planned_lamports = calculate_buy_lamports(&rpc_client, our_wallet_pubkey, config).await.unwrap_or(0);
									let planned_sol = planned_lamports as f64 / 1_000_000_000.0;
									// Estimate CU price from observed tx (avoid divide-by-zero)
									let unit_price_micro = if md.compute_units_consumed > 0 { (md.priority_lamports.saturating_mul(1_000_000)) / md.compute_units_consumed } else { 0 };
									let est_priority_lamports = if md.compute_units_consumed > 0 { (unit_price_micro.saturating_mul(md.compute_units_consumed)) / 1_000_000 } else { 0 };
									let est_total_fee_sol = (5_000u64 + est_priority_lamports) as f64 / 1_000_000_000.0;
									// ATA for mint
									let ata_str = if let Some(mint_str) = &md.mint { if let Ok(m) = Pubkey::from_str(mint_str) { spl_associated_token_account::get_associated_token_address(our_wallet_pubkey, &m).to_string() } else { "Unknown".to_string() } } else { "Unknown".to_string() };
									// PnL counters for dry-run
									{
										let mut st = pnl_state.write().unwrap();
										*st.invested_sol_by_mint.entry(md.mint.clone().unwrap_or_default()).or_insert(0.0) += planned_sol;
										st.buy_count += 1;
										st.total_buy_sol_planned += planned_sol;
									}
									(Some((planned_sol, est_total_fee_sol, unit_price_micro)), Some(ata_str))
								}
								"Sell" => {
									let ata_str = if let Some(mint_str) = &md.mint { if let Ok(m) = Pubkey::from_str(mint_str) { spl_associated_token_account::get_associated_token_address(our_wallet_pubkey, &m).to_string() } else { "Unknown".to_string() } } else { "Unknown".to_string() };
									// Skip if no position (one-shot sell policy)
									{
										let st = pnl_state.read().unwrap();
										if !st.invested_sol_by_mint.contains_key(&md.mint.clone().unwrap_or_default()) {
											info!("[DECISION] action=pass reason=no_position mint={:?} sig={}", md.mint, md.signature);
										}
									}
									// Realize PnL on full sell
									if let Some(out_sol) = md.output_sol {
										let mint_key = md.mint.clone().unwrap_or_default();
										let mut st = pnl_state.write().unwrap();
										let cost = st.invested_sol_by_mint.remove(&mint_key).unwrap_or(0.0);
										let our_tokens = st.holdings_tokens_by_mint.remove(&mint_key).unwrap_or(0.0);
										let est_price_per_token = match md.output_tokens { Some(tok) if tok > 0.0 => out_sol / tok, _ => 0.0 };
										let est_proceeds = if est_price_per_token > 0.0 { our_tokens * est_price_per_token } else { out_sol };
										let change = est_proceeds - cost;
										st.realized_pnl_sol += change;
										st.sell_count += 1;
										st.total_sell_sol_observed += est_proceeds;
										let start = st.start_balance_sol.unwrap_or(0.0);
										let sim_balance = start + st.realized_pnl_sol;
										info!("[PNL] after_sell (planned) mint={} est_proceeds={:.6} SOL cost={:.6} SOL realized_change={:.6} SOL total_realized={:.6} SOL sim_balance={:.6} SOL", mint_key, est_proceeds, cost, change, st.realized_pnl_sol, sim_balance);
									}
									(None, Some(ata_str))
								}
								_ => (None, None)
							}
						} else { (None, None) };

						if config.dry_run {
							info!(
								"[DECISION] action=copy dry_run=true signature={} mint={:?} direction={:?}",
								md.signature, md.mint, md.direction
							);
							if let Some((planned_sol, est_fee_sol, unit_price_micro)) = planned_info {
								info!("[COPY-INTENT] wallet={} dir=Buy amount={:.6} SOL est_fees={:.9} SOL cu_price_micro={} ata={}", our_wallet_pubkey, planned_sol, est_fee_sol, unit_price_micro, ata_info.unwrap_or_else(|| "Unknown".to_string()));
							} else if md.direction.as_deref() == Some("Sell") {
								info!("[COPY-INTENT] wallet={} dir=Sell sell_all_tokens_of_mint={:?} ata={}", our_wallet_pubkey, md.mint, ata_info.unwrap_or_else(|| "Unknown".to_string()));
							}
						} else {
							info!(
								"[DECISION] action=copy dry_run=false signature={} mint={:?} direction={:?}",
								md.signature, md.mint, md.direction
							);
							// Option A execution wired elsewhere
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
	let started = Instant::now();
	let cfg = RpcTransactionConfig {
		encoding: Some(UiTransactionEncoding::JsonParsed),
		max_supported_transaction_version: Some(0),
		commitment: Some(CommitmentConfig::confirmed()),
	};
	let tx = match rpc_client
		.get_transaction_with_config(signature, cfg)
		.await
	{
		Ok(v) => v,
		Err(e) => {
			let msg = e.to_string();
			if msg.contains("invalid type: null") {
				return Err(anyhow!("filtered: tx_not_available"));
			}
			return Err(anyhow!(msg));
		}
	};

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

	// Must be Pump.fun or supported AMM program logs
	let pump_present = logs.iter().any(|l|
		l.contains(PUMP_FUN_PROGRAM_ID) ||
		l.contains(PUMP_FUN_AMM_PROGRAM_ID) ||
		l.contains(METEORA_DBC_PROGRAM_ID)
	);
	if !pump_present { return Err(anyhow!("filtered: not pump.fun")); }

	// Instruction type parsing
	let instruction_type = logs
		.iter()
		.find_map(|l| l.strip_prefix("Program log: Instruction: "))
		.map(|s| s.to_string());

	// Only accept: Buy, Sell, BuyExactIn, SellExactIn
	let is_allowed = instruction_type
		.as_deref()
		.map(|s| matches!(s, "Buy" | "Sell" | "BuyExactIn" | "SellExactIn"))
		.unwrap_or(false);
	if !is_allowed { return Err(anyhow!("filtered: not buy/sell exact")); }

	// Priority fee
	let cu_price_micro = extract_cu_price_micro_from_logs(&logs);
	let priority_lamports = cu_price_micro
		.map(|micro| micro.saturating_mul(compute_units_consumed))
		.map(|micro_total| micro_total / 1_000_000)
		.unwrap_or(0);

	let mint = extract_mint_from_balances(meta);

	let pre_sol = meta.pre_balances.first().copied().unwrap_or(0) as i128;
	let post_sol = meta.post_balances.first().copied().unwrap_or(0) as i128;
	let mut input_sol = if post_sol < pre_sol { Some((pre_sol - post_sol) as f64 / 1_000_000_000.0) } else { None };
	let mut output_sol = if post_sol > pre_sol { Some((post_sol - pre_sol) as f64 / 1_000_000_000.0) } else { None };

	// If native SOL unchanged, check WSOL movement
	if input_sol.is_none() && output_sol.is_none() {
		let (wsol_out, wsol_in) = extract_wsol_sol_delta(meta);
		if let Some(v) = wsol_out { input_sol = Some(v); }
		if let Some(v) = wsol_in { output_sol = Some(v); }
	}

	let (output_tokens, direction_token) = extract_token_delta(meta);

	// Derive direction
	let mut direction = instruction_type.as_deref().map(|s| if s.starts_with("Buy") { "Buy".to_string() } else { "Sell".to_string() });
	if direction.is_none() {
		direction = if let Some(dir) = direction_token { Some(dir) } else if input_sol.unwrap_or(0.0) > 0.0 { Some("Buy".to_string()) } else if output_sol.unwrap_or(0.0) > 0.0 { Some("Sell".to_string()) } else { None };
	}

	let elapsed_ms = started.elapsed().as_millis();

	Ok(TransactionMetadata {
		signature: signature.to_string(),
		instruction_type,
		mint,
		direction,
		input_sol,
		output_tokens,
		output_sol,
		tx_fee_lamports,
		priority_lamports,
		compute_units_consumed,
		slot: tx.slot,
		block_time: tx.block_time,
		elapsed_ms,
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
				if b_amt > a_amt && b.mint != WSOL_MINT {
					return Some(b.mint.clone());
				}
			}
			// fallback: first non-WSOL mint
			for b in post_arr.iter() {
				if b.mint != WSOL_MINT { return Some(b.mint.clone()); }
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
				if b.mint == WSOL_MINT { continue; }
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

fn extract_wsol_sol_delta(meta: &UiTransactionStatusMeta) -> (Option<f64>, Option<f64>) {
	match (&meta.pre_token_balances, &meta.post_token_balances) {
		(OptionSerializer::Some(pre_arr), OptionSerializer::Some(post_arr)) => {
			for (a, b) in pre_arr.iter().zip(post_arr.iter()) {
				if b.mint == WSOL_MINT {
					let pre_amt = a.ui_token_amount.ui_amount.unwrap_or(0.0);
					let post_amt = b.ui_token_amount.ui_amount.unwrap_or(0.0);
					if post_amt > pre_amt {
						// When native SOL unchanged, WSOL increase implies SOL output
						return (None, Some(post_amt - pre_amt));
					}
					if pre_amt > post_amt {
						return (Some(pre_amt - post_amt), None);
					}
				}
			}
			(None, None)
		}
		_ => (None, None),
	}
}

fn log_transaction_metadata_with_format(md: &TransactionMetadata, config: &Config) {
	let json_mode = config.log_format.to_lowercase() == "json";
	let instr = md.instruction_type.as_deref().unwrap_or("Unknown");
	let mint = md.mint.as_deref().unwrap_or("Unknown");
	let total_fee_sol = (md.tx_fee_lamports + md.priority_lamports) as f64 / 1_000_000_000.0;
	let unit_price_micro = if md.compute_units_consumed > 0 { (md.priority_lamports.saturating_mul(1_000_000)) / md.compute_units_consumed } else { 0 };
	let direction = md.direction.as_deref().unwrap_or("");
	if json_mode {
		let obj = serde_json::json!({
			"type": "bot",
			"sig": md.signature,
			"instruction": instr,
			"mint": mint,
			"direction": direction,
			"input_sol": md.input_sol,
			"output_sol": md.output_sol,
			"output_tokens": md.output_tokens,
			"fee_sol": total_fee_sol,
			"cu_price_micro": unit_price_micro,
			"elapsed_ms": md.elapsed_ms
		});
		info!("{}", obj);
		return;
	}
	let emoji = match direction { "Buy" => "🟢", "Sell" => "🔴", _ => "" };
	let flow = match direction {
		"Buy" => {
			let left = format_sol(md.input_sol.unwrap_or(0.0));
			let right = format_tokens(md.output_tokens.unwrap_or(0.0));
			format!("{} SOL → {} tokens", left, right)
		}
		"Sell" => {
			let left = format_tokens(md.output_tokens.unwrap_or(0.0));
			let right = format_sol(md.output_sol.unwrap_or(0.0));
			format!("{} tokens → {} SOL", left, right)
		}
		_ => "N/A".to_string(),
	};
	info!(
		"[BOT] sig={} | {} {} | mint={} | {} | fee={:.9} SOL | cu={} | t={}ms",
		md.signature,
		emoji,
		instr,
		mint,
		flow,
		total_fee_sol,
		unit_price_micro,
		md.elapsed_ms
	);
}

fn log_startup_summary(config: &Config, our_wallet: &Pubkey) {
	let trade_pct = config.trade_percentage * 100.0;
	let slippage_pct = config.slippage * 100.0;
	info!(
		"[STARTUP] rpc_https={} | rpc_wss={} | dry_run={} | log_format={} | trade_pct={:.2}% | slippage={:.2}% | targets={}",
		config.rpc_https,
		config.rpc_wss,
		config.dry_run,
		config.log_format,
		trade_pct,
		slippage_pct,
		config.target_wallets.len()
	);
	info!("[STARTUP] our_wallet={}", our_wallet);
	for w in &config.target_wallets {
		info!("[STARTUP] target_wallet={}", w);
	}

	// Start balance is logged synchronously in copytrader
}

fn log_mode(dry_run: bool) {
	if dry_run {
		info!("[MODE] DRY-RUN enabled: transactions will NOT be sent");
	} else {
		info!("[MODE] LIVE: transactions WILL be sent");
	}
}

fn format_sol(sol: f64) -> String { format!("{:.6}", sol) }
fn format_tokens(tokens: f64) -> String { if tokens >= 1000.0 { format!("{:.0}k", tokens / 1000.0) } else { format!("{:.0}", tokens) } }

fn start_pnl_logger(rpc_url: String, wallet: Pubkey, pnl_state: Arc<RwLock<BotPnlState>>) {
	tokio::spawn(async move {
		let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
		loop {
			let curr_sol = match rpc.get_balance(&wallet).await {
				Ok(l) => l as f64 / 1_000_000_000.0,
				Err(_) => { sleep(Duration::from_secs(30)).await; continue; }
			};
			let (start, realized, buys, sells, buy_sol, sell_sol) = {
				let st = pnl_state.read().unwrap();
				(
					st.start_balance_sol.unwrap_or(curr_sol),
					st.realized_pnl_sol,
					st.buy_count,
					st.sell_count,
					st.total_buy_sol_planned,
					st.total_sell_sol_observed,
				)
			};
			let delta = curr_sol - start;
			let total_trades = buys + sells;
			info!("[PNL-SUMMARY] wallet={} current={:.6} SOL delta_since_start={:.6} SOL realized={:.6} SOL total_trades={} buys={} ({:.6} SOL) sells={} ({:.6} SOL)", wallet, curr_sol, delta, realized, total_trades, buys, buy_sol, sells, sell_sol);
			sleep(Duration::from_secs(30)).await;
		}
	});
} 