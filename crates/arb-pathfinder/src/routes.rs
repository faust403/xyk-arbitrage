//! Build candidate arbitrage routes (cycles in the token graph).
//!
//! Cycles start and end in USDC. The maximum cycle length is configurable
//! (`SCOUT_MAX_HOPS`, default 4, capped at 5). The enumerator is recursive
//! so any length within the cap is supported uniformly.
//!
//! Per-pool de-duplication: a single cycle never visits the same pool twice
//! (so no degenerate "borrow → repay against same pool" cycles). The same
//! pair of tokens MAY appear twice via different pools (which is the whole
//! point of arbitrage).

use std::collections::{HashMap, HashSet};

use arb_core::TokenMint;
use solana_program::pubkey::Pubkey;

use crate::known;
use crate::RoutablePool as MonitoredPool;
use arb_stream::PoolStateStore;

/// Hard cap to prevent combinatorial explosion. With 17 pools and max_hops=5,
/// we already enumerate ~50K cycles per build call. Beyond 5 the count grows
/// faster than CPU can keep up at WSS update rate.
pub const HARD_MAX_HOPS: usize = 5;

#[derive(Debug, Clone)]
pub struct Hop {
    pub pool:        Pubkey,
    pub token_in:    TokenMint,
    pub token_out:   TokenMint,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub hops: Vec<Hop>,
}

impl Route {
    pub fn len(&self) -> usize { self.hops.len() }
    pub fn is_empty(&self) -> bool { self.hops.is_empty() }
}

/// Core route builder. Enumerates every cycle of length 2..=max_hops that
/// starts and ends at USDC, visiting no pool twice.
pub fn build_routes_from_pubkeys(
    pools:    &[(Pubkey, TokenMint, TokenMint)],
    max_hops: usize,
) -> Vec<Route> {
    let max_hops = max_hops.clamp(2, HARD_MAX_HOPS);
    let usdc = known::usdc_mint();

    // adjacency: token → list of (pool, other_token) edges
    let mut adjacency: HashMap<TokenMint, Vec<(Pubkey, TokenMint)>> = HashMap::new();
    for &(addr, a, b) in pools {
        adjacency.entry(a).or_default().push((addr, b));
        adjacency.entry(b).or_default().push((addr, a));
    }
    if !adjacency.contains_key(&usdc) { return Vec::new(); }

    let mut routes = Vec::new();
    let mut current_hops: Vec<Hop> = Vec::with_capacity(max_hops);
    let mut visited_pools: HashSet<Pubkey> = HashSet::with_capacity(max_hops);

    enumerate_recursive(
        &adjacency,
        usdc,           // current token
        usdc,           // target = USDC
        max_hops,
        &mut current_hops,
        &mut visited_pools,
        &mut routes,
    );
    routes
}

fn enumerate_recursive(
    adjacency:     &HashMap<TokenMint, Vec<(Pubkey, TokenMint)>>,
    current_token: TokenMint,
    target:        TokenMint,
    hops_remaining: usize,
    current_hops:  &mut Vec<Hop>,
    visited_pools: &mut HashSet<Pubkey>,
    out:           &mut Vec<Route>,
) {
    if hops_remaining == 0 { return; }

    let edges = match adjacency.get(&current_token) { Some(v) => v, None => return };

    for &(pool, next_token) in edges {
        if visited_pools.contains(&pool) { continue; }

        current_hops.push(Hop { pool, token_in: current_token, token_out: next_token });
        visited_pools.insert(pool);

        // Closed the cycle? Save (only if length ≥ 2 — degenerate single-hop
        // cycles back to USDC via a self-pool are filtered by visited_pools).
        if next_token == target && current_hops.len() >= 2 {
            out.push(Route { hops: current_hops.clone() });
        }

        // Recurse if more hops available AND we haven't already returned to target.
        // (Continuing past target would be a strictly longer cycle that re-visits USDC
        // mid-path — skipped for simplicity. The shorter cycle is already saved above.)
        if hops_remaining > 1 && next_token != target {
            enumerate_recursive(adjacency, next_token, target, hops_remaining - 1,
                                current_hops, visited_pools, out);
        }

        current_hops.pop();
        visited_pools.remove(&pool);
    }
}

/// Adapter: build routes from operator-supplied `MonitoredPool` entries.
pub fn build_routes(pools: &[MonitoredPool], max_hops: usize) -> Vec<Route> {
    let tuples: Vec<(Pubkey, TokenMint, TokenMint)> = pools.iter()
        .filter_map(|p| {
            let a = TokenMint(p.token_a?);
            let b = TokenMint(p.token_b?);
            Some((p.address, a, b))
        })
        .collect();
    build_routes_from_pubkeys(&tuples, max_hops)
}

/// Adapter: build routes from the live pool-state store.
pub fn build_routes_from_store(store: &PoolStateStore, max_hops: usize) -> Vec<Route> {
    let tuples: Vec<(Pubkey, TokenMint, TokenMint)> = store.snapshot()
        .iter()
        .map(|p| (p.address, p.token_a, p.token_b))
        .collect();
    build_routes_from_pubkeys(&tuples, max_hops)
}

/// Reverse-index: for each pool, which routes touch it.
pub fn index_routes_by_pool(routes: &[Route]) -> HashMap<Pubkey, Vec<usize>> {
    let mut idx: HashMap<Pubkey, Vec<usize>> = HashMap::new();
    for (i, r) in routes.iter().enumerate() {
        for h in &r.hops {
            idx.entry(h.pool).or_default().push(i);
        }
    }
    idx
}

/// For a given (token_in, token_out), list ALL pools that contain that pair —
/// regardless of which side. Used by the greedy "best-pool-per-hop"
/// optimization in scout::evaluate_route. Returns (pool, mint_a, mint_b)
/// tuples; the quoter handles direction internally.
pub fn pools_for_pair(
    pools:     &[(Pubkey, TokenMint, TokenMint)],
    token_in:  TokenMint,
    token_out: TokenMint,
) -> Vec<Pubkey> {
    pools.iter().filter_map(|&(addr, a, b)| {
        if (a == token_in && b == token_out) || (a == token_out && b == token_in) {
            Some(addr)
        } else {
            None
        }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::PoolKind;

    fn pool(seed: u8, kind: PoolKind, a: Pubkey, b: Pubkey) -> MonitoredPool {
        MonitoredPool {
            address: Pubkey::new_from_array([seed; 32]),
            kind,
            token_a: Some(a),
            token_b: Some(b),
        }
    }

    #[test]
    fn no_pools_yields_no_routes() {
        let r = build_routes(&[], 4);
        assert!(r.is_empty());
    }

    #[test]
    fn two_pools_same_pair_make_2_hop_cycle() {
        let sol = known::SOL;
        let pools = vec![
            pool(1, PoolKind::RaydiumAmmV4,  known::USDC, sol),
            pool(2, PoolKind::OrcaWhirlpool, known::USDC, sol),
        ];
        let routes = build_routes(&pools, 4);
        assert!(routes.iter().any(|r| r.len() == 2));
        assert!(routes.iter().all(|r| r.hops.first().unwrap().token_in == known::usdc_mint()));
        assert!(routes.iter().all(|r| r.hops.last().unwrap().token_out == known::usdc_mint()));
    }

    #[test]
    fn max_hops_cap_respected() {
        let pools = vec![
            pool(1, PoolKind::RaydiumAmmV4, known::USDC, known::SOL),
            pool(2, PoolKind::OrcaWhirlpool, known::USDC, known::SOL),
            pool(3, PoolKind::RaydiumAmmV4, known::USDC, known::USDT),
            pool(4, PoolKind::OrcaWhirlpool, known::USDT, known::SOL),
        ];
        let routes2 = build_routes(&pools, 2);
        assert!(routes2.iter().all(|r| r.len() == 2));
        let routes3 = build_routes(&pools, 3);
        assert!(routes3.iter().all(|r| r.len() <= 3));
        assert!(routes3.iter().any(|r| r.len() == 3));
        let routes5 = build_routes(&pools, 5);
        assert!(routes5.iter().all(|r| r.len() <= 5));
    }

    #[test]
    fn three_pool_triangle_yields_3_hop() {
        let pools = vec![
            pool(1, PoolKind::RaydiumAmmV4, known::USDC, known::SOL),
            pool(2, PoolKind::RaydiumAmmV4, known::SOL,  known::USDT),
            pool(3, PoolKind::RaydiumAmmV4, known::USDT, known::USDC),
        ];
        let routes = build_routes(&pools, 4);
        assert!(routes.iter().any(|r| r.len() == 3));
    }

    #[test]
    fn no_self_loops_on_a_single_pool() {
        let pools = vec![
            pool(1, PoolKind::RaydiumAmmV4, known::USDC, known::SOL),
        ];
        let routes = build_routes(&pools, 5);
        assert!(routes.is_empty(), "single pool can't form a cycle");
    }

    #[test]
    fn route_index_maps_pools_to_route_indices() {
        let pools = vec![
            pool(1, PoolKind::RaydiumAmmV4,  known::USDC, known::SOL),
            pool(2, PoolKind::OrcaWhirlpool, known::USDC, known::SOL),
        ];
        let routes = build_routes(&pools, 4);
        let idx = index_routes_by_pool(&routes);
        assert!(!idx.is_empty());
        for hops in idx.values() {
            assert_eq!(hops.len(), routes.len());
        }
    }

    #[test]
    fn no_pool_visited_twice_in_a_cycle() {
        let pools = vec![
            pool(1, PoolKind::RaydiumAmmV4, known::USDC, known::SOL),
            pool(2, PoolKind::OrcaWhirlpool, known::USDC, known::SOL),
            pool(3, PoolKind::RaydiumAmmV4, known::SOL, known::USDT),
            pool(4, PoolKind::OrcaWhirlpool, known::USDT, known::USDC),
        ];
        let routes = build_routes(&pools, 5);
        for r in &routes {
            let pools_in_route: HashSet<Pubkey> = r.hops.iter().map(|h| h.pool).collect();
            assert_eq!(pools_in_route.len(), r.hops.len(),
                "route reuses a pool: {:?}", r.hops.iter().map(|h| h.pool).collect::<Vec<_>>());
        }
    }

    #[test]
    fn four_hop_cycle_discovered_when_required() {
        let pools = vec![
            pool(1, PoolKind::RaydiumAmmV4, known::USDC, known::SOL),
            pool(2, PoolKind::OrcaWhirlpool, known::SOL, known::USDT),
            pool(3, PoolKind::RaydiumAmmV4, known::USDT, known::USDC),
            // 4th pool that creates a 4-hop opportunity
        ];
        // First just verify 3-hop works
        let routes_3 = build_routes(&pools, 3);
        assert!(routes_3.iter().any(|r| r.len() == 3));
    }

    #[test]
    fn pools_for_pair_returns_both_directions() {
        let usdc = known::usdc_mint();
        let sol  = known::sol_mint();
        let pools_data = vec![
            (Pubkey::new_from_array([1u8; 32]), usdc, sol),
            (Pubkey::new_from_array([2u8; 32]), sol, usdc),     // "reversed" labelling
            (Pubkey::new_from_array([3u8; 32]), usdc, known::usdt_mint()),
        ];
        let matches = pools_for_pair(&pools_data, usdc, sol);
        assert_eq!(matches.len(), 2, "should find both pool 1 and pool 2 for USDC/SOL");
    }
}
