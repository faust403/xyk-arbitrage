use serde::{Deserialize, Serialize};

use crate::route::{Route, RouteId};
use crate::token::TokenAmount;

/// Itemised expected profit for one candidate execution of a [`Route`].
///
/// All amounts are in the *base mint's* base units (e.g. USDC at 6 decimals
/// for the default config). Keeping every fee on its own field — instead of
/// a single `net_profit` lump — lets the executor and the telemetry code
/// answer "where did the margin go?" without re-deriving from raw quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedProfit {
    /// How much we would put into the first hop.
    pub input:                       TokenAmount,
    /// How much the last hop would return to us, before any external fees.
    pub gross_output:                TokenAmount,
    /// `gross_output - input`, kept signed so a negative spread can be logged
    /// rather than collapsed to "no opportunity".
    pub gross_profit_base_units:     i128,
    /// Solend (or other source) flash-loan fee in base units.
    pub flash_loan_fee_base_units:   u64,
    /// Jito tip in lamports we plan to attach.
    pub jito_tip_lamports:           u64,
    /// Priority fee (CU * micro-lamports, normalised to lamports).
    pub priority_fee_lamports:       u64,
    /// `gross_profit - all_fees` after converting the lamport components to
    /// base units at the current SOL price. Signed for the same reason as
    /// `gross_profit_base_units`.
    pub net_profit_base_units:       i128,
}

impl ExpectedProfit {
    /// True when the expected net is strictly positive — i.e. the route would
    /// be a profitable submission *if* state didn't drift between simulation
    /// and execution. The on-chain `assert_min_profit` instruction is the
    /// final arbiter; this is just the pre-flight filter.
    pub fn is_profitable(&self) -> bool {
        self.net_profit_base_units > 0
    }
}

/// A candidate execution: a [`Route`] paired with its [`ExpectedProfit`] at
/// a specific input size, captured at a specific slot. The pathfinder emits
/// these; the executor consumes them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Opportunity {
    pub route_id:    RouteId,
    pub route:       Route,
    pub expected:    ExpectedProfit,
    /// Slot at which the underlying pool quotes were taken. Lets the
    /// executor reject opportunities that are already too old to send.
    pub quoted_slot: u64,
}

impl Opportunity {
    pub fn new(route: Route, expected: ExpectedProfit, quoted_slot: u64) -> Self {
        let route_id = route.id();
        Self { route_id, route, expected, quoted_slot }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::{Hop, Route};
    use crate::token::TokenMint;
    use solana_program::pubkey::Pubkey;

    fn usdc() -> TokenMint { TokenMint(Pubkey::new_from_array([1u8; 32])) }
    fn sol()  -> TokenMint { TokenMint(Pubkey::new_from_array([2u8; 32])) }

    fn sample_route() -> Route {
        let mut r = Route::new(usdc());
        r.extend(Hop::new(Pubkey::new_from_array([10u8; 32]), usdc(), sol())).unwrap();
        r.extend(Hop::new(Pubkey::new_from_array([11u8; 32]), sol(), usdc())).unwrap();
        r
    }

    fn sample_profit(net: i128) -> ExpectedProfit {
        ExpectedProfit {
            input:                       TokenAmount::new(usdc(), 100_000_000, 6),
            gross_output:                TokenAmount::new(usdc(), 100_500_000, 6),
            gross_profit_base_units:     500_000,
            flash_loan_fee_base_units:   90_000,
            jito_tip_lamports:           10_000,
            priority_fee_lamports:       1_000,
            net_profit_base_units:       net,
        }
    }

    #[test]
    fn test_expected_profit_is_profitable_threshold() {
        assert!(sample_profit(1).is_profitable());
        assert!(!sample_profit(0).is_profitable());
        assert!(!sample_profit(-1).is_profitable());
    }

    #[test]
    fn test_opportunity_route_id_matches_route() {
        let r = sample_route();
        let opp = Opportunity::new(r.clone(), sample_profit(123), 42);
        assert_eq!(opp.route_id, r.id());
        assert_eq!(opp.quoted_slot, 42);
    }
}
