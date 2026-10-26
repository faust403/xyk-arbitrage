//! OpenBook (formerly Serum) market account parser.
//!
//! Raydium AMM v4 pools delegate orderbook execution to an OpenBook market.
//! To build a native swap instruction we need accounts that live INSIDE
//! that market — bids, asks, event queue, the market's own vaults, and
//! the vault-signer PDA. None of these are in the Raydium pool state, so
//! we fetch the OpenBook market account once at startup and cache the
//! derived `OpenBookMarketAccounts` per market id.
//!
//! ## Account layout (Serum v3 / OpenBook v1)
//!
//! The on-chain account begins with a 5-byte ASCII prefix `"serum"` and
//! ends with 7 bytes of padding. All offsets below INCLUDE the prefix.
//!
//! | Field                     | Offset | Type   |
//! |---------------------------|--------|--------|
//! | account_flags             | 5      | u64    |
//! | own_address               | 13     | Pubkey |
//! | vault_signer_nonce        | 45     | u64    |
//! | coin_mint                 | 53     | Pubkey |
//! | pc_mint                   | 85     | Pubkey |
//! | coin_vault                | 117    | Pubkey |
//! | (deposits/fees accrued)   | 149    | u64×2  |
//! | pc_vault                  | 165    | Pubkey |
//! | (deposits/fees accrued)   | 197    | u64×2  |
//! | (pc_dust_threshold)       | 213    | u64    |
//! | request_queue             | 221    | Pubkey |
//! | event_queue               | 253    | Pubkey |
//! | bids                      | 285    | Pubkey |
//! | asks                      | 317    | Pubkey |
//!
//! Total account size: 388 bytes.

use arb_core::{ArbError, ArbResult};
use solana_program::pubkey::Pubkey;

pub const OPENBOOK_MARKET_ACCOUNT_SIZE: usize = 388;

const PREFIX_LEN:               usize = 5; // ascii "serum"
const OFFSET_VAULT_SIGNER_NONCE:usize = 45;
const OFFSET_COIN_VAULT:        usize = 117;
const OFFSET_PC_VAULT:          usize = 165;
const OFFSET_EVENT_QUEUE:       usize = 253;
const OFFSET_BIDS:              usize = 285;
const OFFSET_ASKS:              usize = 317;

/// All accounts a Raydium swap instruction needs FROM the OpenBook market,
/// already de-referenced (so the executor doesn't need a second fetch).
#[derive(Debug, Clone, Copy)]
pub struct OpenBookMarketAccounts {
    pub bids:           Pubkey,
    pub asks:           Pubkey,
    pub event_queue:    Pubkey,
    pub coin_vault:     Pubkey,
    pub pc_vault:       Pubkey,
    /// PDA derived as `create_program_address(seeds=[market, nonce_le], program=serum_program)`.
    pub vault_signer:   Pubkey,
}

/// Parse an OpenBook market account and derive its `vault_signer`.
///
/// `market` is the address of the account being parsed (needed as seed
/// for vault-signer PDA derivation). `serum_program` is the OpenBook /
/// legacy-Serum program ID (read from the Raydium pool's `market_program`
/// field — different pools may use different versions).
pub fn parse_openbook_market(
    data:           &[u8],
    market:         &Pubkey,
    serum_program:  &Pubkey,
) -> ArbResult<OpenBookMarketAccounts> {
    if data.len() != OPENBOOK_MARKET_ACCOUNT_SIZE {
        return Err(ArbError::AccountParse {
            dex: "openbook-market",
            reason: format!("expected {OPENBOOK_MARKET_ACCOUNT_SIZE} bytes, got {}", data.len()),
        });
    }
    if &data[..PREFIX_LEN] != b"serum" {
        return Err(ArbError::AccountParse {
            dex: "openbook-market",
            reason: format!("missing 'serum' prefix; got {:?}", &data[..PREFIX_LEN]),
        });
    }

    let read_pk = |off: usize| -> Pubkey {
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&data[off..off + 32]);
        Pubkey::new_from_array(buf)
    };
    let read_u64 = |off: usize| -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&data[off..off + 8]);
        u64::from_le_bytes(buf)
    };

    let nonce = read_u64(OFFSET_VAULT_SIGNER_NONCE);
    // Serum's vault-signer PDA uses `create_program_address` (not find), since
    // the nonce is stored in the market itself. The seeds are the market
    // address bytes followed by the nonce as 8 little-endian bytes.
    let vault_signer = Pubkey::create_program_address(
        &[market.as_ref(), &nonce.to_le_bytes()],
        serum_program,
    ).map_err(|e| ArbError::AccountParse {
        dex: "openbook-market",
        reason: format!("create_program_address failed for vault_signer (nonce={nonce}): {e:?}"),
    })?;

    Ok(OpenBookMarketAccounts {
        bids:         read_pk(OFFSET_BIDS),
        asks:         read_pk(OFFSET_ASKS),
        event_queue:  read_pk(OFFSET_EVENT_QUEUE),
        coin_vault:   read_pk(OFFSET_COIN_VAULT),
        pc_vault:     read_pk(OFFSET_PC_VAULT),
        vault_signer,
    })
}

/// In-memory store for parsed OpenBook markets. Keyed by market address.
/// Markets are static — we fetch once at bootstrap and never refresh.
pub struct OpenBookMarketStore {
    inner: dashmap::DashMap<Pubkey, OpenBookMarketAccounts>,
}

impl OpenBookMarketStore {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { inner: dashmap::DashMap::new() })
    }
    pub fn insert(&self, market: Pubkey, accs: OpenBookMarketAccounts) {
        self.inner.insert(market, accs);
    }
    pub fn get(&self, market: &Pubkey) -> Option<OpenBookMarketAccounts> {
        self.inner.get(market).map(|r| *r)
    }
    pub fn len(&self) -> usize { self.inner.len() }
    pub fn is_empty(&self) -> bool { self.inner.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_market(coin_vault: Pubkey, pc_vault: Pubkey, ev_q: Pubkey,
                   bids: Pubkey, asks: Pubkey, nonce: u64) -> Vec<u8> {
        let mut data = vec![0u8; OPENBOOK_MARKET_ACCOUNT_SIZE];
        data[..PREFIX_LEN].copy_from_slice(b"serum");
        data[OFFSET_VAULT_SIGNER_NONCE..OFFSET_VAULT_SIGNER_NONCE + 8]
            .copy_from_slice(&nonce.to_le_bytes());
        data[OFFSET_COIN_VAULT..OFFSET_COIN_VAULT + 32].copy_from_slice(&coin_vault.to_bytes());
        data[OFFSET_PC_VAULT..OFFSET_PC_VAULT + 32].copy_from_slice(&pc_vault.to_bytes());
        data[OFFSET_EVENT_QUEUE..OFFSET_EVENT_QUEUE + 32].copy_from_slice(&ev_q.to_bytes());
        data[OFFSET_BIDS..OFFSET_BIDS + 32].copy_from_slice(&bids.to_bytes());
        data[OFFSET_ASKS..OFFSET_ASKS + 32].copy_from_slice(&asks.to_bytes());
        data
    }

    /// Pubkey + nonce that we know forms a valid PDA. Found by trial: any
    /// `create_program_address` call with a nonce that misses the curve
    /// returns Err, so the test fixture has to use known-good values.
    fn known_good_market_and_nonce(serum_program: &Pubkey) -> (Pubkey, u8) {
        // Sweep nonces until we find one that lands off-curve. Same trick
        // Serum's market-create code uses to pick the nonce.
        let market = Pubkey::new_from_array([42u8; 32]);
        for n in 0..=255u8 {
            if Pubkey::create_program_address(
                &[market.as_ref(), &(n as u64).to_le_bytes()],
                serum_program,
            ).is_ok() {
                return (market, n);
            }
        }
        panic!("no valid nonce found in [0,255] — astronomically unlikely")
    }

    #[test]
    fn parse_extracts_correct_pubkeys() {
        let serum_program = Pubkey::new_from_array([99u8; 32]);
        let (market, nonce) = known_good_market_and_nonce(&serum_program);
        let cv = Pubkey::new_from_array([1u8; 32]);
        let pv = Pubkey::new_from_array([2u8; 32]);
        let eq = Pubkey::new_from_array([3u8; 32]);
        let bd = Pubkey::new_from_array([4u8; 32]);
        let ak = Pubkey::new_from_array([5u8; 32]);
        let data = fake_market(cv, pv, eq, bd, ak, nonce as u64);

        let accs = parse_openbook_market(&data, &market, &serum_program).expect("parse");
        assert_eq!(accs.coin_vault,  cv);
        assert_eq!(accs.pc_vault,    pv);
        assert_eq!(accs.event_queue, eq);
        assert_eq!(accs.bids,        bd);
        assert_eq!(accs.asks,        ak);
    }

    #[test]
    fn rejects_wrong_size() {
        let r = parse_openbook_market(&[0u8; 100], &Pubkey::default(), &Pubkey::default());
        assert!(matches!(r, Err(ArbError::AccountParse { .. })));
    }

    #[test]
    fn rejects_missing_prefix() {
        let mut data = vec![0u8; OPENBOOK_MARKET_ACCOUNT_SIZE];
        data[..5].copy_from_slice(b"badxx");
        let r = parse_openbook_market(&data, &Pubkey::default(), &Pubkey::default());
        assert!(matches!(r, Err(ArbError::AccountParse { .. })));
    }

    #[test]
    fn vault_signer_is_deterministic() {
        let serum_program = Pubkey::new_from_array([99u8; 32]);
        let (market, nonce) = known_good_market_and_nonce(&serum_program);
        let data = fake_market(
            Pubkey::default(), Pubkey::default(), Pubkey::default(),
            Pubkey::default(), Pubkey::default(), nonce as u64,
        );
        let accs1 = parse_openbook_market(&data, &market, &serum_program).unwrap();
        let accs2 = parse_openbook_market(&data, &market, &serum_program).unwrap();
        assert_eq!(accs1.vault_signer, accs2.vault_signer);
    }

    #[test]
    fn store_round_trip() {
        let store = OpenBookMarketStore::new();
        let m = Pubkey::new_from_array([7u8; 32]);
        let accs = OpenBookMarketAccounts {
            bids: Pubkey::new_from_array([1; 32]),
            asks: Pubkey::new_from_array([2; 32]),
            event_queue: Pubkey::new_from_array([3; 32]),
            coin_vault: Pubkey::new_from_array([4; 32]),
            pc_vault: Pubkey::new_from_array([5; 32]),
            vault_signer: Pubkey::new_from_array([6; 32]),
        };
        store.insert(m, accs);
        let got = store.get(&m).expect("present");
        assert_eq!(got.bids, accs.bids);
        assert_eq!(got.vault_signer, accs.vault_signer);
        assert_eq!(store.len(), 1);
    }
}
