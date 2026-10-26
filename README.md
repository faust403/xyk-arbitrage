# Solana Arbitrage Bot — Rust + Jito Bundles + Flash Loans

> **High-performance, zero-loss MEV arbitrage bot for the Solana DEX ecosystem.**
> Built in Rust for sub-millisecond execution. Uses Jito bundles for atomic, all-or-nothing execution and Solend (Save Finance) flash loans for capital-efficient trades.

---

## 🏗️ Current Production Infrastructure (live as of 2026-05-17)

| Layer | Provider / Plan | Why |
|---|---|---|
| **RPC + LaserStream gRPC** | **Helius — Business Plan** | Yellowstone Geyser streams (~50 ms ahead of WSS), 5/sec getMultipleAccounts, staked `sendTransaction` endpoint, optional Sender. Production rate limits across all running services (arb-scout, kamino-hunter, marginfi-hunter, pumpswap-watcher, sandwich-runner). Endpoint: `https://mainnet.helius-rpc.com` + `https://laserstream-mainnet-ams.helius-rpc.com`. |
| **Aggregator quotes + swap IXs** | **Jupiter — API Launch Plan** | Hot path for Kamino+MarginFi liquidator legs (asset→liability swap) and exchange-path executor. Launch plan gives sustained throughput + lower latency than the public `lite-api` tier. Endpoint: `https://api.jup.ag/swap/v1`. |
| **Compute host** | **AWS EC2 t3.medium @ eu-central-1 (Frankfurt)** — EIP `35.158.207.68` | 0.622 ms RTT to Jito Frankfurt block engine. All services run as `systemd` units, binaries deployed under `/home/ubuntu/arbbot/target/release/`. Source repo at `/home/ubuntu/arbbot-src`. Old Railway/Ashburn deployment is dormant. |
| **Bundle / submit paths** | Jito Block Engine (Frankfurt+Amsterdam+NY+SLC+Tokyo) + direct TPU UDP send (`NATIVE_SUBMISSION_PATH=tpu`) + optional Helius Sender (`https://sender.helius-rpc.com/fast`) | Path is picked per-service. Native MEV uses direct TPU; liquidators use Helius sendTransaction with high priority fee. |
| **Flash loans** | MarginFi v2 (0 bps) + Solend USDC (preserved as backup) | MarginFi flash-borrow wraps every Kamino + MarginFi liquidation tx. |

**Active services on the Frankfurt box** (`systemctl list-units`):
- `arbbot` — native MEV scout/executor (event-driven via LaserStream, 73 pools loaded)
- `kamino-hunter` — Kamino lend liquidator (Pyth-tick → LaserStream → on-chain fire, ~5 ms reaction)
- `marginfi-hunter` — MarginFi v2 liquidator (LaserStream oracle subscriber, 80K+ accounts indexed, 30 bank oracles subscribed)
- `pumpswap-watcher` — PumpSwap graduation detector + cross-DEX arb candidate logger (Phase 1: detect; Phase 2: fire)
- `sandwich-runner` — shred-detected sandwich runner (currently profit-gated after the 2026-05-16 USDC→wSOL drift incident)
- `shred-subscriber` — Pyth shred-stream consumer (for Solend/Kamino-event triggers)
- `atomic-liquidator` — Python observer/coordinator for the hunters

Deploy flow: `git push origin main` → SSH to Frankfurt → `cd ~/arbbot-src && git pull && cargo build --release --bin <bin> [--features laserstream] -j 2` → `cp target/release/<bin> /home/ubuntu/arbbot/target/release/<bin>` → `sudo systemctl restart <service>`. Always stop arbbot+kamino-hunter before building to free RAM on the 3.8 GB box.

---

## 🎯 Core Goals

1. **Zero-loss guarantee** — Every trade is wrapped in an on-chain `assert_min_profit` check + Jito bundle atomicity. If profit < total fees, the entire bundle reverts. We pay the base fee (5,000 lamports) and walk away. No loss of principal, no failed swaps mid-route.
2. **Maximum profitability** — Multi-DEX scanning across Raydium, Orca, Meteora, Phoenix, and Lifinity. Captures spreads via flash-loaned capital — no idle inventory required.
3. **Speed-first architecture** — Pure Rust hot path. Geyser gRPC for real-time pool state, ShredStream (phase 2) for pre-block visibility. Pre-built instruction templates avoid runtime allocation.
4. **Operator-friendly** — Telegram bot interface (in Rust via `teloxide`) for live monitoring, P&L, kill switch, and parameter tuning without restarting the bot.
5. **Verifiable correctness** — Every component has unit tests, integration tests, and a phase gate before mainnet capital is risked.

---

## ⚠️ START HERE: Phase −1 Paper-Trading Scout

**Before writing a single line of the main bot, build and run the scout.** This is non-negotiable.

**Why:** Solana MEV arbitrage is a saturated, highly competitive market in 2026. Top searchers operate from Frankfurt with ShredStream and have years of optimization. From other locations, the *residual* opportunity rate is uncertain — and it's the difference between a profitable bot and a 3-month money-and-time sink.

**What:** A standalone Rust program (`crates/arb-scout`) that:
- Connects to Helius Enhanced WebSockets (Atlas) — Developer plan
- Monitors top SOL/USDC + stablecoin pools across **5 supported DEXs**
  (Raydium AMM v4, Raydium CLMM, Orca Whirlpool, Meteora DLMM, Lifinity v2)
- Enumerates **2-hop through 5-hop cycles** (configurable `SCOUT_MAX_HOPS`)
- Runs **ternary-search optimal sizing** per profitable cycle (Phase 7 work
  pulled forward — finds the input size that maximizes net profit, not just
  a fixed probe)
- Cross-references each logged opportunity against the on-chain block
  (Jito-tip-account scanner) to compute **actual capture rate** vs other
  searchers — Section 6 of the report
- Logs every opportunity to JSONL: timestamp, route, sizes, fees, net profit
- **Submits nothing.** Reads only.

> **Operator note (Osher, 2026-05-10):** the scout's scope has expanded
> beyond the original "minimal feasibility" framing. It now performs the
> work originally assigned to Phase 7 (optimal sizing, multi-hop pathfinding)
> and Phase 9 partial (Jito correlation). Phase boundaries are no longer
> rigid — the goal is to surface **real profit numbers** from a 7-day
> mainnet run, not just a binary "yes spreads exist".

**Decision criteria after 1–2 weeks of running:**

| Metric | Go-ahead threshold | Reconsider threshold |
|--------|-------------------|---------------------|
| Opportunities/day with net profit ≥ $1 | ≥ 10 | < 5 |
| Opportunities/day with net profit ≥ $5 | ≥ 2 | 0 |
| Largest single opportunity in window | ≥ $20 | < $5 |
| LaserStream uptime | ≥ 99% | < 95% |

If the data clears the "go-ahead threshold" → build the full bot.
If it falls into "reconsider threshold" → stop. Save the 3–4 months. Reconsider strategy, location, or whether this is the right project.

**Cost of the scout phase:** ~1 week of development + **$49/month** (Helius Developer plan) — possibly $0 if the Free tier is sufficient. Worst case if you stop after the scout: ~$100 total spent, minimal time invested.

**Note on streaming:** The scout uses Helius **Enhanced WebSockets** (included in the Developer plan), not LaserStream gRPC. As of April 2026, Enhanced WebSockets run on the same infrastructure as LaserStream and provide comparable latency for typical workloads. LaserStream gRPC is only needed if the scout's results suggest microsecond-level latency advantages would change the outcome — and even then, it's now bundled in the $499/month Business plan, not a $999 Professional plan.

This phase is detailed in `INSTRUCTIONS.md` as **Phase −1**.

---

## 🏛️ Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      TELEGRAM BOT                            │
│       (commands: /status, /pnl, /pause, /resume, /tune)     │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│                    ORCHESTRATOR (Rust)                       │
│           Configuration · State · Risk limits                │
└──┬──────────────┬───────────────┬──────────────┬────────────┘
   │              │               │              │
┌──▼──────┐  ┌────▼─────┐   ┌─────▼────┐   ┌─────▼──────┐
│ MARKET  │  │ STRATEGY │   │  EXEC    │   │ MONITORING │
│  DATA   │  │  ENGINE  │   │  ENGINE  │   │  & PNL     │
└──┬──────┘  └────┬─────┘   └────┬─────┘   └────────────┘
   │              │              │
┌──▼──────────────▼──────────────▼─────────┐
│        SOLANA RPC LAYER (Helius)          │
│  ┌──────────────┐  ┌────────────────────┐│
│  │ LaserStream  │  │  HTTPS RPC (paid)  ││
│  │  (Geyser)    │  │  + Jito Block Eng. ││
│  └──────────────┘  └────────────────────┘│
└───────────────────────────────────────────┘
```

### Modules (Cargo workspace)

| Crate | Purpose | Wave |
|-------|---------|------|
| **`arb-scout`** | **Standalone paper-trading scout — runs BEFORE the main build to validate economic viability** | **−1** |
| `arb-core` | Shared types: `Pool`, `Route`, `Quote`, `Opportunity`, error types | 1 |
| `arb-config` | Loads `.env` + TOML configs, validates them on startup | 1 |
| `arb-flashloan` | Solend flash borrow/repay instruction builder | 1 |
| `arb-jito` | Jito bundle construction, tip accounts, submission | 1 |
| `arb-stream` | Geyser gRPC client (Helius LaserStream) | 1 |
| `arb-pathfinder` | Multi-hop route discovery (Bellman-Ford on token graph) | 1 |
| `arb-dex-raydium` | Raydium AMM v4 + CLMM parsers and quote math | 1 |
| `arb-dex-orca` | Orca Whirlpools (concentrated liquidity) parser + quoter | 1 |
| `arb-dex-meteora` | Meteora DLMM parser + quoter (binned liquidity) | 2 |
| `arb-dex-phoenix` | Phoenix orderbook parser + quoter | 3 |
| `arb-dex-lifinity` | Lifinity proactive market maker | 3 |
| `arb-telemetry` | Logs, metrics, P&L computation, persistent storage | 1 |
| `arb-telegram` | Telegram bot interface | 1 |
| `arb-bot` | Main binary: orchestrator, strategy loop | 1 |

---

## 🔐 The Zero-Loss Mechanism (read this carefully)

We use **two independent layers** of profit enforcement:

### Layer 1: Off-chain simulation (pre-flight)
Before submitting any bundle, we simulate it via `simulateBundle` against the latest committed slot. If simulated final balance ≤ initial balance + min_profit_threshold, we drop the opportunity. Cost: 0 lamports.

### Layer 2: On-chain assert program (in-bundle)
Our custom Anchor program (`assert_min_profit`) is added as the **last instruction** of the bundle. It:
1. Reads the bot's USDC (or other base token) balance at instruction-time.
2. Compares against a `min_balance` value passed as instruction data.
3. Returns `Err` if the check fails — which causes the entire Jito bundle to revert atomically.

**Result:** Even if pool state shifts between simulation and execution, an unprofitable trade can NEVER complete. Worst case: bundle fails, we pay the 5,000-lamport base fee per signature. The flash loan is never repaid? It doesn't matter — Solend's `FlashRepayReserveLiquidity` instruction enforces atomic repayment in the same transaction; if repayment can't happen, the borrow itself reverts.

This double-protection ensures the bot is **mathematically incapable** of finishing a trade at a loss on principal.

---

## ⚙️ Tech Stack

| Layer | Choice | Why |
|-------|--------|-----|
| Language | **Rust 1.78+** | Zero-cost abstractions, no GC pauses, native Solana SDK |
| Async runtime | `tokio` | De-facto standard, ecosystem-rich |
| RPC provider | **Helius (paid)** | Best Solana RPC: low latency, LaserStream Geyser, staked endpoints |
| Block engine | **Jito** | Atomic bundles, MEV-aware leader connection |
| Flash loans | **Solend** (Save Finance) | Highest liquidity, battle-tested, ~0.09% fee |
| On-chain assert | **Anchor 0.30+** | Custom program for `min_profit` enforcement |
| Streaming | `yellowstone-grpc-client` | Geyser plugin protocol — Helius LaserStream uses this |
| Telegram | `teloxide` | Pure-Rust, async, no Node.js dependency |
| Serialization | `borsh`, `bincode` | Solana-native account decoding |
| Data structures | `dashmap`, `arc-swap` | Lock-free concurrent pool state |
| Testing | `cargo test`, `proptest`, `solana-test-validator` | Standard Rust + Solana fork testing |

---

## 🌐 DEX Coverage Roadmap

The bot is designed to scan all major Solana DEXs. We ship them in waves:

### Wave 1 (MVP — week 1–4)
- ✅ Raydium AMM v4 (constant product)
- ✅ Orca Whirlpools (concentrated liquidity)

### Wave 2 (week 5–6)
- ✅ Raydium CLMM
- ✅ Meteora DLMM (binned liquidity — you already know this one!)

### Wave 3 (week 7+)
- ✅ Phoenix (CLOB / orderbook)
- ✅ Lifinity v2 (proactive AMM)
- ⏳ Saber, Aldrin, FluxBeam (only if their nominal volume justifies)

Each DEX implements the `DexQuoter` trait, so adding new venues is mechanical.

---

## 💰 Token Pair Strategy

We focus on liquid, low-rug-risk markets:

**Stablecoins:** USDC, USDT, PYUSD, USDS
**Top 10 SOL-ecosystem tokens:** SOL, JUP, JTO, BONK, WIF, PYTH, RNDR, RAY, ORCA, MNDE
**Liquid wrapped assets:** wBTC, wETH, mSOL, jitoSOL, bSOL

Memecoins and long-tail tokens are excluded by default — the rug/liquidity risk outweighs the spread upside. This can be re-enabled per-token via Telegram.

---

## 🚦 Detection Strategy (hybrid)

Two parallel detectors feed the strategy engine:

### Detector A: Geyser gRPC (account updates) — Wave 1
- Subscribes to all monitored pool accounts via Helius LaserStream.
- On every `account_update`, recomputes affected routes.
- Latency: ~50–150 ms after on-chain change.
- Coverage: 100% of state changes (post-block).

### Detector B: ShredStream — Phase 2 (post-MVP)
- Reads partial blocks from Jito relayers before block finalization.
- Allows backrun strategies — see a large swap mid-block, queue a bundle to capture the resulting imbalance.
- Latency: ~10–30 ms (pre-block).
- **Requires a server geographically close to validators (Frankfurt/Amsterdam/Tokyo).** Won't work effectively from Israel due to physical latency.
- Phase 2 — once the Geyser-based bot is profitable.

---

## 📦 Repository Layout

```
arb-bot/
├── Cargo.toml                     # workspace root
├── README.md                      # this file
├── INSTRUCTIONS.md                # build instructions for Claude Code
├── .env.example                   # environment template (no secrets)
├── .gitignore
├── rust-toolchain.toml            # pin Rust version
├── crates/
│   ├── arb-core/
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── pool.rs
│   │   │   ├── route.rs
│   │   │   ├── opportunity.rs
│   │   │   ├── token.rs
│   │   │   └── error.rs
│   │   └── tests/
│   │       ├── pool_tests.rs
│   │       ├── route_tests.rs
│   │       └── token_math_tests.rs
│   ├── arb-config/
│   ├── arb-flashloan/
│   ├── arb-jito/
│   ├── arb-stream/
│   ├── arb-pathfinder/
│   ├── arb-dex-raydium/
│   ├── arb-dex-orca/
│   ├── arb-dex-meteora/
│   ├── arb-dex-phoenix/
│   ├── arb-dex-lifinity/
│   ├── arb-telemetry/
│   ├── arb-telegram/
│   ├── arb-bot/                   # main binary
│   └── arb-scout/                 # pre-build paper-trading scout (Phase -1)
├── programs/
│   └── assert-min-profit/         # Anchor program
│       ├── Cargo.toml
│       ├── Anchor.toml
│       ├── src/lib.rs
│       └── tests/
│           └── assert_tests.rs
├── tests/
│   ├── integration/               # multi-crate, devnet/fork tests
│   │   ├── flash_loan_e2e.rs
│   │   ├── full_arb_dryrun.rs
│   │   └── assert_program_e2e.rs
│   └── fixtures/                  # snapshot data, mock pools
│       ├── raydium_pools.bin
│       ├── orca_whirlpools.bin
│       └── README.md
├── config/
│   ├── dryrun.toml
│   ├── devnet.toml
│   ├── papertrading.toml
│   └── mainnet.toml
├── scripts/
│   ├── deploy_assert_devnet.sh
│   ├── deploy_assert_mainnet.sh
│   ├── fetch_pool_fixtures.sh
│   ├── run_dryrun.sh
│   ├── run_papertrading.sh
│   └── monitor_pnl.sh
├── docs/
│   ├── RUNBOOK.md                 # day-2 operations
│   ├── ARCHITECTURE.md            # deep dive on internals
│   └── TROUBLESHOOTING.md
└── state/                         # gitignored: pnl.json, last-bundle.json, etc.
```

---

## 🧪 Testing Pyramid

Each crate has three test layers:

| Layer | What it tests | When it runs |
|-------|---------------|--------------|
| **Unit** | Pure functions (quote math, route construction, fee calculations) | `cargo test` — every commit |
| **Property** (proptest) | Invariants under random inputs (e.g., `quote(quote_inverse(x)) == x ± slippage`) | `cargo test` — every commit |
| **Integration** | Multi-crate flows against `solana-test-validator` or devnet | `cargo test --test integration` — pre-PR + nightly |

Plus phased acceptance tests (described in `INSTRUCTIONS.md`):
- **Phase −1: Paper-trading scout (1-2 weeks)** ← economic feasibility check
- Phase 9.1: Mainnet-fork dry-run (24h)
- Phase 9.2: Devnet end-to-end (10+ live bundles)
- Phase 9.3: Mainnet paper-trading (7 days, full bot)
- Phase 9.4: Mainnet live, capped capital (30 days)
- Phase 9.5: Mainnet scale

---

## 🧪 Testing & Deployment Phases

The bot **never** goes straight to mainnet with capital. The release path:

| Phase | Environment | Capital | Goal |
|-------|-------------|---------|------|
| **−1** | **Paper-trading scout (lightweight)** | **0** | **Validate economic viability BEFORE building the full bot** |
| 0–10 | Build the full bot (Rust workspace) | 0 | Code + unit/integration tests |
| 11.1 | `solana-test-validator` (mainnet-fork) | 0 (simulated) | Verify route math, instruction building |
| 11.2 | Devnet | 2–3 devnet SOL (free airdrop) | End-to-end Anchor program testing |
| 11.3 | Paper-trading on mainnet (full bot) | 0 (no submission) | Validate detection latency, opportunity rate |
| 11.4 | Mainnet small | 0.5 SOL working capital | Live bundles, micro-tips, observe inclusion |
| 11.5 | Mainnet scale | gradual increase | Tune tips, expand DEX coverage |

Each phase has **explicit pass criteria** documented in `INSTRUCTIONS.md`.

### 🔍 Why a paper-trading scout BEFORE building?

Building the full bot is 3–4 months of work and ~$1,500–3,000 in setup costs (subscriptions, deploys, server, fees). Solana arb is a saturated market in 2026, and from a non-Frankfurt location the bot may capture only residual opportunities.

**The scout is a lightweight (200–500 lines) Rust program** that connects to Helius LaserStream, monitors 20–30 popular pools, computes hypothetical arbitrage opportunities in real time, and **logs them without submitting anything**. It runs for 1–2 weeks and answers a binary question:

> Are there enough profitable opportunities visible from our location to justify building the full bot?

**If yes** (≥ 10 opportunities/day with potential profit ≥ $1 each, after estimated fees) → proceed to build the full bot.
**If no** → save 3–4 months of work; reconsider strategy or location.

Cost of the scout: ~1 week of work + $99 (one month of Helius Developer plan). This is the cheapest possible insurance against building a bot that won't be profitable.

---

## 🚀 Deployment & Operations (AWS Frankfurt, current production)

The bot is deployed on a **dedicated AWS EC2 t3.medium in eu-central-1 (Frankfurt)**, EIP `35.158.207.68`. Latency to the Jito Frankfurt block-engine is **0.622 ms RTT** — orders of magnitude better than the Railway/Amsterdam fallback we used during phase −1.

**Why we left Railway:** Railway's Amsterdam region sat ~30-50 ms from Jito Frankfurt and we couldn't pin a specific machine — every redeploy could land on a different host with different latency. AWS lets us pin one EC2 in the exact AZ and reach the Jito relay in under 1 ms consistently.

### AWS deployment workflow

**Development (Windows + VS Code):**
1. Code locally on Windows.
2. `cargo check --workspace` to verify compile.
3. `git push origin main`.

**Frankfurt deploy:**
1. SSH to the box: `ssh -i ~/Downloads/arbbot-fra-key.pem ubuntu@35.158.207.68`.
2. `cd /home/ubuntu/arbbot-src && git pull --rebase origin main`.
3. **Stop arbbot + kamino-hunter first** — the 3.8 GB box OOMs when cargo + arbbot + hunter run concurrently. `sudo systemctl stop arbbot kamino-hunter`.
4. `cargo build --release --bin <bin> [--features laserstream] -j 2`.
5. **Copy to the systemd binary path** (NOT `/usr/local/bin`): `cp target/release/<bin> /home/ubuntu/arbbot/target/release/<bin>`.
6. Restart: `sudo systemctl start <bin> arbbot kamino-hunter`.

**Service units live at:** `/etc/systemd/system/<service>.service`. Logs go to `/var/log/<service>.log` via `StandardOutput=append:`. Old logs rotate via logrotate (configured 2026-05-15).

**Env file:** `/etc/arbbot/.env` is the single source of truth for all secrets (HELIUS_API_KEY, JUPITER_API_KEY, SOLANA_KEYPAIR_JSON, MARGINFI_ACCOUNT, executor knobs, etc.). Owned by root, group-readable by ubuntu. Never check into git — committed `.env` would expose the live wallet's private key.

**Legacy:** The original Railway deployment plan in this section was kept dormant during the AWS migration as a fallback. Do not push to Railway from current development branches.

### Environment variable strategy (current — `/etc/arbbot/.env`)

All secrets and tunable knobs live in `/etc/arbbot/.env`, sourced by each systemd unit's `EnvironmentFile=` directive. Highlights:
```env
# RPC / streaming (Helius Business plan)
HELIUS_API_KEY=<live>
HELIUS_RPC_URL=https://mainnet.helius-rpc.com/?api-key=...
LASERSTREAM_GRPC_URL=https://laserstream-mainnet-ams.helius-rpc.com
HELIUS_SENDER_URL=https://sender.helius-rpc.com/fast

# Aggregator (Jupiter API Launch plan)
JUPITER_API_KEY=<live>
JUPITER_ENDPOINT_BASE=https://lite-api.jup.ag/swap/v1  # per-service can switch to api.jup.ag/swap/v1

# Wallet (BURN-AFTER-READING — never commit, never paste into chat)
SOLANA_KEYPAIR_JSON=[1,2,3,...,64]
MARGINFI_ACCOUNT=<our marginfi-account pubkey, derived once via initialize_account>

# ALTs (deployed via alt-deploy-* binaries)
EXECUTOR_NATIVE_ALT=...
EXECUTOR_KAMINO_ALT2=...
EXECUTOR_MARGINFI_ALT=...
EXECUTOR_PYTH_ALT=...

# Submission path + knobs
NATIVE_SUBMISSION_PATH=tpu       # direct UDP to leaders, bypasses Helius/Jito relays
EXECUTOR_USE_JITO=false           # Sender path instead
EXECUTOR_MIN_PROFIT_USDC=10000    # $0.01 — tuned 2026-05-16 after observation
EXECUTOR_PROBE_AMOUNT_USDC=200000000   # $200 cap
```

### Why this stack

- **Helius Business** — Only tier with LaserStream Geyser gRPC + staked sendTransaction + the 5/sec `getMultipleAccounts` budget that the marginfi-hunter's bootstrap (80K+ accounts × 100/chunk) needs. Public RPCs simply rate-limit us out within minutes.
- **Jupiter API Launch** — `lite-api.jup.ag` returns 429 under our combined liquidator + arbbot load. Launch plan moves us to `api.jup.ag` which has the throughput headroom for the hot path (`attempt_liquidation` calls `/quote` + `/swap-instructions` for every fire).
- **AWS Frankfurt** — same physical region as Jito's Frankfurt block engine + most validator nodes. The TPU-direct send path lives or dies on this — from Israel/US-East the same code would lose every contested slot.
- **EU-West region available** — optimal latency to Frankfurt validators
- **Built-in monitoring** — logs, metrics, alerts via Railway dashboard
- **Secrets management** — no keypair files in git repos

### Cost structure (current production, 2026-05-17)

| Item | Monthly cost (USD) |
|---|---|
| **Helius Business plan** (LaserStream gRPC, staked send, Sender, 5/s gMA) | $499 |
| **Jupiter API Launch plan** (api.jup.ag throughput tier) | varies — operator-set |
| **AWS EC2 t3.medium @ eu-central-1** + EIP + EBS | ~$35 |
| Jito tips & priority fees | variable (paid only on landed trades) |
| Solend / MarginFi flash loan fees | MarginFi = 0 bps; Solend = 9 bps |
| **Total fixed monthly (infra)** | **~$535 + Jupiter plan** |

Break-even: roughly **$18-25 net profit per day** to cover infra. With marginfi-hunter + pumpswap-watcher + native arbbot all running, each landed liquidation typically nets $20-$700 (MarginFi liqs) and each pump.fun cross-DEX arb $5-$200 once Phase-2 fires — single good day clears multiple months.

**Scout-only phase (historical, Phase −1):**
- Helius Developer ($49/mo) + Railway (~$10) = ~$60/mo — enough to log opportunities without firing anything. We graduated past this in 2026-05.

---

## 💸 Operating Cost Estimates

We graduated past the scout-only economics and are on the full production stack: **Helius Business + Jupiter API Launch + AWS Frankfurt** (see the [Current Production Infrastructure](#-current-production-infrastructure-live-as-of-2026-05-17) table at the top of this doc for live endpoints and the rationale for each).

### Current production — minimum fixed costs

| Item | Monthly cost (USD) |
|------|-------------------|
| **Helius Business plan** — LaserStream gRPC + staked send + Sender + 5/s getMultipleAccounts | $499 |
| **Jupiter API Launch plan** — `api.jup.ag/swap/v1` throughput | operator-set (varies by tier) |
| **AWS EC2 t3.medium** @ eu-central-1 + Elastic IP + EBS | ~$35 |
| Anchor program deploy (one-time historical) | ~$300 (~2 SOL @ ~$150 at the time) |
| Jito tips & priority fees | variable — paid only on landed trades |
| MarginFi v2 flash-loan fee | 0 bps |
| Solend USDC flash-loan fee (backup path) | 9 bps of borrowed amount per trade |
| **Total fixed monthly (infra only)** | **~$535 + Jupiter** |

**Break-even**: roughly **$18-25 net profit per day** to cover infra. With marginfi-hunter, kamino-hunter, arbbot (native MEV), pumpswap-watcher, and sandwich-runner all live, each MarginFi liquidation typically nets $20-$700 and each PumpSwap-graduation arb nets $5-$200 once Phase-2 fires — a single good day clears multiple months of infra.

### Why not the cheaper Helius tiers

Tried Developer ($49/mo) during the scout — works fine for log-only flows. Once we wired marginfi-hunter's 80K+ account bootstrap + arbbot's per-tick build_leg quotes + kamino-hunter's 30 oracle subscribers, Helius Developer's 1/sec getMultipleAccounts cap rate-limited every service into uselessness. Business plan unlocks the 5/sec budget and (more importantly) the LaserStream Geyser gRPC stream — without it, the event-driven hunters can't react to a Pyth tick before competitor bots fire.

### Why Jupiter Launch instead of lite-api

`lite-api.jup.ag` returns 429 once you push more than a handful of `/quote` + `/swap-instructions` calls per second. Liquidator hot path does ~1 quote + 1 swap-ix per fire, with ~150 fire attempts/hr at peak. Launch plan moves us to `api.jup.ag` with the headroom to not lose opportunities to throttling.

### Scout-only economics (historical, kept for reference)

| Item | Monthly cost (USD) |
|------|-------------------|
| Helius Developer plan | $49 |
| Railway EU-West (Phase −1 host, now decommissioned) | $5–15 |
| Telegram bot | Free |
| **Total during scout phase (historical)** | **~$55–65** |

---

## ⚠️ Honest Risk Disclosure

This bot enforces zero loss **of principal**. It does NOT guarantee profit. Real risks that remain:

- **Operational losses** — Subscription costs, electricity, server uptime can outpace gross profit during slow weeks.
- **Smart contract risk** — Solend's flash loan program could have an exploit (audited, but not invulnerable). Our own assert program could have a bug — mitigated via tests + small initial capital.
- **Stuck flash loan failures** — Bundle fails, no SOL lost on principal, but priority fees + 5,000-lamport base fees are paid per attempt. Many failures = bleed.
- **Competition** — Top MEV searchers on Solana use ShredStream from Frankfurt validators. From Israel, we are at a 20–80 ms latency disadvantage. We compensate via better routing, more DEX coverage, and patience for opportunities others miss.
- **Validator skipped slots** — Bundles can land as failed transactions on uncled blocks. Rare, but documented by Jito.

---

## 🤝 Working with Claude Code

This project is intended to be built primarily by Claude Code following `INSTRUCTIONS.md`. The instructions file is structured as **phases**, each with:

1. Code requirements (specific files, structs, traits)
2. Test requirements (unit tests, property tests, integration tests with concrete examples)
3. Pass criteria (objective, measurable)
4. **"Stop and ask the operator if..."** clauses — explicit triggers for Claude Code to stop and request human input rather than guess.

Claude Code is instructed to:
- **Never advance to the next phase** without all pass criteria met.
- **Always ask before** making non-obvious architectural choices, deploying to mainnet, or deviating from the documented design.
- **Always ask before spending real SOL** on deployments, transactions, or fees.
- **Always ask when documentation is ambiguous** rather than guessing — especially for on-chain account layouts and instruction encoding, where guessing wastes SOL on failed transactions.

---

## 🚀 Quick Start

See **[INSTRUCTIONS.md](./INSTRUCTIONS.md)** for the full build plan.

```bash
# Clone, configure, build
git clone <your-repo> arb-bot
cd arb-bot
cp .env.example .env
# Edit .env with your Helius API key, wallet keypair path, Telegram token
cargo build --release

# Phase 1: Mainnet-fork dry-run
./scripts/run_dryrun.sh

# Phase 4: Mainnet, small capital (only after passing earlier phases)
./target/release/arb-bot --config config/mainnet.toml
```

---

## 📜 License

Private — for personal use only. Do not distribute.

---

## 👤 Author

Built by Osher (CapitalRise, prior Meteora DLMM bot, UltraShop). For development assistance via Claude Code.
