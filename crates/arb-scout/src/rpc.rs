//! Minimal Helius JSON-RPC client used by the scout.
//!
//! Only what we need: `getMultipleAccounts` for bootstrap (initial state
//! population at startup and on every WSS reconnect, per INSTRUCTIONS.md
//! Phase 6 reconnect rule).
//!
//! We intentionally don't pull in `solana-client` here — its blocking
//! `RpcClient` doesn't compose well with our async loop, and `nonblocking`
//! brings in a large dependency tree we don't need for one method.

/// Build the Helius RPC URL for the given api_key. Host is overridable via
/// the `HELIUS_RPC_HOST` env var so operators can flip between
/// `mainnet.helius-rpc.com` (default Cloudflare path) and
/// `beta.helius-rpc.com` (Gatekeeper edge gateway, ~10× faster median per
/// 2026-05-17 benchmark). Single point of control means every binary —
/// arb-scout, kamino-hunter, marginfi-hunter, pumpswap-watcher — flips
/// together when the env is set.
pub fn helius_rpc_url(api_key: &str) -> String {
    let host = std::env::var("HELIUS_RPC_HOST")
        .unwrap_or_else(|_| "mainnet.helius-rpc.com".to_string());
    format!("https://{host}/?api-key={api_key}")
}

use arb_core::{ArbError, ArbResult};
use base64::Engine;
use serde_json::{json, Value};
use solana_program::pubkey::Pubkey;

#[derive(Debug, Clone)]
pub struct AccountSnapshot {
    pub pubkey: Pubkey,
    pub owner:  Pubkey,
    pub data:   Vec<u8>,
    pub slot:   u64,
}

/// Minimal view of one transaction inside a fetched block. Only the parts the
/// Jito correlator needs: which accounts it touched and which instructions
/// it ran (so we can spot SystemProgram::Transfer to a Jito tip account).
#[derive(Debug, Clone)]
pub struct BlockTransaction {
    pub account_keys: Vec<Pubkey>,
    pub instructions: Vec<BlockInstruction>,
    pub failed:       bool,
    pub fee_lamports: u64,
}

#[derive(Debug, Clone)]
pub struct BlockInstruction {
    pub program_id_index: u8,
    pub account_indices:  Vec<u8>,
    pub data:             Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct FetchedBlock {
    pub slot:         u64,
    pub block_height: Option<u64>,
    pub transactions: Vec<BlockTransaction>,
}

/// Output of `simulate_transaction` — just enough for Smart-C to decide
/// whether to flip the token program on the ATA-create IX.
#[derive(Debug, Clone)]
pub struct SimulateOutcome {
    pub err_is_null: bool,
    pub logs:        Vec<String>,
}

#[derive(Clone)]
pub struct HeliusRpc {
    url:  String,
    http: reqwest::Client,
}

impl HeliusRpc {
    pub fn new(url: String) -> Self {
        Self { url, http: reqwest::Client::new() }
    }

    /// Submit a fully-signed base64 transaction to the network. Returns the
    /// transaction signature. `skip_preflight=true` skips the simulator
    /// (faster, but you may submit txs that fail on chain). Use `false` for
    /// safety; `true` only when you've already simulated.
    pub async fn send_transaction(&self, signed_tx_b64: &str, skip_preflight: bool) -> ArbResult<String> {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "sendTransaction",
            "params": [
                signed_tx_b64,
                { "encoding": "base64", "skipPreflight": skip_preflight,
                  "preflightCommitment": "confirmed", "maxRetries": 0 }
            ],
        });
        let resp: Value = self.http.post(&self.url).json(&body).send().await
            .map_err(|e| ArbError::Rpc(format!("sendTransaction send: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("sendTransaction parse: {e}")))?;
        if let Some(err) = resp.get("error") {
            return Err(ArbError::Rpc(format!("sendTransaction error: {err}")));
        }
        resp.get("result").and_then(|v| v.as_str()).map(|s| s.to_string())
            .ok_or_else(|| ArbError::Rpc(format!("sendTransaction: missing result: {resp}")))
    }

    /// Pre-flight `simulateTransaction`. Returns the program logs (used by
    /// the pump.fun Smart-C path to decide whether the classic-ATA build
    /// failed with "Invalid Mint" — meaning the mint is actually Token-2022
    /// — so the executor can rebuild with the right token program before
    /// burning the real send).
    pub async fn simulate_transaction(&self, signed_tx_b64: &str) -> ArbResult<SimulateOutcome> {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "simulateTransaction",
            "params": [
                signed_tx_b64,
                { "encoding": "base64", "sigVerify": false, "replaceRecentBlockhash": true,
                  "commitment": "processed" }
            ],
        });
        let resp: Value = self.http.post(&self.url).json(&body).send().await
            .map_err(|e| ArbError::Rpc(format!("simulateTransaction send: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("simulateTransaction parse: {e}")))?;
        if let Some(err) = resp.get("error") {
            return Err(ArbError::Rpc(format!("simulateTransaction RPC error: {err}")));
        }
        let value = resp.pointer("/result/value")
            .ok_or_else(|| ArbError::Rpc("simulateTransaction: missing result.value".into()))?;
        let err = value.get("err").cloned().unwrap_or(Value::Null);
        let logs = value.get("logs").and_then(|l| l.as_array()).cloned().unwrap_or_default();
        let logs: Vec<String> = logs.into_iter().filter_map(|l| l.as_str().map(String::from)).collect();
        Ok(SimulateOutcome { err_is_null: err.is_null(), logs })
    }

    /// Fetch the addresses stored in an Address Lookup Table account. The
    /// ALT account layout (per Solana docs):
    ///   0-56: discriminator + deactivation slot + last_extended_slot + bumps
    ///   56-: 32-byte pubkeys (variable count, up to 256)
    /// Returns up to 256 pubkeys.
    pub async fn fetch_alt_addresses(&self, alt: &solana_program::pubkey::Pubkey)
        -> ArbResult<Vec<solana_program::pubkey::Pubkey>>
    {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
            "params": [alt.to_string(), {"encoding": "base64", "commitment": "confirmed"}],
        });
        let resp: Value = self.http.post(&self.url).json(&body).send().await
            .map_err(|e| ArbError::Rpc(format!("fetch_alt send: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("fetch_alt parse: {e}")))?;
        let data_b64 = resp.pointer("/result/value/data/0")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ArbError::Rpc(format!("fetch_alt: no data: {resp}")))?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| ArbError::Rpc(format!("fetch_alt base64: {e}")))?;
        if data.len() < 56 { return Ok(vec![]); }
        // Addresses start at offset 56, each 32 bytes
        let addr_section = &data[56..];
        let n = addr_section.len() / 32;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let off = i * 32;
            let pk_bytes: [u8; 32] = addr_section[off..off+32].try_into()
                .map_err(|_| ArbError::Invariant("fetch_alt: chunk size"))?;
            out.push(solana_program::pubkey::Pubkey::new_from_array(pk_bytes));
        }
        Ok(out)
    }

    /// List all SPL token accounts owned by `owner`. Returns Vec<(ata_pubkey,
    /// mint, balance_base_units)>. Used by the sell-all-pump-holdings flow
    /// to identify positions worth liquidating back to SOL.
    pub async fn get_token_accounts_by_owner(
        &self,
        owner: &solana_program::pubkey::Pubkey,
    ) -> ArbResult<Vec<(solana_program::pubkey::Pubkey, solana_program::pubkey::Pubkey, u64)>> {
        use std::str::FromStr;
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "getTokenAccountsByOwner",
            "params": [owner.to_string(),
                {"programId": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"},
                {"encoding": "jsonParsed", "commitment": "confirmed"}],
        });
        let resp: Value = self.http.post(&self.url).json(&body).send().await
            .map_err(|e| ArbError::Rpc(format!("getTokenAccountsByOwner send: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("getTokenAccountsByOwner parse: {e}")))?;
        let arr = resp.pointer("/result/value")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ArbError::Rpc("getTokenAccountsByOwner: no value array".into()))?;
        let mut out = Vec::with_capacity(arr.len());
        for acc in arr {
            let ata_str = acc.get("pubkey").and_then(|v| v.as_str()).unwrap_or("");
            let info = acc.pointer("/account/data/parsed/info");
            let mint_str = info.and_then(|i| i.get("mint")).and_then(|v| v.as_str()).unwrap_or("");
            let amt_str = info.and_then(|i| i.pointer("/tokenAmount/amount"))
                .and_then(|v| v.as_str()).unwrap_or("0");
            if let (Ok(ata), Ok(mint), Ok(amt)) = (
                solana_program::pubkey::Pubkey::from_str(ata_str),
                solana_program::pubkey::Pubkey::from_str(mint_str),
                amt_str.parse::<u64>(),
            ) {
                if amt > 0 { out.push((ata, mint, amt)); }
            }
        }
        Ok(out)
    }

    /// Fetch SOL balance (lamports) for an account. Returns 0 if missing.
    /// Used by `run_wallet_watch` to detect sudden balance drops that
    /// suggest a compromise.
    pub async fn get_balance(&self, owner: &solana_program::pubkey::Pubkey) -> ArbResult<u64> {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "getBalance",
            "params": [owner.to_string(), {"commitment": "confirmed"}],
        });
        let resp: Value = self.http.post(&self.url).json(&body).send().await
            .map_err(|e| ArbError::Rpc(format!("getBalance send: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("getBalance parse: {e}")))?;
        let lamports = resp.pointer("/result/value")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Ok(lamports)
    }

    /// Fetch a token account's balance in base units. Used for sizing
    /// liquidations against the live USDC balance. Returns 0 if the ATA
    /// doesn't exist yet or the response is malformed.
    pub async fn get_token_account_balance(&self, ata: &solana_program::pubkey::Pubkey) -> ArbResult<u64> {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "getTokenAccountBalance",
            "params": [ata.to_string(), {"commitment": "confirmed"}],
        });
        let resp: Value = self.http.post(&self.url).json(&body).send().await
            .map_err(|e| ArbError::Rpc(format!("getTokenAccountBalance send: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("getTokenAccountBalance parse: {e}")))?;
        let amt_str = resp.pointer("/result/value/amount")
            .and_then(|v| v.as_str())
            .unwrap_or("0");
        Ok(amt_str.parse::<u64>().unwrap_or(0))
    }

    /// Fetch `getRecentPrioritizationFees` for a list of write-locked
    /// accounts. Returns the 75th-percentile prioritization fee
    /// (micro-lamports per CU) across non-zero samples from the last
    /// ~150 blocks. Pass the same accounts your tx will write-lock so
    /// the result reflects what competitors are paying for the same
    /// slot of attention.
    ///
    /// Returns `0` when no fee data is available (RPC empty response,
    /// all-zero history, or transport error). Callers should clamp to a
    /// sensible floor before using as cu_price.
    pub async fn get_priority_fee_p75(
        &self,
        write_locked_accounts: &[solana_program::pubkey::Pubkey],
    ) -> u64 {
        let addrs: Vec<String> = write_locked_accounts.iter()
            .map(|p| p.to_string()).collect();
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "getRecentPrioritizationFees",
            "params": [addrs],
        });
        let resp: Value = match self.http.post(&self.url).json(&body).send().await {
            Ok(r) => match r.json().await { Ok(v) => v, Err(_) => return 0 },
            Err(_) => return 0,
        };
        let arr = match resp.pointer("/result").and_then(|v| v.as_array()) {
            Some(a) => a, None => return 0,
        };
        let mut fees: Vec<u64> = arr.iter()
            .filter_map(|e| e.get("prioritizationFee").and_then(|v| v.as_u64()))
            .filter(|f| *f > 0)
            .collect();
        if fees.is_empty() { return 0; }
        fees.sort_unstable();
        // p75 — take the 75th index (or last if too small)
        let idx = (fees.len() * 3 / 4).min(fees.len() - 1);
        fees[idx]
    }

    /// Poll `getSignatureStatuses` for a single signature. Returns:
    /// - `Ok(Some(true))`  ⇒ confirmed (or finalized)
    /// - `Ok(Some(false))` ⇒ landed but failed
    /// - `Ok(None)`        ⇒ not yet seen by RPC
    /// - `Err(_)`          ⇒ transport / parse failure
    pub async fn signature_status(&self, sig: &str) -> ArbResult<Option<bool>> {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "getSignatureStatuses",
            "params": [[sig], { "searchTransactionHistory": false }],
        });
        let resp: Value = self.http.post(&self.url).json(&body).send().await
            .map_err(|e| ArbError::Rpc(format!("getSignatureStatuses send: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("getSignatureStatuses parse: {e}")))?;
        let entry = resp.pointer("/result/value/0");
        match entry {
            Some(Value::Null) | None => Ok(None),
            Some(v) => {
                let status = v.get("confirmationStatus").and_then(|s| s.as_str()).unwrap_or("");
                let succeeded = v.get("err").map(|e| e.is_null()).unwrap_or(true);
                if matches!(status, "confirmed" | "finalized") {
                    Ok(Some(succeeded))
                } else { Ok(None) }
            }
        }
    }

    /// Batch-fetch up to 100 accounts in one HTTP round trip.
    /// Returns one entry per requested address; missing accounts are `None`.
    pub async fn get_multiple_accounts(
        &self,
        addresses: &[Pubkey],
    ) -> ArbResult<Vec<Option<AccountSnapshot>>> {
        if addresses.is_empty() { return Ok(vec![]); }
        if addresses.len() > 100 {
            return Err(ArbError::Rpc(format!(
                "getMultipleAccounts limit is 100, got {}", addresses.len()
            )));
        }

        let addr_strings: Vec<String> = addresses.iter().map(|a| a.to_string()).collect();
        let body = json!({
            "jsonrpc": "2.0",
            "id":      1,
            "method":  "getMultipleAccounts",
            "params":  [
                addr_strings,
                { "encoding": "base64", "commitment": "confirmed" }
            ]
        });

        let resp: Value = self.http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ArbError::Rpc(format!("getMultipleAccounts send: {e}")))?
            .json()
            .await
            .map_err(|e| ArbError::Rpc(format!("getMultipleAccounts parse: {e}")))?;

        if let Some(err) = resp.get("error") {
            return Err(ArbError::Rpc(format!("RPC error: {err}")));
        }

        let result = resp.get("result").ok_or_else(|| ArbError::Rpc("missing result".into()))?;
        let slot = result.pointer("/context/slot").and_then(|s| s.as_u64()).unwrap_or(0);
        let value_arr = result.pointer("/value")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ArbError::Rpc("missing result.value array".into()))?;

        if value_arr.len() != addresses.len() {
            return Err(ArbError::Rpc(format!(
                "expected {} values, got {}", addresses.len(), value_arr.len()
            )));
        }

        let out: Vec<Option<AccountSnapshot>> = addresses.iter().zip(value_arr.iter()).map(|(addr, v)| {
            if v.is_null() { return None; }
            let owner_str = v.get("owner").and_then(|o| o.as_str())?;
            let owner = owner_str.parse::<Pubkey>().ok()?;
            let data_arr = v.get("data").and_then(|d| d.as_array())?;
            if data_arr.len() < 2 { return None; }
            let b64 = data_arr[0].as_str()?;
            let data = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
            Some(AccountSnapshot { pubkey: *addr, owner, data, slot })
        }).collect();

        Ok(out)
    }

    /// Fetch a confirmed block via `getBlock`. Returns `Ok(None)` if the slot
    /// was skipped (validator missed it) or not yet confirmed; `Err` only on
    /// transport / parse failures.
    ///
    /// Configures `transactionDetails: "full"` and `rewards: false` to keep
    /// the response size manageable while still giving us each transaction's
    /// account_keys + instructions.
    pub async fn get_block(&self, slot: u64) -> ArbResult<Option<FetchedBlock>> {
        let body = json!({
            "jsonrpc": "2.0",
            "id":      1,
            "method":  "getBlock",
            "params":  [
                slot,
                {
                    "encoding": "json",
                    "transactionDetails": "full",
                    "rewards": false,
                    "maxSupportedTransactionVersion": 0,
                    "commitment": "confirmed"
                }
            ]
        });

        let resp: Value = self.http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ArbError::Rpc(format!("getBlock send: {e}")))?
            .json()
            .await
            .map_err(|e| ArbError::Rpc(format!("getBlock parse: {e}")))?;

        if let Some(err) = resp.get("error") {
            // -32004 (block not available yet) and -32007 (slot was skipped)
            // are normal — return None so the caller can retry / skip.
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            if code == -32004 || code == -32007 || code == -32009 || code == -32014 {
                return Ok(None);
            }
            return Err(ArbError::Rpc(format!("RPC error: {err}")));
        }

        let result = match resp.get("result") {
            Some(r) if !r.is_null() => r,
            _ => return Ok(None),
        };

        let block_height = result.get("blockHeight").and_then(|v| v.as_u64());
        let txs_v = result.get("transactions").and_then(|t| t.as_array());
        let mut transactions = Vec::new();
        if let Some(txs) = txs_v {
            transactions.reserve(txs.len());
            for tx in txs {
                if let Some(parsed) = parse_block_transaction(tx) {
                    transactions.push(parsed);
                }
            }
        }

        Ok(Some(FetchedBlock { slot, block_height, transactions }))
    }
}

fn parse_block_transaction(tx: &Value) -> Option<BlockTransaction> {
    let txn = tx.get("transaction")?;
    let meta = tx.get("meta");

    let message = txn.get("message")?;
    let keys_v = message.get("accountKeys")?.as_array()?;
    let mut account_keys = Vec::with_capacity(keys_v.len());
    for k in keys_v {
        let s = k.as_str()?;
        if let Ok(pk) = s.parse::<Pubkey>() { account_keys.push(pk); }
    }

    let ixs_v = message.get("instructions").and_then(|i| i.as_array()).cloned().unwrap_or_default();
    let mut instructions = Vec::with_capacity(ixs_v.len());
    for ix in ixs_v {
        let program_id_index = ix.get("programIdIndex").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
        let account_indices: Vec<u8> = ix.get("accounts")
            .and_then(|a| a.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_u64().map(|n| n as u8)).collect())
            .unwrap_or_default();
        // `data` is base58 (default for jsonParsed/json encoding's instructions).
        let data_b58 = ix.get("data").and_then(|d| d.as_str()).unwrap_or("");
        let data = bs58_decode(data_b58);
        instructions.push(BlockInstruction { program_id_index, account_indices, data });
    }

    let failed = meta
        .and_then(|m| m.get("err"))
        .map(|e| !e.is_null())
        .unwrap_or(false);
    let fee_lamports = meta
        .and_then(|m| m.get("fee"))
        .and_then(|f| f.as_u64())
        .unwrap_or(0);

    Some(BlockTransaction { account_keys, instructions, failed, fee_lamports })
}

/// Inline bs58 decoder so we don't pull in an extra crate. Solana addresses
/// and instruction data are bs58 in the JSON-RPC default encoding.
fn bs58_decode(s: &str) -> Vec<u8> {
    const ALPH: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    if s.is_empty() { return Vec::new(); }
    let mut num = vec![0u8; s.len()];
    let mut len = 0usize;
    for &c in s.as_bytes() {
        let mut carry = match ALPH.iter().position(|&x| x == c) {
            Some(p) => p as u32,
            None    => return Vec::new(), // invalid char → empty
        };
        for byte in num.iter_mut().take(len) {
            carry += (*byte as u32) * 58;
            *byte = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            num[len] = (carry & 0xff) as u8;
            len += 1;
            carry >>= 8;
        }
    }
    let leading_ones = s.bytes().take_while(|&b| b == b'1').count();
    let mut out = vec![0u8; leading_ones];
    out.extend(num[..len].iter().rev());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_addresses_returns_empty() {
        let rpc = HeliusRpc::new("http://localhost".into());
        // Just verify the call constructs (the future is never awaited).
        let fut = rpc.get_multiple_accounts(&[]);
        drop(fut);
    }

    #[test]
    fn bs58_decode_round_trip_known_value() {
        // "11111111111111111111111111111111" base58-decodes to 32 zero bytes
        // (the System Program ID).
        let zeros = bs58_decode("11111111111111111111111111111111");
        assert_eq!(zeros, vec![0u8; 32]);
    }

    #[test]
    fn bs58_decode_short_string() {
        // "1" -> [0]
        let one = bs58_decode("1");
        assert_eq!(one, vec![0]);
        // "2" -> [1]  ('2' is the 2nd char in the alphabet, index 1)
        let two = bs58_decode("2");
        assert_eq!(two, vec![1]);
    }

    #[test]
    fn bs58_decode_empty_returns_empty() {
        assert!(bs58_decode("").is_empty());
    }
}
