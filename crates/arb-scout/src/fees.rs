//! Fee estimation for the scout. We never actually pay these — they're
//! subtracted from the gross spread to compute *net* expected profit, which
//! is what determines whether an opportunity would have been worth pursuing.
//!
//! Reference for the inputs: `.env.example` `SCOUT_*` values.
//!
//! ## Live SOL price
//!
//! The estimator holds `sol_price_micros: Arc<AtomicU64>` (USD × 1_000_000)
//! so a background task can refresh the price from a live SOL/USDC pool
//! without reaching for a lock. `0` means "unknown" — the lamport-denominated
//! fees still compute, they just don't get converted into USDC.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
const USDC_DECIMALS: u32 = 6;

/// Price unit factor: stored as USD × this multiplier.
pub const SOL_PRICE_SCALE: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeBreakdown {
    pub flashloan_fee_usdc:    u64,
    pub priority_fee_lamports: u64,
    pub jito_tip_lamports:     u64,
    pub total_fees_usdc:       u64,
}

#[derive(Clone)]
pub struct FeeEstimator {
    pub flashloan_fee_bps:                 u64,
    pub priority_fee_microlamports_per_cu: u64,
    pub compute_unit_limit:                u64,
    pub base_tip_lamports:                 u64,
    pub profit_tip_percent:                u64,
    /// USD × 1_000_000. `0` ⇒ price unknown ⇒ skip lamport→USDC conversion.
    pub sol_price_micros:                  Arc<AtomicU64>,
}

impl FeeEstimator {
    pub fn sol_price_usd(&self) -> f64 {
        self.sol_price_micros.load(Ordering::Relaxed) as f64 / SOL_PRICE_SCALE as f64
    }

    pub fn set_sol_price_usd(&self, price: f64) {
        let micros = (price * SOL_PRICE_SCALE as f64).max(0.0) as u64;
        self.sol_price_micros.store(micros, Ordering::Relaxed);
    }

    pub fn estimate(&self, borrow_usdc: u64, gross_profit_usdc: u64) -> FeeBreakdown {
        let flashloan_fee_usdc = borrow_usdc.saturating_mul(self.flashloan_fee_bps) / 10_000;

        let priority_fee_lamports = self.priority_fee_microlamports_per_cu
            .saturating_mul(self.compute_unit_limit) / 1_000_000;

        let sol_price_usd = self.sol_price_usd();
        let gross_profit_dollars = (gross_profit_usdc as f64) / 10f64.powi(USDC_DECIMALS as i32);
        let profit_tip_dollars   = gross_profit_dollars * (self.profit_tip_percent as f64) / 100.0;
        let profit_tip_lamports  = if sol_price_usd > 0.0 {
            ((profit_tip_dollars / sol_price_usd) * LAMPORTS_PER_SOL as f64) as u64
        } else { 0 };
        let jito_tip_lamports = self.base_tip_lamports.saturating_add(profit_tip_lamports);

        let lamports_total = priority_fee_lamports.saturating_add(jito_tip_lamports);
        let lamport_fees_usdc = if sol_price_usd > 0.0 {
            ((lamports_total as f64 / LAMPORTS_PER_SOL as f64) * sol_price_usd
                * 10f64.powi(USDC_DECIMALS as i32)) as u64
        } else { 0 };
        let total_fees_usdc = flashloan_fee_usdc.saturating_add(lamport_fees_usdc);

        FeeBreakdown {
            flashloan_fee_usdc,
            priority_fee_lamports,
            jito_tip_lamports,
            total_fees_usdc,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn estimator(price: f64) -> FeeEstimator {
        FeeEstimator {
            flashloan_fee_bps:                  9,
            priority_fee_microlamports_per_cu:  50_000,
            compute_unit_limit:                 600_000,
            base_tip_lamports:                  10_000,
            profit_tip_percent:                 30,
            sol_price_micros:                   Arc::new(AtomicU64::new((price * SOL_PRICE_SCALE as f64) as u64)),
        }
    }

    #[test]
    fn test_scout_fee_estimation_includes_all_costs() {
        let f = estimator(150.0).estimate(1_000_000_000, 5_000_000);
        assert_eq!(f.flashloan_fee_usdc, 900_000);
        assert_eq!(f.priority_fee_lamports, 30_000);
        assert_eq!(f.jito_tip_lamports, 10_010_000);
        assert!(f.total_fees_usdc > f.flashloan_fee_usdc);
    }

    #[test]
    fn test_scout_net_profit_subtracts_fees() {
        let est = estimator(150.0);
        let f = est.estimate(1_000_000_000, 10_000_000);
        let net = 10_000_000_i64 - f.total_fees_usdc as i64;
        assert!(net < 10_000_000);
        assert!(f.total_fees_usdc > 0);
    }

    #[test]
    fn zero_borrow_zero_profit_yields_only_priority_and_base_tip() {
        let f = estimator(150.0).estimate(0, 0);
        assert_eq!(f.flashloan_fee_usdc, 0);
        assert_eq!(f.priority_fee_lamports, 30_000);
        assert_eq!(f.jito_tip_lamports, 10_000);
    }

    #[test]
    fn fee_is_monotonic_in_borrow() {
        let e = estimator(150.0);
        let f_small = e.estimate(1_000_000, 0);
        let f_large = e.estimate(1_000_000_000, 0);
        assert!(f_large.flashloan_fee_usdc > f_small.flashloan_fee_usdc);
    }

    #[test]
    fn zero_sol_price_does_not_panic() {
        let e = estimator(0.0);
        let f = e.estimate(1_000_000_000, 5_000_000);
        assert_eq!(f.flashloan_fee_usdc, 900_000);
        assert_eq!(f.total_fees_usdc, 900_000);
    }

    #[test]
    fn set_sol_price_updates_atomically() {
        let e = estimator(0.0);
        e.set_sol_price_usd(172.5);
        assert!((e.sol_price_usd() - 172.5).abs() < 1e-3);
    }
}
