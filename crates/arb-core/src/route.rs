use serde::{Deserialize, Serialize};
use solana_program::pubkey::Pubkey;

use crate::error::{ArbError, ArbResult};
use crate::token::TokenMint;

/// A single swap step inside a [`Route`]. Identifies the pool, the side
/// (input mint), and the resulting output mint. The pool's `kind` is *not*
/// stored here — the orchestrator looks it up from the live `PoolStateStore`
/// at quote time so a hop never goes stale when a pool's metadata is refreshed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Hop {
    pub pool:        Pubkey,
    pub input_mint:  TokenMint,
    pub output_mint: TokenMint,
}

impl Hop {
    pub const fn new(pool: Pubkey, input_mint: TokenMint, output_mint: TokenMint) -> Self {
        Self { pool, input_mint, output_mint }
    }
}

/// Stable identifier for a [`Route`]. Same hops in the same order always
/// produce the same id; reordering or substituting any hop yields a different
/// id. Used to dedupe routes and to key per-route metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RouteId(pub u64);

impl std::fmt::Display for RouteId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "route#{:016x}", self.0)
    }
}

/// An ordered sequence of [`Hop`]s starting at `base_mint` and (for a closed
/// arb cycle) ending at the same mint. Open-ended routes are also legal so
/// the type can serve both the pathfinder (constructs cycles) and the
/// quoter (evaluates partial paths during search).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    pub base_mint: TokenMint,
    pub hops:      Vec<Hop>,
}

impl Route {
    /// New empty route anchored at `base_mint`. The first call to
    /// [`Route::extend`] determines what's hop[0]; the route's "current
    /// token" advances with each hop.
    pub fn new(base_mint: TokenMint) -> Self {
        Self { base_mint, hops: Vec::new() }
    }

    /// Token currently held after the last hop (or `base_mint` if no hops).
    pub fn current_mint(&self) -> TokenMint {
        self.hops.last().map(|h| h.output_mint).unwrap_or(self.base_mint)
    }

    /// Number of hops in the route.
    pub fn len(&self) -> usize {
        self.hops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hops.is_empty()
    }

    /// True if the route returns to the base mint and contains ≥ 1 hop.
    pub fn is_cycle(&self) -> bool {
        !self.hops.is_empty() && self.current_mint() == self.base_mint
    }

    /// Append a hop. Fails if `hop.input_mint` doesn't match the route's
    /// current token (would be an invalid swap chain).
    pub fn extend(&mut self, hop: Hop) -> ArbResult<()> {
        let expected = self.current_mint();
        if hop.input_mint != expected {
            return Err(ArbError::MintMismatch {
                expected: expected.pubkey(),
                actual:   hop.input_mint.pubkey(),
            });
        }
        self.hops.push(hop);
        Ok(())
    }

    /// Stable [`RouteId`] derived from the hop sequence. Uses Rust's default
    /// Hasher seeded deterministically (DefaultHasher, fixed initial state)
    /// so the id is consistent within a single process. The id format is not
    /// suitable for cross-version persistence; callers needing on-disk
    /// stability should hash the serialized route themselves.
    pub fn id(&self) -> RouteId {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut h = DefaultHasher::new();
        self.base_mint.hash(&mut h);
        for hop in &self.hops {
            hop.hash(&mut h);
        }
        RouteId(h.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn mk_mint(seed: u8) -> TokenMint {
        TokenMint(Pubkey::new_from_array([seed; 32]))
    }
    fn mk_pool(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    #[test]
    fn test_route_new_starts_at_base() {
        let usdc = mk_mint(1);
        let r = Route::new(usdc);
        assert!(r.is_empty());
        assert_eq!(r.current_mint(), usdc);
        assert!(!r.is_cycle());
    }

    #[test]
    fn test_route_extend_appends_hop() {
        let usdc = mk_mint(1);
        let sol  = mk_mint(2);
        let mut r = Route::new(usdc);
        r.extend(Hop::new(mk_pool(10), usdc, sol)).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r.current_mint(), sol);
    }

    #[test]
    fn test_route_extend_mismatched_token_fails() {
        let usdc = mk_mint(1);
        let sol  = mk_mint(2);
        let usdt = mk_mint(3);
        let mut r = Route::new(usdc);
        // First hop is fine: USDC → SOL.
        r.extend(Hop::new(mk_pool(10), usdc, sol)).unwrap();
        // Second hop tries to swap USDT (which we don't hold) and must fail.
        let err = r.extend(Hop::new(mk_pool(11), usdt, usdc)).unwrap_err();
        assert!(matches!(err, ArbError::MintMismatch { .. }));
        // Route must remain unchanged after the failed extend.
        assert_eq!(r.len(), 1);
        assert_eq!(r.current_mint(), sol);
    }

    #[test]
    fn test_route_is_cycle_after_returning_to_base() {
        let usdc = mk_mint(1);
        let sol  = mk_mint(2);
        let mut r = Route::new(usdc);
        r.extend(Hop::new(mk_pool(10), usdc, sol)).unwrap();
        assert!(!r.is_cycle()); // open mid-flight
        r.extend(Hop::new(mk_pool(11), sol, usdc)).unwrap();
        assert!(r.is_cycle());
    }

    #[test]
    fn test_route_id_distinct_for_distinct_hops() {
        let usdc = mk_mint(1);
        let sol  = mk_mint(2);
        let mut a = Route::new(usdc);
        a.extend(Hop::new(mk_pool(10), usdc, sol)).unwrap();
        a.extend(Hop::new(mk_pool(11), sol, usdc)).unwrap();

        let mut b = Route::new(usdc);
        // Different second pool → different route → different id.
        b.extend(Hop::new(mk_pool(10), usdc, sol)).unwrap();
        b.extend(Hop::new(mk_pool(99), sol, usdc)).unwrap();

        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn test_route_id_order_sensitive() {
        // Reversing the hop order changes the id even when the hop *set* is
        // identical, because direction matters for arb cycles.
        let usdc = mk_mint(1);
        let sol  = mk_mint(2);
        let mut a = Route::new(usdc);
        a.extend(Hop::new(mk_pool(10), usdc, sol)).unwrap();
        a.extend(Hop::new(mk_pool(11), sol, usdc)).unwrap();

        let mut b = Route::new(usdc);
        b.extend(Hop::new(mk_pool(11), usdc, sol)).unwrap();
        b.extend(Hop::new(mk_pool(10), sol, usdc)).unwrap();

        assert_ne!(a.id(), b.id());
    }

    proptest! {
        #[test]
        fn prop_route_id_deterministic(
            seeds in proptest::collection::vec(0u8..=255u8, 0..6)
        ) {
            // Build the same route twice from the same seeds; ids must match.
            // This detects accidental non-determinism (e.g. random hash seeds).
            let usdc = mk_mint(1);
            let sol  = mk_mint(2);
            let build = |seeds: &[u8]| -> Route {
                let mut r = Route::new(usdc);
                let mut at_base = true;
                for s in seeds {
                    let (ip, op) = if at_base { (usdc, sol) } else { (sol, usdc) };
                    r.extend(Hop::new(mk_pool(*s), ip, op)).unwrap();
                    at_base = !at_base;
                }
                r
            };
            let a = build(&seeds);
            let b = build(&seeds);
            prop_assert_eq!(a.id(), b.id());
        }
    }
}
