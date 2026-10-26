use serde::{Deserialize, Serialize};
use solana_program::pubkey::Pubkey;

/// A token mint, wrapped in a newtype so we can't accidentally swap it with a
/// pool address or wallet pubkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenMint(pub Pubkey);

impl TokenMint {
    pub const fn new(pk: Pubkey) -> Self {
        Self(pk)
    }
    pub fn pubkey(&self) -> Pubkey {
        self.0
    }
}

impl std::fmt::Display for TokenMint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An amount in a specific token's base units. Carrying `decimals` and `mint`
/// alongside `amount` prevents the classic mistake of adding 6-decimal USDC to
/// 9-decimal SOL or treating a UI value as a base-unit value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenAmount {
    pub mint:     TokenMint,
    pub amount:   u64,
    pub decimals: u8,
}

impl TokenAmount {
    pub const fn new(mint: TokenMint, amount: u64, decimals: u8) -> Self {
        Self { mint, amount, decimals }
    }

    /// Returns `Some` only if the two amounts share the same mint AND decimals.
    /// Returns `None` on mint mismatch, decimals mismatch, or arithmetic overflow.
    pub fn checked_add(self, other: Self) -> Option<Self> {
        if self.mint != other.mint || self.decimals != other.decimals {
            return None;
        }
        Some(Self {
            mint:     self.mint,
            amount:   self.amount.checked_add(other.amount)?,
            decimals: self.decimals,
        })
    }

    pub fn checked_sub(self, other: Self) -> Option<Self> {
        if self.mint != other.mint || self.decimals != other.decimals {
            return None;
        }
        Some(Self {
            mint:     self.mint,
            amount:   self.amount.checked_sub(other.amount)?,
            decimals: self.decimals,
        })
    }

    /// Convert to a floating-point UI amount (`amount / 10^decimals`).
    /// Lossy beyond `u64` range, but adequate for human-readable displays.
    pub fn ui_amount(&self) -> f64 {
        self.amount as f64 / 10f64.powi(self.decimals as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn mint_a() -> TokenMint {
        TokenMint(Pubkey::new_from_array([1u8; 32]))
    }
    fn mint_b() -> TokenMint {
        TokenMint(Pubkey::new_from_array([2u8; 32]))
    }

    #[test]
    fn test_token_amount_add_normal() {
        let a = TokenAmount::new(mint_a(), 100, 6);
        let b = TokenAmount::new(mint_a(), 250, 6);
        let s = a.checked_add(b).unwrap();
        assert_eq!(s.amount, 350);
        assert_eq!(s.mint, mint_a());
        assert_eq!(s.decimals, 6);
    }

    #[test]
    fn test_token_amount_add_different_mints_fails() {
        let a = TokenAmount::new(mint_a(), 100, 6);
        let b = TokenAmount::new(mint_b(), 100, 6);
        assert!(a.checked_add(b).is_none());
    }

    #[test]
    fn test_token_amount_add_different_decimals_fails() {
        let a = TokenAmount::new(mint_a(), 100, 6);
        let b = TokenAmount::new(mint_a(), 100, 9);
        assert!(a.checked_add(b).is_none());
    }

    #[test]
    fn test_token_amount_overflow_returns_none() {
        let a = TokenAmount::new(mint_a(), u64::MAX, 6);
        let b = TokenAmount::new(mint_a(), 1, 6);
        assert!(a.checked_add(b).is_none());
    }

    #[test]
    fn test_token_amount_sub_underflow_returns_none() {
        let a = TokenAmount::new(mint_a(), 5, 6);
        let b = TokenAmount::new(mint_a(), 10, 6);
        assert!(a.checked_sub(b).is_none());
    }

    #[test]
    fn test_token_amount_ui_amount_decimals() {
        // 1 USDC == 1_000_000 base units at 6 decimals.
        let usdc = TokenAmount::new(mint_a(), 1_000_000, 6);
        assert!((usdc.ui_amount() - 1.0).abs() < 1e-9);
        // 0.5 SOL at 9 decimals.
        let sol = TokenAmount::new(mint_a(), 500_000_000, 9);
        assert!((sol.ui_amount() - 0.5).abs() < 1e-9);
    }

    proptest! {
        #[test]
        fn prop_token_amount_add_commutative(a in 0u64..u64::MAX/2, b in 0u64..u64::MAX/2) {
            let x = TokenAmount::new(mint_a(), a, 6);
            let y = TokenAmount::new(mint_a(), b, 6);
            prop_assert_eq!(x.checked_add(y), y.checked_add(x));
        }
    }
}
