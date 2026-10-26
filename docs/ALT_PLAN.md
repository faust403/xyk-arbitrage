# Address Lookup Table (ALT) — implementation plan

## When this becomes necessary

Only if, after the native-path switchover from `RUNBOOK_NATIVE.md`, the operator observes `tx_too_large` rejections on the `native_executor` path (currently expected to fit, per the doc comment in `crates/arb-scout/src/raw_tx.rs`). If native bundles land cleanly, this work is deferred indefinitely.

## What ALT buys

Each Pubkey in a transaction's account list costs 32 bytes inline. An ALT lets the tx reference up to 256 of them by 1-byte index instead, after declaring the ALT account once. For our two-leg native swap path the typical reusable accounts are:

| Account | Reused per swap leg | In every tx |
|---|---|---|
| SPL Token Program | × | ✅ |
| System Program | × | ✅ |
| ComputeBudget Program | — | ✅ |
| ATA Program | — | ✅ |
| `RAYDIUM_AMM_AUTHORITY` (PDA) | × | when an AMM v4 leg fires |
| All 8 Jito tip accounts | — | one per tx, but rotates |
| USDC mint | × | ✅ |
| WSOL mint | × | when SOL is the interim |
| Raydium AMM v4 program id | × | when AMM v4 leg fires |
| Raydium CLMM program id | × | when CLMM leg fires |
| Orca Whirlpool program id | × | when Whirlpool leg fires |
| Sysvar (Rent / Clock) | — | sometimes |

That's ~14–16 reusable pubkeys. At 32 bytes saved per ALT lookup minus 1-byte index cost, an aggressive ALT can cut ~430 bytes per tx — comfortably enough to cover any over-budget native tx.

## Steps

### Step 1 — Deploy the ALT (one-time, operator)

Use the Solana CLI on the operator's local Linux/WSL machine (Solana CLI not required on Windows):

```bash
# Pick a recent slot; the ALT is keyed by (authority, slot).
solana address-lookup-table create --keypair ~/path/to/wallet.json

# Capture the ALT pubkey it prints. Then extend it with the reusable set:
solana address-lookup-table extend <ALT_PUBKEY> --addresses \
    TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA \
    11111111111111111111111111111111 \
    ComputeBudget111111111111111111111111111111 \
    ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL \
    5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1 \
    EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
    So11111111111111111111111111111111111111112 \
    675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8 \
    CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK \
    whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc \
    96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5 \
    HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe \
    Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY \
    ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49 \
    DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh \
    ADuUkR4vqLUMWXxW9gh6D6L8pivKeVBBjNs5kyqMzjqJ \
    DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL \
    3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT
```

Cost: ~0.005 SOL one-time + a small per-byte rent.

After extension, **wait at least one slot** before consuming the ALT — the runtime defers visibility. (`solana address-lookup-table get <ALT_PUBKEY>` will show the entries.)

### Step 2 — Wire the bot to use it

This is code work, deferred until Step 1 is done. The plan:

1. **Read the ALT contents at startup** (similar to how `bootstrap` already fetches AmmConfigs). Cache `(ALT_pubkey, [Pubkey; N])`.
2. **Parallel `build_and_sign_v0_tx_b64`** in `crates/arb-scout/src/raw_tx.rs`. Wire format reference: <https://docs.solana.com/developing/versioned-transactions>. Highlights:
   - Replace 4-byte `header` with 5-byte `header` (the version-prefix byte `0x80` + the same 3 header counts).
   - After the static-account-keys list, append:
     - `compact-u16` count of address-lookup-table entries (1 for our case)
     - For each entry: 32-byte ALT pubkey + `compact-u16` writable indexes + `compact-u16` readonly indexes.
   - Compiled instructions that reference an ALT-resolved account use indices that start at `static_keys.len()` and continue first into writable-from-ALT, then readonly-from-ALT.
3. **Selector in `native_executor`** based on `ALT_ADDRESS` env var:
   ```
   if let Some(alt) = env::var("ALT_ADDRESS").ok() {
       build_and_sign_v0_tx_b64(payer, &ixs, &blockhash, wallet_bytes, &alt_account)
   } else {
       build_and_sign_legacy_tx_b64(payer, &ixs, &blockhash, wallet_bytes)
   }
   ```
4. **Tests:** unit-test the v0 wire format by comparing a known-input output against a reference produced by `solana-sdk` on a Linux box.

### Estimated size for a v0 build (target)

| Component | Bytes |
|---|---|
| Signature(s) | 65 |
| Header (incl. version byte) | 5 |
| Static account keys (~6 unique non-ALT) | 6×32 + ~2 = 194 |
| ALT entries (1 ALT, ~10 idx total) | 32 + ~14 = 46 |
| Recent blockhash | 32 |
| ComputeBudget × 2 | ~26 |
| Tip transfer | ~14 |
| Swap leg1 (compiled with ALT-indexed accounts) | ~250 |
| Swap leg2 | ~250 |
| **Total** | **~882** |

Comfortably under 1232 bytes.

## Stop and ask before deploying

Per `INSTRUCTIONS.md` Phase 11.4 / hard rule "before spending real SOL":
- Creating the ALT spends ~0.005 SOL — operator confirms before running the `solana address-lookup-table create` command.
- The first v0-tx submission is a "first of its kind" call — operator confirms before flipping `ALT_ADDRESS` in production env.
