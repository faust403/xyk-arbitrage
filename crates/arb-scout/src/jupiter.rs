//! Jupiter v6 Quote API client.
//!
//! Each opportunity our scout flags is computed with a SPOT-PRICE
//! APPROXIMATION (Whirlpool/CLMM/DLMM/Lifinity quoters all ignore slippage
//! past the current tick/bin). Jupiter, by contrast, returns the actual
//! best routed quote across every DEX it integrates — including slippage
//! and the specific instruction sequence a real trade would use.
//!
//! By querying Jupiter for every Ok(Some) opportunity we get TWO numbers:
//! 1. `scout_profit_usdc`     — what our spot quoter thinks
//! 2. `jupiter_profit_usdc`   — what a real Jupiter route would deliver
//!
//! Their RATIO tells us whether our scout is overoptimistic. If Jupiter
//! consistently agrees within a few percent, the opportunities are real
//! and Stage B (actual execution) is worth building. If Jupiter shows 5×
//! less profit, our spot approximation is the problem and we need full
//! tick-walking math before any real money goes out.
//!
//! ## Cost considerations
//!
//! Jupiter's quote API is free but rate-limited (~10 req/sec on the public
//! endpoint). With ~3000 opportunities per 5 minutes (= 10/sec) we'd be
//! right at the limit. To stay safe, the verifier samples opportunities
//! (e.g. only every Nth one, configurable via `SCOUT_JUPITER_SAMPLE_N`).

use std::time::Duration;

use arb_core::{ArbError, ArbResult};
use serde_json::Value;
use solana_program::pubkey::Pubkey;

const DEFAULT_JUPITER_BASE: &str = "https://quote-api.jup.ag/v6";

#[derive(Debug, Clone)]
pub struct JupiterQuote {
    /// What Jupiter says you'd actually receive for the given input.
    pub out_amount:        u64,
    /// Best-route slippage estimate Jupiter computed (basis points × 100).
    pub price_impact_bps:  Option<f64>,
    /// Number of swap legs Jupiter chose (1 = single DEX, 2+ = split route).
    pub route_hops:        usize,
    /// How long Jupiter's API call took. Useful to verify we're not the
    /// bottleneck — at >500 ms latency, Jupiter quotes are stale by the
    /// time we'd act on them.
    pub round_trip:        Duration,
}

#[derive(Clone)]
pub struct JupiterClient {
    http:    reqwest::Client,
    base:    String,
    api_key: Option<String>,
}

impl Default for JupiterClient {
    fn default() -> Self { Self::from_env() }
}

impl JupiterClient {
    pub fn from_env() -> Self {
        let api_key = std::env::var("JUPITER_API_KEY").ok()
            .filter(|s| !s.trim().is_empty() && !s.starts_with("put-"));
        let base = std::env::var("JUPITER_ENDPOINT_BASE")
            .unwrap_or_else(|_| if api_key.is_some() {
                "https://api.jup.ag/swap/v1".to_string()
            } else {
                DEFAULT_JUPITER_BASE.to_string()
            });
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(12))
                .build()
                .expect("reqwest client"),
            base, api_key,
        }
    }

    pub fn new() -> Self { Self::from_env() }

    /// Fetch a single-leg quote: how much `output_mint` would I get for
    /// `input_amount` of `input_mint`? Slippage cap defaults to 50 bps.
    pub async fn quote(
        &self,
        input_mint:    &Pubkey,
        output_mint:   &Pubkey,
        input_amount:  u64,
        slippage_bps:  u32,
    ) -> ArbResult<JupiterQuote> {
        let started = std::time::Instant::now();
        let mut url = format!(
            "{}/quote?inputMint={input_mint}&outputMint={output_mint}&amount={input_amount}&slippageBps={slippage_bps}",
            self.base,
        );
        // Mirror the executor's filters so scout doesn't surface
        // opportunities the executor can't trade on (Jito vote-account
        // rejections, oversized routes, etc.).
        //
        // INCLUDE takes priority over EXCLUDE: whitelisting is the only
        // reliable way to keep Jupiter inside DEXes whose routes don't
        // touch validator vote accounts. Set INCLUDE to e.g.
        //   "Raydium,Raydium CLMM,Whirlpool,Meteora DLMM"
        // to constrain routing tightly.
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
        let mut req = self.http.get(&url);
        if let Some(k) = &self.api_key { req = req.header("x-api-key", k); }
        let resp = req.send().await
            .map_err(|e| ArbError::Rpc(format!("jupiter quote send: {e}")))?;
        let status = resp.status();
        let body: Value = resp.json().await
            .map_err(|e| ArbError::Rpc(format!("jupiter quote parse ({status}): {e}")))?;

        if !status.is_success() {
            return Err(ArbError::Rpc(format!(
                "jupiter quote {status} for {input_mint}->{output_mint} amount={input_amount}: {body}"
            )));
        }

        let out_amount = body.get("outAmount")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| ArbError::Rpc(format!("jupiter response missing outAmount: {body}")))?;

        let price_impact_bps = body.get("priceImpactPct")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .map(|pct| pct * 10_000.0); // pct is fractional, multiply by 100×100

        let route_hops = body.get("routePlan")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        Ok(JupiterQuote {
            out_amount, price_impact_bps, route_hops,
            round_trip: started.elapsed(),
        })
    }

    /// Verify a USDC→X→USDC arb cycle by getting two back-to-back Jupiter
    /// quotes and computing the round-trip net.
    ///
    /// Returns `(jupiter_final_usdc, scout_vs_jupiter_ratio)` where the
    /// ratio is `jupiter_final / probe_amount` minus 1 — positive means
    /// Jupiter agrees there's an arb; negative means Jupiter sees no arb
    /// (and our scout was wrong).
    pub async fn verify_arb_cycle(
        &self,
        usdc_mint:    &Pubkey,
        intermediate: &Pubkey,
        probe_usdc:   u64,
        slippage_bps: u32,
    ) -> ArbResult<JupiterArbCheck> {
        // Leg 1: USDC -> intermediate
        let leg1 = self.quote(usdc_mint, intermediate, probe_usdc, slippage_bps).await?;
        // Leg 2: intermediate -> USDC, using leg1's output as input
        let leg2 = self.quote(intermediate, usdc_mint, leg1.out_amount, slippage_bps).await?;

        let final_usdc = leg2.out_amount;
        let net_profit_usdc: i64 = (final_usdc as i64) - (probe_usdc as i64);

        Ok(JupiterArbCheck {
            probe_usdc,
            intermediate_amount: leg1.out_amount,
            final_usdc,
            net_profit_usdc,
            leg1_hops:         leg1.route_hops,
            leg2_hops:         leg2.route_hops,
            leg1_impact_bps:   leg1.price_impact_bps,
            leg2_impact_bps:   leg2.price_impact_bps,
            total_round_trip:  leg1.round_trip + leg2.round_trip,
        })
    }
}

#[derive(Debug, Clone)]
pub struct JupiterArbCheck {
    pub probe_usdc:          u64,
    pub intermediate_amount: u64,
    pub final_usdc:          u64,
    pub net_profit_usdc:     i64,    // signed; negative if Jupiter sees no arb
    pub leg1_hops:           usize,
    pub leg2_hops:           usize,
    pub leg1_impact_bps:     Option<f64>,
    pub leg2_impact_bps:     Option<f64>,
    pub total_round_trip:    Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_constructs() {
        let _ = JupiterClient::new();
    }

    // No live API tests — would be flaky and rate-limit us. Integration
    // tests with a real API key live elsewhere.
}
