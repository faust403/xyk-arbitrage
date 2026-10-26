//! Jupiter v6 Swap API client — turns a quote into an executable transaction.
//!
//! Flow per arb leg:
//! 1. `quote` (already in `crate::jupiter`) returns the route + outAmount
//! 2. `swap_transaction` here POSTs that quote to `/v6/swap` with our wallet
//!    pubkey and gets back a base64-encoded `VersionedTransaction` ready to
//!    sign (signature slots are pre-allocated and zeroed)
//! 3. We sign with `signing::sign_versioned_transaction_b64`
//! 4. Either `sendTransaction` directly (single leg) OR pack into a Jito
//!    bundle with the second leg + tip (atomic two-leg arb)

use std::time::Duration;

use arb_core::{ArbError, ArbResult};
use base64::Engine;
use serde_json::{json, Value};
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey::Pubkey;

/// Free public endpoint base. Override via `JUPITER_ENDPOINT_BASE` env var
/// to switch to the paid surface (`https://api.jup.ag/swap/v1`) when an
/// API key is set.
///
/// **2026-05-12 endpoint migration:** Jupiter retired `quote-api.jup.ag`
/// (DNS no longer resolves). The new free public surface is
/// `lite-api.jup.ag/swap/v1` — same v6-equivalent schema, different
/// host. Paid users hit `api.jup.ag/swap/v1`. We default to lite-api so
/// callers without an API key still work.
const DEFAULT_JUPITER_BASE: &str = "https://lite-api.jup.ag/swap/v1";

/// Quote response, kept opaque — we forward it directly back to /v6/swap.
#[derive(Debug, Clone)]
pub struct JupiterQuoteResponse {
    pub raw:        Value,
    pub out_amount: u64,
}

#[derive(Debug, Clone)]
pub struct JupiterSwapResponse {
    /// Base64-encoded VersionedTransaction with empty signature slot(s).
    pub swap_tx_b64:           String,
    /// Slot to put OUR signature in (Jupiter sets us as the fee payer at index 0).
    pub signer_index:          usize,
    /// Last valid block-height for the transaction (must be `<` this when sent).
    pub last_valid_block_height: u64,
}

#[derive(Clone)]
pub struct JupiterSwapClient {
    http:    reqwest::Client,
    base:    String,
    api_key: Option<String>,
    /// Soft back-off after a 429: when set, all quote/swap calls short-circuit
    /// without hitting the network until the deadline expires. Arc+Mutex so
    /// the Clone'd client shares one back-off state across worker tasks.
    back_off: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
}

impl Default for JupiterSwapClient {
    fn default() -> Self { Self::from_env() }
}

impl JupiterSwapClient {
    /// Construct from environment: `JUPITER_API_KEY` (optional) and
    /// `JUPITER_ENDPOINT_BASE` (optional, defaults to the free public base).
    pub fn from_env() -> Self {
        let api_key = std::env::var("JUPITER_API_KEY").ok()
            .filter(|s| !s.trim().is_empty() && !s.starts_with("put-"));
        // If user set an API key but didn't override the base, switch to the
        // paid endpoint automatically.
        let base = std::env::var("JUPITER_ENDPOINT_BASE")
            .unwrap_or_else(|_| if api_key.is_some() {
                "https://api.jup.ag/swap/v1".to_string()
            } else {
                DEFAULT_JUPITER_BASE.to_string()
            });
        if let Some(k) = &api_key {
            tracing::info!(target: "scout::jupiter",
                base = %base, key_chars = k.len(),
                "Jupiter Swap client configured with API key");
        }
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(12))
                .build().expect("reqwest"),
            base, api_key,
            back_off: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Returns true if we're currently in 429 back-off. Side-effect-free
    /// check at the start of each call.
    fn jupiter_in_back_off(&self) -> bool {
        match self.back_off.lock() {
            Ok(g) => match *g {
                Some(until) => std::time::Instant::now() < until,
                None => false,
            },
            Err(_) => false,
        }
    }

    /// Record a back-off deadline after Jupiter responds with 429.
    /// Duration is conservative — Jupiter doesn't publish a `Retry-After`,
    /// so we wait 2 s to let any burst clear without escalating into a
    /// hard ban.
    fn jupiter_record_back_off(&self) {
        let until = std::time::Instant::now() + Duration::from_millis(2_000);
        if let Ok(mut g) = self.back_off.lock() {
            *g = Some(until);
        }
        tracing::warn!(target: "scout::jupiter",
            "⏸ Jupiter 429 — backing off all calls for 2 s");
    }

    pub fn new() -> Self { Self::from_env() }

    fn auth_get(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.get(url);
        if let Some(k) = &self.api_key {
            req = req.header("x-api-key", k);
        }
        req
    }

    fn auth_post(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.post(url);
        if let Some(k) = &self.api_key {
            req = req.header("x-api-key", k);
        }
        req
    }

    /// Fetch a quote — returns the raw JSON to forward to /v6/swap, plus
    /// the parsed outAmount for our own profitability check.
    ///
    /// Two routing controls read from env each call (cheap; lets the operator
    /// retune via Railway dashboard without a redeploy):
    /// * `EXECUTOR_EXCLUDE_DEXES` (comma-separated) → forwarded as Jupiter's
    ///   `excludeDexes`. Use to filter out DEXes whose routes hit Jito's
    ///   "bundles cannot lock any vote accounts" guard (typically stake-pool
    ///   adapters: Marinade, Lido, Sanctum, SolFi).
    /// * `EXECUTOR_MAX_ACCOUNTS` (default unset → Jupiter's default of 64) →
    ///   `maxAccounts`. Lower values force Jupiter into shorter routes, which
    ///   helps tx size when Jito bundle limits bite.
    pub async fn quote(
        &self, input_mint: &Pubkey, output_mint: &Pubkey,
        input_amount: u64, slippage_bps: u32,
    ) -> ArbResult<JupiterQuoteResponse> {
        // Short-circuit when in 429 back-off — saves the network round-trip
        // AND prevents the back-off from escalating.
        if self.jupiter_in_back_off() {
            return Err(ArbError::Rpc("jupiter quote: in 429 back-off".into()));
        }
        // ─── Paid-tier knob set ────────────────────────────────────────
        //
        // `restrictIntermediateTokens` — when `true`, Jupiter sticks to a
        // curated set of intermediates (USDC, USDT, SOL, mSOL, JitoSOL).
        // OFF lets it use long-tail intermediates that surface real arb.
        //
        // `dynamicSlippage` — Jupiter auto-tunes per-route slippage based
        // on its own price-impact model. PAID feature; replaces our fixed
        // `slippage_bps` with a route-aware value. Surfaces +5-20% more
        // routes that the static slippage would have killed.
        //
        // `maxAccounts` — caps tx-builder account count. Default 64 keeps
        // tx under v0 size limit BUT cuts off legitimate 4+ hop routes
        // through pools with many vault/oracle accounts. Bump to 128
        // (still safely under our 256-byte ALT-compressed budget) to let
        // Jupiter explore deeper routes.
        //
        // `onlyDirectRoutes=false` — explicit. Without it some clients
        // default to direct-only which kills multi-hop arb routing.
        //
        // `swapMode=ExactIn` — explicit, matches our "given X USDC in,
        // what's the most USDC I can get back?" arb pattern.
        // `restrictIntermediateTokens=true` is critical here.
        //
        // With `false` (our earlier setting) Jupiter routed through 4-hop
        // paths via niche pools — those routes are MORE profitable on
        // paper ($1-5 net at $5K) BUT the resulting tx is too large
        // (~40 distinct accounts most not in our ALT) so Jito rejects
        // with `transaction #0 could not be decoded`. We verified this
        // pattern: every `📈 Jupiter cleared min-profit` came with
        // sendBundle failing on the same error.
        //
        // `true` keeps Jupiter on a curated intermediate set (USDC,
        // USDT, SOL, mSOL, JitoSOL, …) — the same default the scout's
        // own cross-check uses, which DOES land bundles successfully.
        // The arbs we lose are the niche-pool ones we can't size for
        // anyway.
        let restrict = std::env::var("JUPITER_RESTRICT_INTERMEDIATES")
            .as_deref() != Ok("false"); // default ON now
        let mut url = format!(
            "{}/quote?inputMint={input_mint}&outputMint={output_mint}\
             &amount={input_amount}&slippageBps={slippage_bps}\
             &restrictIntermediateTokens={restrict}",
            self.base, restrict = restrict,
        );
        // INCLUDE (whitelist) takes priority over EXCLUDE (blacklist) —
        // see jupiter.rs for the rationale. Setting `EXECUTOR_INCLUDE_DEXES`
        // to e.g. "Raydium,Raydium CLMM,Whirlpool,Meteora DLMM" is the only
        // reliable way to dodge Jito's "bundles cannot lock any vote
        // accounts" rejection — exclude lists keep missing the offending
        // adapter as Jupiter onboards new DEXes.
        let inc_set = std::env::var("EXECUTOR_INCLUDE_DEXES").ok()
            .map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if let Some(inc) = &inc_set {
            let encoded = inc.replace(' ', "%20").replace(',', "%2C");
            url.push_str(&format!("&dexes={encoded}"));
        } else if let Ok(excl) = std::env::var("EXECUTOR_EXCLUDE_DEXES") {
            let trimmed = excl.trim();
            if !trimmed.is_empty() {
                let encoded = trimmed.replace(' ', "%20").replace(',', "%2C");
                url.push_str(&format!("&excludeDexes={encoded}"));
            }
        }
        if let Ok(max) = std::env::var("EXECUTOR_MAX_ACCOUNTS") {
            if let Ok(n) = max.trim().parse::<u32>() {
                url.push_str(&format!("&maxAccounts={n}"));
            }
        }
        let raw: Value = self.auth_get(&url).send().await
            .map_err(|e| ArbError::Rpc(format!("jupiter quote: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("jupiter quote parse: {e}")))?;

        if let Some(err) = raw.get("error") {
            return Err(ArbError::Rpc(format!("jupiter quote error: {err}")));
        }
        // 429 from Jupiter's API Gateway returns `{"code":429,"message":"..."}`
        // (no `error` field). Detect explicitly and trigger back-off so the
        // polling worker doesn't keep hammering during the rate-limit window.
        if raw.get("code").and_then(|v| v.as_u64()) == Some(429) {
            self.jupiter_record_back_off();
            return Err(ArbError::Rpc(format!("jupiter quote: rate-limited (429): {raw}")));
        }
        let out_amount = raw.get("outAmount")
            .and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| ArbError::Rpc(format!("jupiter quote: missing outAmount: {raw}")))?;
        Ok(JupiterQuoteResponse { raw, out_amount })
    }

    /// Build a swap transaction from a previously-fetched quote.
    /// `user_pubkey` is the wallet that will sign + receive output.
    /// If `jito_tip_lamports` is `Some`, Jupiter embeds a Jito-tip transfer
    /// inside the returned tx (so the bundle has a tip without us building
    /// SystemProgram::Transfer manually).
    pub async fn swap_transaction(
        &self,
        quote:             &JupiterQuoteResponse,
        user_pubkey:       &Pubkey,
        jito_tip_lamports: Option<u64>,
    ) -> ArbResult<JupiterSwapResponse> {
        // When sending via Jito bundle, priority fee is wasted (bundles are
        // selected by tip, not priority fee) — so route the whole budget
        // into a jito tip if requested. Otherwise let Jupiter pick a sane
        // priority fee for normal sendTransaction.
        let prio = match jito_tip_lamports {
            Some(tip) => json!({ "jitoTipLamports": tip }),
            None      => json!("auto"),
        };
        let body = json!({
            "quoteResponse":             quote.raw,
            "userPublicKey":             user_pubkey.to_string(),
            "wrapAndUnwrapSol":          true,
            "prioritizationFeeLamports": prio,
            "asLegacyTransaction":       false,
            "useSharedAccounts":         true,
        });

        let url = format!("{}/swap", self.base);
        let resp: Value = self.auth_post(&url).json(&body).send().await
            .map_err(|e| ArbError::Rpc(format!("jupiter swap: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("jupiter swap parse: {e}")))?;

        if let Some(err) = resp.get("error") {
            return Err(ArbError::Rpc(format!("jupiter swap error: {err}")));
        }
        let swap_tx_b64 = resp.get("swapTransaction")
            .and_then(|v| v.as_str()).map(|s| s.to_string())
            .ok_or_else(|| ArbError::Rpc(format!("jupiter swap: missing swapTransaction: {resp}")))?;
        let last_valid_block_height = resp.get("lastValidBlockHeight")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ArbError::Rpc("jupiter swap: missing lastValidBlockHeight".into()))?;
        // Jupiter docs: signer is always at slot 0 (fee payer = our wallet).
        Ok(JupiterSwapResponse { swap_tx_b64, signer_index: 0, last_valid_block_height })
    }
}

/// Decomposed instructions returned from Jupiter's `/swap-instructions`
/// endpoint. Lets us assemble a *single* atomic Solana transaction that
/// contains both legs of an arb cycle — no Jito required.
#[derive(Debug, Clone)]
pub struct JupiterSwapInstructions {
    /// Compute budget IXs Jupiter recommends. We usually replace with our
    /// own (single budget at the top of the combined tx) but keep them
    /// available for inspection.
    pub compute_budget_ixs:    Vec<Instruction>,
    /// Wraps SOL, creates ATAs, etc. — must run BEFORE the swap.
    pub setup_ixs:             Vec<Instruction>,
    /// The actual swap instruction (the heart of the leg).
    pub swap_ix:               Instruction,
    /// Closes temp wSOL ATA / unwraps SOL — runs AFTER the swap.
    pub cleanup_ix:            Option<Instruction>,
    /// V0-tx LookupTable addresses Jupiter recommends. We *ignore* these
    /// for legacy txs; if the combined tx is too large for legacy we'll
    /// either fall back to Jito or build a V0 tx (deferred).
    pub address_lookup_tables: Vec<Pubkey>,
}

impl JupiterSwapClient {
    /// Fetch decomposed instructions for a swap (Jupiter `/swap-instructions`).
    /// `wrap_unwrap_sol=false` assumes the caller manages a permanent wSOL
    /// ATA (saves rent ~$0.002 per leg). Pass `true` when the route uses
    /// SOL as base and the wallet has no pre-funded wSOL — Jupiter will
    /// insert the wrap/unwrap ixs into setup/cleanup automatically.
    pub async fn swap_instructions(
        &self,
        quote:        &JupiterQuoteResponse,
        user_pubkey:  &Pubkey,
        wrap_unwrap_sol: bool,
    ) -> ArbResult<JupiterSwapInstructions> {
        let body = json!({
            "quoteResponse":      quote.raw,
            "userPublicKey":      user_pubkey.to_string(),
            "wrapAndUnwrapSol":   wrap_unwrap_sol,
            "asLegacyTransaction": true,
            "useSharedAccounts":  true,
        });
        let url = format!("{}/swap-instructions", self.base);
        let resp: Value = self.auth_post(&url).json(&body).send().await
            .map_err(|e| ArbError::Rpc(format!("jupiter swap-instructions: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("jupiter swap-instructions parse: {e}")))?;
        if let Some(err) = resp.get("error") {
            return Err(ArbError::Rpc(format!("jupiter swap-instructions error: {err}")));
        }

        let parse_arr = |key: &str| -> ArbResult<Vec<Instruction>> {
            resp.get(key).and_then(|v| v.as_array())
                .map(|arr| arr.iter().map(parse_instruction_json).collect::<ArbResult<Vec<_>>>())
                .unwrap_or(Ok(vec![]))
        };
        let compute_budget_ixs = parse_arr("computeBudgetInstructions")?;
        let setup_ixs          = parse_arr("setupInstructions")?;

        let swap_ix = resp.get("swapInstruction")
            .ok_or_else(|| ArbError::Rpc(format!("swap-instructions: missing swapInstruction: {resp}")))
            .and_then(parse_instruction_json)?;

        let cleanup_ix = resp.get("cleanupInstruction")
            .filter(|v| !v.is_null())
            .map(parse_instruction_json).transpose()?;

        let address_lookup_tables = resp.get("addressLookupTableAddresses")
            .and_then(|v| v.as_array()).cloned().unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.as_str().and_then(|s| s.parse::<Pubkey>().ok()))
            .collect();

        Ok(JupiterSwapInstructions {
            compute_budget_ixs, setup_ixs, swap_ix, cleanup_ix, address_lookup_tables,
        })
    }
}

/// Convert a Jupiter-shaped instruction object into a `solana_program::Instruction`.
/// Jupiter encodes `data` as base64 in v6.
fn parse_instruction_json(v: &Value) -> ArbResult<Instruction> {
    let program_id_str = v.get("programId").and_then(|p| p.as_str())
        .ok_or_else(|| ArbError::Rpc("jupiter ix: missing programId".into()))?;
    let program_id = program_id_str.parse::<Pubkey>()
        .map_err(|e| ArbError::Rpc(format!("jupiter ix: bad programId {program_id_str}: {e}")))?;

    let accounts_arr = v.get("accounts").and_then(|a| a.as_array())
        .ok_or_else(|| ArbError::Rpc("jupiter ix: missing accounts".into()))?;
    let mut accounts = Vec::with_capacity(accounts_arr.len());
    for a in accounts_arr {
        let pk_str = a.get("pubkey").and_then(|p| p.as_str())
            .ok_or_else(|| ArbError::Rpc("jupiter ix: account missing pubkey".into()))?;
        let pubkey = pk_str.parse::<Pubkey>()
            .map_err(|e| ArbError::Rpc(format!("jupiter ix: bad account pubkey {pk_str}: {e}")))?;
        accounts.push(AccountMeta {
            pubkey,
            is_signer:   a.get("isSigner").and_then(|s| s.as_bool()).unwrap_or(false),
            is_writable: a.get("isWritable").and_then(|s| s.as_bool()).unwrap_or(false),
        });
    }

    let data_str = v.get("data").and_then(|d| d.as_str())
        .ok_or_else(|| ArbError::Rpc("jupiter ix: missing data".into()))?;
    // Jupiter v6 returns `data` as base64. Earlier versions used base58; we
    // try both for robustness.
    let data = base64::engine::general_purpose::STANDARD.decode(data_str)
        .or_else(|_| bs58::decode(data_str).into_vec().map_err(|_| base64::DecodeError::InvalidPadding))
        .map_err(|e| ArbError::Rpc(format!("jupiter ix: data decode failed (tried b64 then b58): {e:?}")))?;

    Ok(Instruction { program_id, accounts, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_constructs() { let _ = JupiterSwapClient::new(); }

    #[test]
    fn parse_instruction_json_minimal() {
        let v: Value = serde_json::from_str(r#"{
            "programId": "11111111111111111111111111111111",
            "accounts": [
                {"pubkey": "11111111111111111111111111111111", "isSigner": true, "isWritable": false}
            ],
            "data": "AQID"
        }"#).unwrap();
        let ix = parse_instruction_json(&v).expect("parse");
        assert_eq!(ix.accounts.len(), 1);
        assert!(ix.accounts[0].is_signer);
        assert!(!ix.accounts[0].is_writable);
        assert_eq!(ix.data, vec![1u8, 2, 3]);  // AQID base64 = 0x01,0x02,0x03
    }

    #[test]
    fn parse_instruction_json_b58_fallback() {
        // "Ldp" base58 decodes to [1,2,3]
        let v: Value = serde_json::from_str(r#"{
            "programId": "11111111111111111111111111111111",
            "accounts": [],
            "data": "Ldp"
        }"#).unwrap();
        let ix = parse_instruction_json(&v).expect("parse fallback");
        assert_eq!(ix.data, vec![1u8, 2, 3]);
    }

    #[test]
    fn parse_instruction_json_rejects_bad_pubkey() {
        let v: Value = serde_json::from_str(r#"{
            "programId": "not-a-pubkey",
            "accounts": [],
            "data": ""
        }"#).unwrap();
        assert!(parse_instruction_json(&v).is_err());
    }
}
