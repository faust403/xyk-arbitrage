# Build Instructions for Claude Code

> **Audience:** Claude Code (or any AI coding agent) building this bot from scratch.
> **Read this file in full before writing any code.** The order of operations matters — building the assert program before the DEX parsers, for example, will block testing.

---

## 🛑 META-RULE: When in doubt, ASK

This is the most important rule in this document. Claude Code, you are dealing with **real money on a live blockchain**. Every guess that turns out wrong wastes SOL. Every assumption you make instead of asking can ruin a deploy.

### You MUST stop and ask the operator (Osher) when:

1. **Account layouts or instruction encodings are unclear** — DEX SDKs change. If you can't find authoritative documentation for an account discriminator, field offset, or instruction format, ask. Don't guess based on "what it probably looks like."

2. **You're about to spend real SOL** — Any mainnet deployment, any first-of-its-kind transaction, any fee that could exceed 0.01 SOL. Ask first, even if the instructions say to proceed.

3. **A test fails in a way you can't immediately diagnose** — Don't loop on "let me try a different approach." After 2 failed attempts at fixing a single test, stop and present:
   - What you tried
   - What you expected
   - What happened
   - Three hypotheses for why
   - The one you'd pursue next

4. **The phase pass criteria can be interpreted multiple ways** — If a criterion says "low latency," ask for the threshold. If it says "frequent opportunities," ask for a number.

5. **You discover a security concern** — Anything that looks like it could leak the keypair, log a private key, or expose API credentials. STOP. Report it. Do not "fix it cleverly."

6. **A dependency version is ambiguous** — Solana ecosystem dependencies break frequently. If `Cargo.toml` resolution fails or you're tempted to bump a major version, ask first.

7. **You'd need to add a CEX, a non-Helius RPC fallback, or any non-Rust runtime code** — These are explicitly forbidden. If you find yourself wanting to add them to "solve a problem," the problem has a different solution. Ask.

8. **The operator's intent is ambiguous** — If a config value isn't specified anywhere, default to the conservative interpretation AND mention what you defaulted to in your status message.

9. **You're tempted to skip Phase −1 (paper-trading scout)** — Phase −1 is mandatory and comes BEFORE Phase 0. If the operator says "let's just build the bot" or "skip the scout", politely confirm: "The scout takes ~1 week and saves potentially 3+ months of work if the data is unfavorable. Are you sure you want to skip it?" — and require an explicit confirmation before proceeding.

### How to ask

When you stop to ask, format your question like this:

```
🛑 Need operator input before proceeding.

CONTEXT: Building Phase X.Y — [task name]
BLOCKER: [one-sentence description]
WHAT I TRIED: [if anything]
OPTIONS I SEE:
  A) [option] — pros: [...] cons: [...]
  B) [option] — pros: [...] cons: [...]
  C) [option] — pros: [...] cons: [...]
MY RECOMMENDATION: [A/B/C] because [reason]
```

Wait for the operator's response. Do not advance.

---

## ⚙️ Hard Rules (do not violate)

1. **Rust everywhere.** No Node.js, no Python, no TypeScript in the runtime path. Bash scripts are allowed for deployment automation only.
2. **No CEX integration.** Ever. This bot is DEX-only. Do not add Binance, Coinbase, or any centralized exchange code paths even if "it would help price discovery."
3. **Helius is the sole RPC provider.** Use the paid endpoint from `.env`. Do not fall back to public RPCs (`api.mainnet-beta.solana.com`) — they will rate-limit and corrupt the bot's worldview.
4. **Every bundle must include the `assert_min_profit` instruction.** No exceptions. No "let's skip it for testing" — testing happens against the assert program too.
5. **Flash-loan repayment is mandatory and atomic.** The borrow and repay instructions are always paired in the same transaction. Never split them across a bundle's transactions.
6. **Phase gates are non-negotiable.** Do not advance to the next testing phase until the current phase's pass criteria are met (defined per phase below).
7. **No hardcoded keypairs, API keys, or webhook URLs in code.** Everything via `.env` and `config/*.toml`.
8. **Hebrew comments are welcome** but all identifiers (function names, struct names, log messages) must be in English for ecosystem compatibility.
9. **Test-first for the assert program.** The Anchor program is the last line of defense — write tests before implementation.
10. **Conservative defaults.** Initial `min_profit_lamports` = 50,000 (0.00005 SOL), initial Jito tip = 10,000 lamports, initial capital cap = 0.5 SOL. Operator increases via Telegram.
11. **Phase −1 (paper-trading scout) is mandatory.** Do not begin Phase 0 (full bot bootstrap) until Phase −1 results are reviewed and approved by the operator. The scout determines whether building the full bot is economically justified.

---

## 🔍 Phase −1: Paper-Trading Scout (PRE-BUILD CHECKPOINT)

> **OPERATOR NOTE (Osher, 2026-05-10):** The scout's scope has been expanded
> beyond the original minimal-viable framing. It now includes work that was
> previously assigned to Phase 7 (optimal-size ternary search, multi-hop
> cycle enumeration up to `SCOUT_MAX_HOPS=5`) and Phase 9 (Jito-bundle
> capture-rate correlator). Rationale: the operator wants the 7-day data to
> show **real profit numbers**, not just "spreads exist". Phase boundaries
> are now flexible per Osher's authorization.

**Goal:** Determine whether the full arbitrage bot is worth building, given the operator's geographic location (Israel) and the saturated state of Solana MEV. Cost: ~1 week development + $55-65 (Helius Developer + Railway hosting). Saves potentially 3–4 months and ~$2,000+ if results are negative.

**This phase happens BEFORE Phase 0.** Build the scout as a standalone Rust binary in `crates/arb-scout/`. Do not build any other crate yet. Do not deploy any Anchor program. Do not touch flash loans, Jito, or the assert program.

### Development and deployment workflow

**Phase -1A: Local development (Windows + VS Code):**
1. Code on Windows using VS Code and `cargo build`/`cargo test`
2. Scout connects to Helius Enhanced WebSockets (Developer plan, $49/month)
3. Test locally with 5-minute runs to verify compilation and basic connectivity
4. **Do not run 24/7 locally** — move to Railway for continuous operation

**Phase -1B: Railway deployment (continuous operation):**
5. Create Railway project linked to GitHub repo
6. **Critical: Select EU-West region** (Amsterdam/Netherlands)
7. Configure environment variables in Railway dashboard
8. Add persistent volume: `/app/logs` (1GB, to preserve scout data)
9. Deploy and run continuously for 1-2 weeks to gather economic data

### Streaming approach for the scout

The scout uses **Helius Enhanced WebSockets** (`accountSubscribe` over WSS), NOT LaserStream gRPC.

**Why:** As of April 2026, Helius runs Enhanced WebSockets on the same backend infrastructure as LaserStream. For the scout's purposes (logging opportunities, not winning auctions), the latency is comparable. Enhanced WebSockets are included in the **Developer plan at $49/month**, while LaserStream gRPC requires the **Business plan at $499/month**. The scout exists specifically to determine whether the more expensive infrastructure is worth it — so it must NOT depend on that infrastructure to operate.

**Connection details:**
- Endpoint: `wss://atlas-mainnet.helius-rpc.com?api-key=YOUR_API_KEY` (Enhanced WSS)
- Subscribe via JSON-RPC: `accountSubscribe` for each pool address, `commitment: "confirmed"`
- Crate: `tokio-tungstenite` for WebSocket, `serde_json` for JSON-RPC framing

### Railway deployment configuration

Create a Railway project with these specific settings:

**1. Project setup:**
```bash
# In your local arb-bot directory (after git repo is ready)
# Install Railway CLI on Windows:
# Download from https://railway.app/cli or use:
# iwr -useb https://railway.app/install.ps1 | iex

railway login
railway init  # Link to existing GitHub repo
railway up    # Initial deploy
```

**2. Region selection (CRITICAL):**
- In Railway dashboard: Project → Settings → Environment
- **Region: EU West (Netherlands)** — this provides <150ms latency to Frankfurt/Amsterdam Solana validators
- **Never use US regions** for this bot — latency penalty is too high for arbitrage

**3. Environment variables (Railway dashboard → Variables):**
```env
HELIUS_API_KEY=your_api_key_here
HELIUS_ENHANCED_WSS_URL=wss://atlas-mainnet.helius-rpc.com?api-key=${HELIUS_API_KEY}
RUN_MODE=scout
RUST_LOG=info

# Optional: Telegram notifications (can be added later)
# TELEGRAM_BOT_TOKEN=
# TELEGRAM_OPERATOR_CHAT_ID=
```

**4. Persistent storage:**
- Railway dashboard → Settings → Volumes
- **Mount Path:** `/app/logs`
- **Size:** 1GB (sufficient for 2 weeks of scout logs)
- **Purpose:** Preserve scout data across Railway restarts/deploys

**5. Resource allocation:**
- Railway auto-scales, but you can set minimums in Settings
- **CPU:** 1 vCPU (scout is not CPU-intensive)
- **RAM:** 512MB - 1GB (sufficient for scout operations)
- **Estimated cost:** $5-15/month during scout phase

### Local development setup (Windows)

**Prerequisites on Windows:**
1. **Rust toolchain:** Download from https://rustup.rs, install with default settings
2. **Git:** Pre-installed with GitHub Desktop or https://git-scm.com/download/win
3. **VS Code:** With rust-analyzer extension for optimal Rust development

**Local testing workflow:**
```powershell
# Clone your repo and navigate to it
git clone https://github.com/youruser/arb-bot
cd arb-bot

# Set up environment for local testing (create .env file):
echo "HELIUS_API_KEY=your_key_here" > .env
echo "RUN_MODE=scout" >> .env
echo "RUST_LOG=debug" >> .env

# Build and test compilation
cargo check --workspace
cargo build --bin arb-scout

# Short local test run (5 minutes max)
cargo run --bin arb-scout
# Ctrl+C to stop — do NOT run 24/7 locally
```

**Why local testing is limited:**
- Your Windows machine should sleep, restart, update — not run the scout 24/7
- Railway provides superior uptime, monitoring, and persistent logs
### Railway configuration files

Create these files in your project root to optimize Railway builds:

**`railway.json`** (optional, but recommended):
```json
{
  "$schema": "https://railway.app/railway.schema.json",
  "build": {
    "builder": "nixpacks"
  },
  "deploy": {
    "startCommand": "cargo run --release --bin arb-scout",
    "healthcheckPath": "/health",
    "healthcheckTimeout": 300,
    "restartPolicyType": "always"
  },
  "environments": {
    "production": {
      "variables": {
        "CARGO_PROFILE_RELEASE_DEBUG": "false",
        "CARGO_PROFILE_RELEASE_STRIP": "true"
      }
    }
  }
}
```

**`Dockerfile`** (optional - Railway can auto-detect Rust, but explicit is better):
```dockerfile
# Use Rust 1.78 to match rust-toolchain.toml
FROM rust:1.78-slim as builder

# Install required system dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /app

# Copy over Cargo files and source
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ ./crates/
COPY config/ ./config/

# Build the scout binary only
RUN cargo build --release --bin arb-scout

# Runtime stage - smaller image
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create app user and directories
RUN useradd --create-home --shell /bin/bash app
USER app
WORKDIR /app

# Create logs directory
RUN mkdir -p /app/logs

# Copy binary from builder
COPY --from=builder /app/target/release/arb-scout /app/arb-scout
COPY --from=builder /app/config /app/config

# Health check endpoint (simple)
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
  CMD pgrep arb-scout || exit 1

# Run the scout
CMD ["/app/arb-scout"]
```

**`.dockerignore`** (to speed up builds):
```
target/
.git/
.env
.env.*
logs/
*.log
.DS_Store
.vscode/
.idea/
```

These files are optional but will give Railway better control over the build process and reduce deployment time.

### What the scout does

1. Connects to Helius Enhanced WebSocket (`wss://atlas-mainnet.helius-rpc.com?api-key=...`).
2. Subscribes to ~20 hand-picked popular pool accounts via `accountSubscribe` (specified in `config/scout-pools.toml`).
3. On each pool update, evaluates pre-computed candidate arb routes touching that pool.
4. For each route, computes:
   - Theoretical gross profit (output − input, after pool swap fees)
   - Estimated Solend flash loan fee (~9 bps of borrow size)
   - Estimated Jito tip (configurable; default 10,000 lamports + 30% of profit)
   - Estimated priority fee (configurable; default 50,000 microlamports/CU × 600,000 CU)
   - **Net profit** = gross − all fees
5. Writes each opportunity to `logs/scout-YYYY-MM-DD.jsonl` (one JSON object per line).
6. Cross-references against Jito explorer (background task) to determine if the same opportunity was actually captured by another searcher, and how much they tipped.
7. Every hour, emits a Telegram summary: count of opportunities, distribution of net profit values, capture rate (fraction we'd have lost to faster competitors).

### Required structure

```rust
// crates/arb-scout/src/main.rs

#[tokio::main]
async fn main() -> Result<()> {
    let config = ScoutConfig::load()?;

    let store = PoolStateStore::new();          // simplified — same idea as full bot
    let parsers: Vec<Box<dyn DexQuoter>> = vec![
        Box::new(RaydiumAmmQuoter::new()),
        Box::new(OrcaWhirlpoolQuoter::new()),
        Box::new(MeteoraDlmmQuoter::new()),     // OK to ship from day one for the scout
    ];

    let routes = build_routes(&config.pools);   // simple 2-hop and 3-hop cycles in USDC
    let scout = Scout::new(store, parsers, routes, config);

    tokio::try_join!(
        scout.run_enhanced_websocket(),         // tokio-tungstenite + accountSubscribe
        scout.run_telegram_summary(),
        scout.run_jito_explorer_correlator(),
    )?;
    Ok(())
}
```

### `Cargo.toml` for the scout crate

```toml
[package]
name = "arb-scout"
version = "0.1.0"
edition = "2021"

[dependencies]
solana-sdk = { workspace = true }
solana-client = { workspace = true }
spl-token = { workspace = true }

# WebSocket — note: NOT yellowstone-grpc-client.
# The scout uses standard JSON-RPC over WSS, which is included in the Developer plan.
tokio-tungstenite = { version = "0.21", features = ["rustls-tls-webpki-roots"] }
tokio = { workspace = true }
futures = { workspace = true }

teloxide = { workspace = true }

borsh = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
toml = { workspace = true }

anyhow = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }

dashmap = { workspace = true }
arc-swap = { workspace = true }

reqwest = { workspace = true }
dotenvy = { workspace = true }
```

The scout shares some primitives with the full bot — notably `arb-core::DexQuoter` and the basic pool parsers. **This is intentional**: the work done here is reusable later. When the operator approves Phase 0, the DEX parser code from the scout becomes the seed for `arb-dex-raydium`, `arb-dex-orca`, `arb-dex-meteora`. The streaming code (Enhanced WSS) will need to be replaced with LaserStream gRPC if the operator chooses to upgrade later — but that is a small, well-defined change.

### What the scout does NOT do

- ❌ Does NOT build flash loan instructions.
- ❌ Does NOT build or send transactions or bundles.
- ❌ Does NOT require an Anchor program deploy.
- ❌ Does NOT need a Jito subscription (it only reads Jito's public explorer API).
- ❌ Does NOT use LaserStream gRPC. Uses **Helius Enhanced WebSockets** (Developer plan, $49/month). The scout is exactly the experiment that determines whether LaserStream is worth its cost.
- ❌ **Does NOT need a Solana keypair/wallet.** The scout is read-only — it never signs transactions or holds SOL. Skip all keypair setup until you decide to build the full bot.

### `config/scout-pools.toml`

```toml
[[pools]]
name = "Raydium SOL/USDC"
address = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2"
dex = "raydium-amm-v4"

[[pools]]
name = "Orca Whirlpool SOL/USDC (0.05% tier)"
address = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtL_REPLACE_ME"  # ASK OPERATOR for canonical addresses
dex = "orca-whirlpool"

# ... ~20 pools total
```

🛑 **Stop and ask the operator** for the canonical pool addresses. Do not guess or scrape — use the addresses the operator provides or asks for clarification.

### Required tests for Phase −1

#### Unit tests

| Test | What it verifies |
|------|------------------|
| `test_scout_quote_raydium_matches_simulation` | For 5 SOL/USDC quotes, matches `simulateTransaction` output within 1 unit |
| `test_scout_quote_orca_matches_simulation` | Same for Orca Whirlpool |
| `test_scout_quote_meteora_matches_simulation` | Same for Meteora DLMM |
| `test_scout_fee_estimation_includes_all_costs` | Fee calculation includes flash loan, swap fees, Jito tip estimate, priority fee |
| `test_scout_net_profit_subtracts_fees` | Net = gross − flashloan − tip − priority |
| `test_scout_logs_jsonl_format` | Each line is parseable JSON with required fields |
| `test_scout_routes_only_2_or_3_hops` | No 4+ hop routes evaluated (compute cap) |
| `test_scout_telegram_summary_format` | Hourly summary message is well-formatted |

#### Integration tests

| Test | What it verifies | Environment |
|------|------------------|-------------|
| `scout_subscribes_to_laserstream` | After connecting, receives ≥ 1 update in 60s | mainnet (read-only) |
| `scout_logs_at_least_one_opportunity_in_24h` | After 24h run, at least 1 opportunity in JSONL | mainnet (read-only) |
| `scout_jito_correlator_resolves` | Given a logged opportunity, queries Jito explorer for that slot and finds the actual capturing bundle (or confirms none existed) | mainnet (read-only) |

### Run plan

```bash
# Day 1: Build scout, test against single pool
cargo run --release --bin arb-scout -- --config config/scout-single.toml

# Days 2-3: Expand to ~20 pools, fix bugs, verify quote accuracy
cargo run --release --bin arb-scout -- --config config/scout-full.toml

# Days 4-14: Continuous run, collect data
nohup cargo run --release --bin arb-scout -- --config config/scout-full.toml > logs/scout.out 2>&1 &
```

### Required deliverables at end of Phase −1

After 7–14 days of continuous running, produce a report file `docs/SCOUT_REPORT.md` with:

1. **Total opportunities observed** (counted from JSONL)
2. **Distribution of net profit values:**
   - Number with net profit ≥ $0.10
   - Number with net profit ≥ $1
   - Number with net profit ≥ $5
   - Number with net profit ≥ $20
3. **Average net profit per opportunity**
4. **Time-of-day distribution** (when opportunities cluster — useful for tuning later)
5. **DEX-pair distribution** (which pairs dominate)
6. **Capture rate analysis:** of opportunities ≥ $1, what fraction would have been won by us vs already captured by another searcher (per Jito explorer)
7. **LaserStream uptime** during the period
8. **Estimated monthly net profit** if we'd been live, accounting for capture rate

### Decision criteria — operator approval gate

The operator (Osher) reviews the report and decides. As guidance:

| Metric | Build the full bot | Reconsider |
|--------|-------------------|------------|
| Opportunities/day with net profit ≥ $1 | ≥ 10 | < 5 |
| Opportunities/day with net profit ≥ $5 | ≥ 2 | 0 |
| Largest single opportunity in window | ≥ $20 | < $5 |
| Estimated monthly net (after capture rate) | ≥ $1,500 | < $500 |
| LaserStream uptime | ≥ 99% | < 95% |

🛑 **Phase −1 ALWAYS ends with an explicit operator decision.** Do not auto-advance to Phase 0. Even if all metrics are green, output the report and wait for operator confirmation: "OK, build the full bot."

### 🛑 Phase −1 — Stop and ask if:

- The operator hasn't provided pool addresses for the scout.
- LaserStream connection details (URL, region, token) aren't configured.
- Quote math for any DEX disagrees with `simulateTransaction` by more than 1 unit during testing — this would invalidate the scout's data.
- After 48 hours of running, fewer than 5 total opportunities have been logged. Possible causes: (a) bug in scout logic, (b) wrong pool addresses, (c) genuinely no opportunities — report and ask.
- LaserStream uptime falls below 95% during the window — switching regions may be needed.
- The operator wants to extend the scout window (run longer for more data) or shorten it (decide faster).

### Phase −1 Pass Criteria

- [ ] Scout binary builds and runs.
- [ ] All unit tests pass.
- [ ] All integration tests pass.
- [ ] At least 7 consecutive days of continuous run completed with ≥ 99% uptime.
- [ ] `docs/SCOUT_REPORT.md` produced with all required sections.
- [ ] Operator has reviewed the report and explicitly approved (or rejected) advancing to Phase 0.
- [ ] Output the Test Inventory table.

### What happens if the scout says "reconsider"?

Stop. Do not build the full bot. Save the 3+ months of work. Possible next steps for the operator (NOT for Claude Code to decide):

- Reconsider whether to deploy a server in Frankfurt/Amsterdam (closes the latency gap, allows ShredStream — but adds cost and complexity).
- Pivot focus back to CapitalRise, the Meteora DLMM bot, or other higher-conviction projects.
- Consider a fundamentally different MEV strategy (liquidations? token launches? long-tail pool monitoring?).
- Wait 6 months and re-run the scout — competitive landscape shifts.

The scout's job is to give honest data. Your job is to deliver that data accurately. The operator's job is to decide.

---

## 🧪 Testing Standards (apply to ALL phases)

Every crate Claude Code creates MUST satisfy these baseline test requirements before advancing:

### Unit tests
- Located in `crates/<name>/src/<module>.rs` under `#[cfg(test)] mod tests`.
- Cover all public functions with at least: a happy path, an edge case, and an error case.
- For numerical functions: include zero-input, max-input (overflow check), and boundary cases.
- Naming: `test_<function_name>_<scenario>` — e.g., `test_quote_zero_input_returns_zero`.

### Property tests (where applicable)
- Use `proptest` crate.
- Required for: math functions (quote calculations, fee math), route construction, instruction encoding/decoding.
- At least one property test per module, e.g.:
  ```rust
  proptest! {
      #[test]
      fn quote_is_deterministic(input in 0u64..u64::MAX/2) {
          let q1 = quote(&pool, input);
          let q2 = quote(&pool, input);
          prop_assert_eq!(q1, q2);
      }
  }
  ```

### Integration tests
- Located in `tests/integration/`.
- Run against `solana-test-validator` (started via `cargo test` setup) or devnet (gated behind `#[ignore]` and run explicitly).
- Each phase below specifies which integration tests must pass.

### Coverage minimum
- 80% line coverage on `arb-core`, `arb-flashloan`, `arb-jito`, `arb-pathfinder`.
- 90% on the Anchor program (small surface area, no excuse).
- Use `cargo tarpaulin` to measure. If you don't reach the threshold, add tests; do not ask to lower the threshold.

### Test naming and reporting
- Every phase below ends with a "Test Inventory" table. Claude Code MUST output, at end of phase, a status report listing each test by name and result. Example:

```
PHASE 3 TEST INVENTORY
======================
arb-flashloan::solend::test_borrow_ix_account_order ......... PASS
arb-flashloan::solend::test_repay_ix_references_borrow ...... PASS
arb-flashloan::solend::test_fee_calculation_zero ............ PASS
arb-flashloan::solend::test_fee_calculation_max ............. PASS
arb-flashloan::solend::prop_borrow_repay_round_trip ......... PASS (1000 cases)
integration::flash_loan_e2e_borrow_and_repay ................ PASS (devnet)
integration::flash_loan_e2e_skip_repay_reverts .............. PASS (devnet)
==================================================
Total: 7/7 PASS — Phase 3 gate: ✅ READY FOR PHASE 4
```

If any test fails: ❌ DO NOT ADVANCE. Ask operator.

---

## 📂 Phase 0: Repository Bootstrap

**⚠️ Prerequisite:** Phase −1 (paper-trading scout) is complete and the operator has explicitly approved advancing to Phase 0. If you have not received this approval, STOP and ask. Do not begin Phase 0 unilaterally.

**Goal:** Empty workspace that compiles. No business logic yet.

### Tasks

1. Create directory structure as shown in README.md `Repository Layout`.
2. Create workspace `Cargo.toml`.
3. Create `rust-toolchain.toml` pinning to `1.78` (or latest stable; ask if unsure).
4. Create empty `lib.rs` for each crate with module-level doc comment.
5. Create `.env.example` (template below).
6. Create `.gitignore` excluding: `target/`, `.env`, `*.json` keypair files, `state/`, `logs/`.
7. Create empty config TOML files in `config/`.

### `Cargo.toml` (workspace root)

```toml
[workspace]
resolver = "2"
members = [
    "crates/arb-scout",
    "crates/arb-core",
    "crates/arb-config",
    "crates/arb-dex-raydium",
    "crates/arb-dex-orca",
    "crates/arb-dex-meteora",
    "crates/arb-dex-phoenix",
    "crates/arb-dex-lifinity",
    "crates/arb-pathfinder",
    "crates/arb-flashloan",
    "crates/arb-jito",
    "crates/arb-stream",
    "crates/arb-telemetry",
    "crates/arb-telegram",
    "crates/arb-bot",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
rust-version = "1.78"

[workspace.dependencies]
# Solana ecosystem (pin versions; updates break things)
solana-sdk = "2.0"
solana-client = "2.0"
solana-program = "2.0"
spl-token = "6.0"
spl-associated-token-account = "5.0"
anchor-lang = "0.30"
anchor-client = "0.30"

# Async runtime
tokio = { version = "1", features = ["full"] }
futures = "0.3"
async-trait = "0.1"

# Streaming
yellowstone-grpc-client = "3"
yellowstone-grpc-proto = "3"
tokio-tungstenite = { version = "0.21", features = ["rustls-tls-webpki-roots"] }

# Telegram
teloxide = { version = "0.13", features = ["macros"] }

# Serialization & utility
borsh = "1.5"
bincode = "1.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
anyhow = "1"
thiserror = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Concurrency
dashmap = "6"
arc-swap = "1.7"
parking_lot = "0.12"

# Math
rust_decimal = "1.35"
num-bigint = "0.4"

# HTTP
reqwest = { version = "0.12", features = ["json", "rustls-tls"] }

# Env
dotenvy = "0.15"

# Testing
proptest = "1"
mockall = "0.13"
```

### `.env.example`

```env
# ============================================================
# RPC endpoints
# ============================================================

HELIUS_API_KEY=put-your-key-here
HELIUS_RPC_URL=https://mainnet.helius-rpc.com/?api-key=${HELIUS_API_KEY}

# Used by the scout (Phase -1) — Enhanced WebSocket on Developer plan ($49/mo)
HELIUS_ENHANCED_WSS_URL=wss://atlas-mainnet.helius-rpc.com?api-key=${HELIUS_API_KEY}

# Used by the full bot (Phase 6+) — only required if you upgraded to Business plan ($499/mo)
# Leave blank during the scout phase. Bot will use Enhanced WSS as fallback if this is empty.
HELIUS_LASERSTREAM_URL=
HELIUS_LASERSTREAM_TOKEN=

# ============================================================
# Jito (only used by the full bot, not the scout)
# ============================================================

JITO_BLOCK_ENGINE_URL=https://frankfurt.mainnet.block-engine.jito.wtf
JITO_TIP_LAMPORTS_DEFAULT=10000

# ============================================================
# Wallet (only used by the full bot, not the scout)
# ============================================================

# CRITICAL: For Railway deployment, use environment variable format (JSON array)
# NOT file paths. Railway secrets handle this securely.
# Example: SOLANA_KEYPAIR_JSON=[1,2,3,...,64]  (64 numbers from your private key)
SOLANA_KEYPAIR_JSON=
ASSERT_PROGRAM_ID=put-deployed-program-id-here

# ============================================================
# Solend (only used by the full bot, not the scout)
# ============================================================

SOLEND_LENDING_MARKET=4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY
SOLEND_USDC_RESERVE=BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw

# ============================================================
# Telegram (used by both scout and full bot)
# ============================================================

TELEGRAM_BOT_TOKEN=put-your-bot-token-here
TELEGRAM_OPERATOR_CHAT_ID=put-your-chat-id-here

# ============================================================
# Risk parameters (full bot only)
# ============================================================

MIN_PROFIT_LAMPORTS=50000
MAX_FLASHLOAN_AMOUNT_USDC=1000000000   # 1,000 USDC (6 decimals)
MAX_CONCURRENT_OPPORTUNITIES=4

# ============================================================
# Mode & Environment
# ============================================================

# scout | dryrun | papertrading | mainnet
RUN_MODE=scout

# Logging level (debug for local testing, info for Railway production)
RUST_LOG=info

# ============================================================
# Railway-specific settings (set in Railway dashboard, not in .env)
# ============================================================

# These are managed via Railway environment variables:
# - Region: EU West (Netherlands) — set in Railway project settings
# - Volumes: /app/logs mounted — set in Railway volumes settings
# - Auto-scaling: enabled — set in Railway resource settings
```

**Important notes for Railway deployment:**

1. **Never put real secrets in `.env`** — that file goes to git. Use Railway environment variables for all sensitive data.

2. **Keypair format:** Railway uses environment variables, not files. Convert your Phantom/Solflare private key to JSON array format: `[1,2,3,...,64]`

3. **Region setting:** Must be done in Railway dashboard, not via environment variable.

4. **Local vs Railway:** Use `.env` for local testing only. Railway production uses the Variables section of the dashboard.

### Tests for Phase 0

| Test | Type | Pass criterion |
|------|------|----------------|
| `cargo build` | Compilation | Exits 0, no warnings |
| `cargo test --workspace` | Unit | Exits 0 (no tests yet, but framework works) |
| `cargo clippy --workspace -- -D warnings` | Lint | Exits 0 |
| Verify `.env.example` is committed and `.env` is in `.gitignore` | Manual | True |

### 🛑 Phase 0 — Stop and ask if:
- Workspace doesn't compile after the listed setup.
- Any dependency version above isn't available on crates.io (likely a major version change).
- The operator hasn't provided any of: Helius API key, Telegram bot token, keypair path. Don't try to use placeholder values for testing — ask which to mock.

### Phase 0 Pass Criteria

- [ ] All directories and stub files exist.
- [ ] `cargo build` succeeds.
- [ ] `cargo clippy` has zero warnings.
- [ ] `.env.example` exists; `.env` is gitignored.
- [ ] Output the Test Inventory table.

---

## 📐 Phase 1: Core Types (`arb-core`)

**Goal:** Shared vocabulary that every other crate depends on.

### Required types

```rust
// crates/arb-core/src/lib.rs
pub mod pool;
pub mod route;
pub mod opportunity;
pub mod error;
pub mod token;

pub use pool::{Pool, PoolKind, PoolState};
pub use route::{Route, Hop, RouteId};
pub use opportunity::{Opportunity, ExpectedProfit};
pub use error::{ArbError, ArbResult};
pub use token::{TokenMint, TokenAmount};
```

### `token.rs` — must include

```rust
/// A token mint, wrapped Pubkey for type safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenMint(pub Pubkey);

/// An amount in a specific token's base units.
/// Carries decimals to prevent unit-confusion bugs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenAmount {
    pub mint: TokenMint,
    pub amount: u64,
    pub decimals: u8,
}

impl TokenAmount {
    pub fn checked_add(self, other: Self) -> Option<Self> { /* ... */ }
    pub fn checked_sub(self, other: Self) -> Option<Self> { /* ... */ }
    pub fn ui_amount(self) -> f64 { /* amount / 10^decimals */ }
}
```

### `pool.rs` — must include

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
    RaydiumAmmV4,
    RaydiumClmm,
    OrcaWhirlpool,
    MeteoraDlmm,
    PhoenixMarket,
    LifinityV2,
}

#[derive(Debug, Clone)]
pub struct Pool {
    pub address: Pubkey,
    pub kind: PoolKind,
    pub token_a: TokenMint,
    pub token_b: TokenMint,
    pub state: PoolState,           // raw on-chain bytes, parsed on demand
    pub last_update_slot: u64,
}
```

### `error.rs` — must include

```rust
#[derive(Debug, thiserror::Error)]
pub enum ArbError {
    #[error("pool not found: {0}")]
    PoolNotFound(Pubkey),
    #[error("insufficient liquidity for amount {amount} in pool {pool}")]
    InsufficientLiquidity { pool: Pubkey, amount: u64 },
    #[error("quote computation overflow")]
    QuoteOverflow,
    #[error("flash loan repayment shortfall: have {have}, need {need}")]
    RepaymentShortfall { have: u64, need: u64 },
    #[error("simulation failed: {0}")]
    SimulationFailed(String),
    #[error("rpc error: {0}")]
    Rpc(String),
    #[error("config error: {0}")]
    Config(String),
    // ... extend as needed
}

pub type ArbResult<T> = Result<T, ArbError>;
```

### Key trait: `DexQuoter`

```rust
#[async_trait::async_trait]
pub trait DexQuoter: Send + Sync {
    fn dex_id(&self) -> &'static str;

    /// Compute output amount for a given input on this pool.
    /// MUST be deterministic and match on-chain math exactly.
    fn quote(&self, pool: &Pool, input: TokenAmount) -> ArbResult<TokenAmount>;

    fn build_swap_ix(
        &self,
        pool: &Pool,
        input: TokenAmount,
        min_output: TokenAmount,
        user: Pubkey,
    ) -> ArbResult<Instruction>;

    fn try_parse_account(&self, pubkey: &Pubkey, data: &[u8]) -> Option<Pool>;
}
```

### Required tests for Phase 1

| Test | Type | What it verifies |
|------|------|------------------|
| `test_token_amount_add_normal` | Unit | Addition in same mint works |
| `test_token_amount_add_different_mints_fails` | Unit | Cross-mint addition returns None |
| `test_token_amount_overflow_returns_none` | Unit | Overflow handled safely |
| `test_token_amount_ui_amount_decimals` | Unit | Decimals applied correctly (e.g., USDC 6 decimals) |
| `test_pool_kind_serde` | Unit | All `PoolKind` variants serialize/deserialize |
| `test_route_extend_appends_hop` | Unit | `Route::extend()` adds hop and updates token chain |
| `test_route_extend_mismatched_token_fails` | Unit | Cannot extend with hop whose input ≠ route's current token |
| `test_route_id_unique` | Unit | Same hops → same `RouteId`; different hops → different |
| `test_arb_error_display_messages` | Unit | All error variants format correctly |
| `prop_token_amount_add_commutative` | Property | a + b == b + a |
| `prop_route_id_deterministic` | Property | Building same route in same order always gets same ID |
| `mock_dex_quoter_implements_trait` | Unit | Mock satisfies the trait (compile-time check) |

### 🛑 Phase 1 — Stop and ask if:
- You're unsure how `Pool` should represent state for DEXs with very different layouts (Raydium AMM vs Orca CLMM vs Phoenix orderbook). The current design uses raw bytes + parser-on-demand — confirm before deviating.
- The operator wants `TokenAmount` to support negative values (e.g., for net-flow tracking). Currently it doesn't.

### Phase 1 Pass Criteria

- [ ] All listed types compile.
- [ ] All listed tests pass.
- [ ] `cargo doc --no-deps --workspace` succeeds (every public item documented).
- [ ] Coverage on `arb-core` ≥ 80%.
- [ ] Output the Test Inventory table.

---

## 🔧 Phase 2: Configuration (`arb-config`)

**Goal:** Single source of truth for all runtime configuration. Validates aggressively at startup.

### Required functionality

```rust
// crates/arb-config/src/lib.rs

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    pub mode: RunMode,                 // DryRun | PaperTrading | Mainnet
    pub rpc: RpcConfig,
    pub jito: JitoConfig,
    pub wallet: WalletConfig,
    pub solend: SolendConfig,
    pub telegram: TelegramConfig,
    pub risk: RiskConfig,
    pub dexes: DexesConfig,
    pub pools: Vec<MonitoredPool>,
}

impl Config {
    pub fn load() -> ArbResult<Self> {
        // 1. Load .env via dotenvy
        // 2. Determine config file from RUN_MODE (config/dryrun.toml, etc.)
        // 3. Parse TOML
        // 4. Substitute ${VAR} references from environment
        // 5. Validate (next function)
    }

    pub fn validate(&self) -> ArbResult<()> {
        // - All Pubkey fields parse correctly
        // - All URLs are well-formed
        // - min_profit_lamports > 0
        // - max_flashloan_amount > min_swap_amount
        // - In Mainnet mode, ASSERT_PROGRAM_ID is non-default
        // - In Mainnet mode, keypair file exists and is readable
        // - Telegram chat ID is non-zero in non-DryRun modes
    }
}
```

### Config files

Create `config/dryrun.toml`, `config/devnet.toml`, `config/papertrading.toml`, `config/mainnet.toml`. Each config differs only in:
- `mode`
- `rpc.endpoint` (devnet vs mainnet URL)
- `risk.max_flashloan_amount_usdc` (0 in dryrun/papertrading)
- `solend.lending_market` (devnet vs mainnet addresses)

### Required tests for Phase 2

| Test | Type | What it verifies |
|------|------|------------------|
| `test_config_load_dryrun` | Unit | Parses `dryrun.toml` correctly |
| `test_config_load_mainnet` | Unit | Parses `mainnet.toml` correctly |
| `test_config_substitutes_env_vars` | Unit | `${HELIUS_API_KEY}` is substituted |
| `test_config_invalid_pubkey_rejected` | Unit | Bad Pubkey in TOML returns Err |
| `test_config_mainnet_requires_assert_program_id` | Unit | Default placeholder rejected in Mainnet mode |
| `test_config_validates_min_profit_positive` | Unit | `min_profit_lamports = 0` rejected |
| `test_config_validates_url_format` | Unit | Bad URL rejected |
| `test_config_missing_env_var_clear_error` | Unit | Missing `HELIUS_API_KEY` produces actionable error message |

### 🛑 Phase 2 — Stop and ask if:
- The operator hasn't provided concrete values for any required field at config-load time.
- A field is mentioned in `.env.example` but not in any TOML file (or vice versa) — these must stay in sync.

### Phase 2 Pass Criteria

- [ ] All four config files load without error using `Config::load()`.
- [ ] Every `validate()` rule has a test.
- [ ] Switching `RUN_MODE` env var actually loads a different config file.
- [ ] Output the Test Inventory table.

---

## 🔒 Phase 3: Anchor Program (`programs/assert-min-profit`)

**Build this BEFORE the DEX integrations.** It's the safety net everything else relies on.

### Program responsibility

A single instruction: `check_min_balance(min_balance: u64)`. Reads the operator's specified token account; if `account.amount < min_balance`, return error. Otherwise, no-op.

### `programs/assert-min-profit/src/lib.rs`

```rust
use anchor_lang::prelude::*;
use anchor_spl::token::TokenAccount;

declare_id!("REPLACE_AFTER_DEPLOY");

#[program]
pub mod assert_min_profit {
    use super::*;

    /// Reverts the entire transaction if `token_account.amount < min_balance`.
    pub fn check_min_balance(
        ctx: Context<CheckMinBalance>,
        min_balance: u64,
    ) -> Result<()> {
        let actual = ctx.accounts.token_account.amount;
        require!(
            actual >= min_balance,
            AssertError::ProfitBelowThreshold
        );
        msg!("Profit check passed: {} >= {}", actual, min_balance);
        Ok(())
    }
}

#[derive(Accounts)]
pub struct CheckMinBalance<'info> {
    pub token_account: Account<'info, TokenAccount>,
}

#[error_code]
pub enum AssertError {
    #[msg("Final balance is below the required minimum profit threshold")]
    ProfitBelowThreshold,
}
```

### Required tests for Phase 3 (Anchor program)

ALL of these must be implemented in `programs/assert-min-profit/tests/assert_tests.rs` using Anchor's test framework. Run with `anchor test`.

| Test | What it verifies |
|------|------------------|
| `test_passes_when_balance_equals_min` | Account has exactly `min_balance` → success |
| `test_passes_when_balance_above_min` | Account has `min_balance + 1` → success |
| `test_fails_when_balance_below_min` | Account has `min_balance - 1` → ProfitBelowThreshold error |
| `test_fails_when_account_empty` | Account has 0, `min_balance > 0` → error |
| `test_passes_with_zero_min_balance` | `min_balance = 0`, account empty → success |
| `test_passes_at_u64_max` | Account at `u64::MAX`, `min_balance` slightly below → success |
| `test_fails_with_wrong_account_owner` | Token account owned by wrong wallet → Anchor account constraint fails |
| `test_program_does_not_modify_account` | Pre/post account state byte-identical (no mutation) |
| `test_program_fits_under_compute_budget` | Instruction uses ≤ 5,000 CU (instrumented test) |

### Deployment plan

```bash
# Phase 3a: localnet (FREE — start here)
cd programs/assert-min-profit
anchor test                        # runs against solana-test-validator

# Phase 3b: devnet (FREE — devnet airdrop)
solana config set --url devnet
solana airdrop 5
anchor build
anchor deploy --provider.cluster devnet
# Capture the program ID from output. Update config/devnet.toml ASSERT_PROGRAM_ID.

# Phase 3c: mainnet (COSTS ~2-3 SOL — DO NOT RUN WITHOUT OPERATOR CONFIRMATION)
# ⛔ STOP HERE. ASK OPERATOR FOR EXPLICIT GO-AHEAD.
```

### 🛑 Phase 3 — Stop and ask if:
- ANY mainnet deploy is about to happen. EVERY mainnet deploy requires explicit operator confirmation.
- An anchor test fails for a reason you can't immediately diagnose.
- The Anchor version in `Cargo.toml` doesn't match the Anchor CLI version installed on the operator's system (causes IDL mismatches).
- You're considering a more complex assert program (e.g., reading multiple accounts, computing diffs). Don't. The simplicity is the security feature.

### Phase 3 Pass Criteria

- [ ] All 9 tests pass on `solana-test-validator`.
- [ ] All 9 tests pass on devnet.
- [ ] Program deployed to devnet; program ID recorded in `config/devnet.toml`.
- [ ] Program does NOT yet exist on mainnet (this requires operator approval — Phase 9.4).
- [ ] Output the Test Inventory table.

---

## 🌊 Phase 4: Solend Flash Loan Builder (`arb-flashloan`)

**Goal:** Pure-function instruction builders for Solend's flash borrow/repay flow. No async, no RPC calls — just instruction construction.

### Critical Solend rules (from official docs)

1. Borrow and repay MUST be in the **same transaction**.
2. They MUST be at the **top level** — Solend rejects flash loans called via CPI (re-entrancy guard).
3. The fee is paid in the borrowed token, computed at borrow time. Fee = `amount * flash_loan_fee_wad / 10^18` + `host_fee` (typically ~9 bps total).
4. Reserve must be refreshed (`RefreshReserve` instruction) before borrowing.

### Required functions

```rust
// crates/arb-flashloan/src/solend.rs

pub fn build_refresh_reserve_ix(
    reserve: Pubkey,
    pyth_oracle: Pubkey,
    switchboard_oracle: Pubkey,
) -> Instruction;

pub fn build_flash_borrow_ix(
    lending_market: Pubkey,
    reserve: Pubkey,
    source_liquidity: Pubkey,
    destination_liquidity: Pubkey,
    user: Pubkey,
    amount: u64,
) -> Instruction;

pub fn build_flash_repay_ix(
    lending_market: Pubkey,
    reserve: Pubkey,
    source_liquidity: Pubkey,
    destination_liquidity: Pubkey,
    user: Pubkey,
    amount: u64,
    borrow_instruction_index: u8,  // index of borrow ix in the same tx
) -> Instruction;

/// Computes the fee owed for a flash loan of the given amount.
/// MUST match Solend's on-chain fee math exactly.
pub fn calculate_flash_loan_fee(reserve_state: &ReserveState, amount: u64) -> u64;

/// Convenience: build all three instructions for a USDC flash loan.
pub fn build_usdc_flash_loan_envelope(
    config: &SolendConfig,
    user: Pubkey,
    user_usdc_ata: Pubkey,
    amount: u64,
) -> ArbResult<FlashLoanEnvelope>;

pub struct FlashLoanEnvelope {
    pub refresh_ix: Instruction,
    pub borrow_ix: Instruction,
    pub repay_ix: Instruction,
    pub fee_lamports: u64,           // fee in token base units
}
```

### Transaction layout for a 2-hop arbitrage

```
TX in the bundle:
  [0] ComputeBudget::SetComputeUnitLimit
  [1] ComputeBudget::SetComputeUnitPrice  (priority fee)
  [2] RefreshReserve (USDC reserve)
  [3] FlashBorrowReserveLiquidity (borrow X USDC from Solend)
  [4] Swap A: USDC → SOL on Raydium
  [5] Swap B: SOL → USDC on Orca
  [6] FlashRepayReserveLiquidity (repay X USDC + fee)
  [7] AssertMinBalance (check final USDC ≥ initial + min_profit)
  [8] Tip transfer to Jito tip account
```

If swap B fails or returns less USDC than borrow + fee + min_profit, the assert at [7] reverts the entire transaction.

### Required tests for Phase 4

#### Unit tests (no RPC, pure instruction construction)

| Test | What it verifies |
|------|------------------|
| `test_borrow_ix_program_id_correct` | Program ID matches Solend mainnet |
| `test_borrow_ix_account_count` | Exactly N accounts (number depends on Solend version) |
| `test_borrow_ix_account_order` | Each account at expected index, with correct signer/writable flags |
| `test_borrow_ix_data_encoding` | Instruction discriminator + amount little-endian |
| `test_repay_ix_references_correct_borrow_index` | `borrow_instruction_index` field encoded correctly |
| `test_repay_ix_account_order` | Account order matches Solend spec |
| `test_refresh_ix_includes_oracles` | Both Pyth and Switchboard oracle accounts present |
| `test_envelope_repay_amount_includes_fee` | `repay_amount = borrow_amount + fee` |
| `test_fee_calculation_known_values` | Fee for 1000 USDC, 100 USDC, 1 USDC against expected (precomputed from Solend's math) |
| `test_fee_calculation_zero_amount` | Fee for 0 amount = 0 |
| `test_fee_calculation_no_overflow` | Fee for `u64::MAX / 2` doesn't overflow |

#### Property tests

| Test | Property |
|------|----------|
| `prop_fee_is_monotonic` | Larger amount → larger or equal fee |
| `prop_fee_under_one_percent` | For all reasonable amounts, fee < 1% of borrow |

#### Integration tests (`tests/integration/flash_loan_e2e.rs`)

| Test | What it verifies | Environment |
|------|------------------|-------------|
| `e2e_borrow_repay_round_trip` | On devnet, borrow 1 USDC, do nothing, repay → succeeds | devnet |
| `e2e_borrow_without_repay_reverts` | Borrow 1 USDC, skip repay → entire tx reverts | devnet |
| `e2e_borrow_repay_short_reverts` | Borrow 1 USDC, repay only 0.99 → reverts | devnet |
| `e2e_borrow_via_cpi_reverts` | Wrap borrow in another program (CPI) → Solend rejects | devnet |
| `e2e_envelope_with_fake_swaps` | Build envelope with no-op swaps, verify fee is paid correctly | devnet |

### 🛑 Phase 4 — Stop and ask if:
- You can't find the exact Solend instruction discriminator. The Solend SDK is in flux (the protocol rebranded to "Save Finance"). Verify against latest source: https://github.com/solendprotocol/solana-program-library/tree/master/token-lending
- Solend reserve addresses for USDC/USDT differ between docs sources. Ask which address is canonical.
- You're tempted to add Kamino/MarginFi as a fallback flash loan source. We agreed: Solend only for MVP. Ask before expanding.
- Devnet integration tests fail because Solend isn't deployed there in usable form. (It may not be — confirm before assuming a workaround.)

### Phase 4 Pass Criteria

- [ ] All unit tests pass.
- [ ] All property tests pass (proptest default 256 cases).
- [ ] All 5 integration tests pass on devnet OR operator approves an alternative test plan.
- [ ] Output the Test Inventory table.

---

## 🔄 Phase 5: DEX Integrations (Wave 1 — Raydium AMM + Orca Whirlpools)

**Goal:** First two DEXs implementing `DexQuoter`. The pattern established here applies to all later DEXs.

This phase has TWO sub-phases:

### Phase 5A: `arb-dex-raydium` (Raydium AMM v4)

Source-of-truth: https://github.com/raydium-io/raydium-amm
Program ID: `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8`

#### Quote math
Constant product:
```
out = (in * (10000 - swap_fee_bps)) * reserve_out
      / (reserve_in * 10000 + in * (10000 - swap_fee_bps))
```
Default fees: 25 bps swap fee + 5 bps protocol fee. Some pools differ — read from account state.

#### Required functions

```rust
// crates/arb-dex-raydium/src/lib.rs

pub struct RaydiumAmmQuoter { /* config */ }

#[async_trait::async_trait]
impl DexQuoter for RaydiumAmmQuoter {
    fn dex_id(&self) -> &'static str { "raydium-amm-v4" }

    fn quote(&self, pool: &Pool, input: TokenAmount) -> ArbResult<TokenAmount>;

    fn build_swap_ix(&self, ...) -> ArbResult<Instruction>;

    fn try_parse_account(&self, pubkey: &Pubkey, data: &[u8]) -> Option<Pool>;
}
```

**Common pitfall:** Raydium pools have OpenBook market accounts. The swap instruction needs the OpenBook accounts even though we never touch the orderbook. Build the full account list correctly or the swap will fail.

#### Required tests for Phase 5A

##### Unit tests

| Test | What it verifies |
|------|------------------|
| `test_parse_amm_v4_account_valid` | Real on-chain account (fixture) parses correctly |
| `test_parse_amm_v4_account_wrong_size` | Wrong-size data returns None |
| `test_parse_amm_v4_account_wrong_discriminator` | Non-Raydium data returns None |
| `test_quote_constant_product_known_value` | For SOL/USDC pool with known reserves, quote matches expected output |
| `test_quote_zero_input` | Zero input → zero output |
| `test_quote_max_input` | Input > reserve_in / 2 still produces valid output (not overflow) |
| `test_quote_fee_applied` | Same input on pool with 25bps vs 100bps fee → second produces less |
| `test_swap_ix_account_count` | Swap ix has correct number of accounts (incl. OpenBook) |
| `test_swap_ix_account_order` | Each account at correct index |
| `test_swap_ix_min_amount_out_encoded` | `minimum_amount_out` is in instruction data |

##### Property tests

| Test | Property |
|------|----------|
| `prop_quote_monotonic_in_input` | Larger input → larger or equal output |
| `prop_quote_no_overflow` | No input within u64 produces overflow |
| `prop_quote_round_trip_within_fees` | quote(quote_inverse(out)) ≤ out (fees don't add value) |

##### Integration tests (`tests/integration/raydium_quote_match.rs`)

| Test | What it verifies | Environment |
|------|------------------|-------------|
| `quote_matches_simulation_sol_usdc` | Quote for 10 different inputs against real SOL/USDC pool matches `simulateTransaction` result within 1 unit | mainnet-fork |
| `quote_matches_simulation_random_pools` | Sample 5 pools from `tests/fixtures/raydium_pools.bin`, verify all quotes match | mainnet-fork |

#### How to fetch fixture pools

Create `scripts/fetch_pool_fixtures.sh`:
```bash
#!/usr/bin/env bash
# Fetch top Raydium pools by TVL into tests/fixtures/raydium_pools.bin
solana account <POOL_ADDRESS> --output-file tests/fixtures/<name>.bin --output binary
```

The operator should provide the list of pools to use. If not provided:

🛑 **Stop and ask:** "Which Raydium pools should I use for fixtures? At minimum I need SOL/USDC. Is there a list?"

### Phase 5B: `arb-dex-orca` (Orca Whirlpools)

Source: https://github.com/orca-so/whirlpools
Program ID: `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc`

Quote math is significantly more complex than constant-product — uses tick arrays. Use Orca's published quote math from their SDK or replicate carefully.

**Critical:** the tick arrays passed to a swap MUST be the ones the swap will actually traverse. If you pass too few, the swap reverts. Compute the expected tick traversal from the input amount + current pool state.

#### Required tests for Phase 5B

##### Unit tests

| Test | What it verifies |
|------|------------------|
| `test_parse_whirlpool_account_valid` | Real on-chain account parses correctly |
| `test_parse_whirlpool_wrong_account_size` | Wrong size returns None |
| `test_quote_within_single_tick_array` | Small swap that doesn't cross tick boundaries — quote correct |
| `test_quote_crosses_one_tick_boundary` | Mid-size swap crosses one boundary — quote correct |
| `test_quote_crosses_multiple_boundaries` | Large swap crosses 2+ boundaries — quote correct |
| `test_compute_tick_arrays_needed` | Helper returns correct array PDAs for a given swap |
| `test_quote_zero_input` | Zero input → zero output |
| `test_quote_above_liquidity` | Input that would exhaust pool returns InsufficientLiquidity |
| `test_swap_ix_includes_required_tick_arrays` | All needed tick array PDAs are in `remaining_accounts` |

##### Property tests

| Test | Property |
|------|----------|
| `prop_quote_monotonic_in_input` | Larger input → larger or equal output |
| `prop_tick_arrays_sufficient` | For random swap sizes, computed tick arrays cover the swap |

##### Integration tests

| Test | What it verifies | Environment |
|------|------------------|-------------|
| `whirlpool_quote_matches_sdk` | For 10 inputs, our quote matches `@orca-so/whirlpools-sdk` output bit-exact | mainnet-fork |
| `whirlpool_quote_matches_simulation` | For 10 inputs, our quote matches `simulateTransaction` within 1 unit | mainnet-fork |
| `whirlpool_swap_ix_lands_on_devnet` | Our swap ix executes successfully on devnet | devnet |

### 🛑 Phase 5 — Stop and ask if:
- For Raydium: you can't reproduce the on-chain quote math exactly. A 1-unit discrepancy is OK (rounding). A 100+ unit discrepancy means we got the formula wrong — ASK rather than fudge.
- For Orca: tick array computation is intricate. If you're unsure how to compute the array PDAs for a given swap direction, ASK. Don't guess — wrong tick arrays = guaranteed failure.
- The fixtures aren't available and you don't have a clear way to fetch them.
- You discover the SDK math has been recently updated (check the protocol's GitHub for breaking changes since this doc was written).

### Phase 5 Pass Criteria

- [ ] Both DEXs implement `DexQuoter` trait fully.
- [ ] All unit tests pass.
- [ ] All property tests pass.
- [ ] Integration test results show quote matches simulation/SDK to within 1 unit on **all** 10+ test cases per DEX.
- [ ] Devnet swap test passes for at least Orca (Raydium devnet support is uncertain — ask if blocked).
- [ ] Output the Test Inventory table.

---

## 📡 Phase 6: Streaming & Pool State (`arb-stream`)

**Goal:** Subscribe to Helius for real-time pool updates. Maintain a fast, lock-free in-memory map of pool states.

### Streaming backend selection

By Phase 6, the operator has reviewed the scout report and decided on a streaming tier:

- **Path A — Stay on Developer plan ($49/mo)**: Keep using **Enhanced WebSockets** (the same code from the scout). Latency is comparable to LaserStream for non-extreme workloads. The bot will be cost-efficient but may capture fewer of the most-contested opportunities.
- **Path B — Upgrade to Business plan ($499/mo)**: Use **LaserStream gRPC** via `yellowstone-grpc-client`. Lower latency, regional redundancy, replay capability. Recommended only if scout data showed many opportunities lost to slightly faster competitors.

**Default to Path A unless the operator explicitly chooses Path B.** The default reflects cost-conservatism. The architecture supports both — they implement the same `StreamingBackend` trait.

### Required types

```rust
// crates/arb-stream/src/lib.rs

#[async_trait::async_trait]
pub trait StreamingBackend: Send + Sync {
    async fn run(
        self: Arc<Self>,
        store: Arc<PoolStateStore>,
        update_tx: broadcast::Sender<PoolUpdate>,
    ) -> ArbResult<()>;
}

pub struct EnhancedWssBackend { /* ... */ }
pub struct LaserStreamBackend { /* ... */ }

impl StreamingBackend for EnhancedWssBackend { /* ... */ }
impl StreamingBackend for LaserStreamBackend { /* ... */ }

pub struct PoolStateStore {
    pools: Arc<DashMap<Pubkey, ArcSwap<Pool>>>,
}

impl PoolStateStore {
    pub fn get(&self, address: &Pubkey) -> Option<Arc<Pool>>;
    pub fn upsert(&self, pool: Pool);
}
```

### Subscription strategy (both backends)

1. On startup, fetch the full list of monitored pools (from `config/pools.toml` via `arb-config`).
2. Subscribe to all pools via the chosen backend.
3. Maintain `Arc<DashMap<Pubkey, ArcSwap<Pool>>>` keyed by pool address.
4. On each update: deserialize via the appropriate `DexQuoter::try_parse_account`, atomically swap into the map.
5. Notify subscribers (strategy engine) via `tokio::sync::broadcast::channel`.

### Reconnection logic (both backends)

- Exponential backoff: start 200ms, multiply by 2, cap 30s.
- After EVERY reconnect, re-fetch full account state for ALL monitored pools (we may have missed updates during disconnect).
- Track disconnection events in telemetry (count, total downtime).

### Required tests for Phase 6

#### Unit tests

| Test | What it verifies |
|------|------------------|
| `test_pool_state_store_upsert_replaces` | Upsert with same address replaces |
| `test_pool_state_store_concurrent_reads_writes` | Use `tokio::test` with N readers + 1 writer; no panics |
| `test_route_subscribers_receive_update` | Broadcast channel delivers to N subscribers |
| `test_subscribers_can_lag_without_blocking` | Slow subscriber doesn't block fast one (drop oldest) |
| `test_parser_dispatch_matches_pool_kind` | Update for Raydium account → only Raydium parser tries |
| `test_streaming_backend_trait_impls` | Both `EnhancedWssBackend` and `LaserStreamBackend` satisfy the trait (compile-time check) |

#### Integration tests

| Test | What it verifies | Environment |
|------|------------------|-------------|
| `enhanced_wss_subscribe_receives_updates` | Subscribe to mainnet pool via WSS, receive ≥ 1 update in 60s | mainnet (read-only) |
| `enhanced_wss_reconnect_resyncs` | Force-disconnect, verify reconnect + state refresh | mainnet (read-only) |
| `enhanced_wss_100_pools_1hour_stable` | Subscribe to 100 pools for 1 hour, no panics, no missed-state | mainnet (read-only) |
| `laserstream_subscribe_receives_updates` | Same as above, but via LaserStream gRPC | mainnet (read-only) — only if Path B chosen |
| `laserstream_reconnect_resyncs` | Same | mainnet (read-only) — only if Path B chosen |
| `latency_comparison_wss_vs_laserstream` | Run both for 1 hour; compare update arrival times | mainnet — only if both available |

### 🛑 Phase 6 — Stop and ask if:
- The operator hasn't decided between Path A (Enhanced WSS, Developer plan) and Path B (LaserStream gRPC, Business plan). The default is Path A.
- Helius streaming details aren't clear (the operator must confirm region: Frankfurt vs Amsterdam vs NY).
- The subscription requires a payment plan the operator hasn't confirmed.
- You hit a rate limit or connection limit you can't resolve via config.
- Update latency p99 exceeds 1 second on Path A — this would invalidate the bot's competitiveness; report and ask whether to switch to Path B before continuing.

### Phase 6 Pass Criteria

- [ ] Bot maintains live state for 100+ pools for 1 hour without crashes.
- [ ] Disconnect simulation: kill the network for 30s, bot reconnects and resyncs cleanly.
- [ ] Update latency (chain confirmation → state in memory) measured p99 < 500ms.
- [ ] Output the Test Inventory table.

---

## 🧭 Phase 7: Pathfinder & Strategy Engine (`arb-pathfinder`)

**Goal:** Find profitable arbitrage cycles and emit `Opportunity` events.

### Pathfinder

Builds a token graph; nodes = mints, edges = pools. For each token-of-interest pair, finds candidate cycles of length 2 and 3 (start in USDC, end in USDC).

**Algorithm:** modified Bellman-Ford with `log(price)` weights. We are looking for *negative cycles* in log-space — these are arbitrage opportunities.

**Optimization:** precompute candidate routes at startup. At runtime, only re-quote affected routes when their pools update.

### Required types

```rust
// crates/arb-pathfinder/src/lib.rs

pub struct Pathfinder {
    candidate_routes: Vec<Route>,        // precomputed at startup
    routes_by_pool: HashMap<Pubkey, Vec<RouteId>>,  // pool → routes that touch it
}

impl Pathfinder {
    pub fn build(pools: &[Pool], base_token: TokenMint) -> ArbResult<Self>;

    pub fn routes_touching(&self, pool: &Pubkey) -> &[RouteId];

    pub fn evaluate_route(
        &self,
        route_id: RouteId,
        store: &PoolStateStore,
        amount_in: u64,
    ) -> ArbResult<Option<Opportunity>>;

    /// Binary-search for the size that maximizes net profit.
    pub fn find_optimal_size(
        &self,
        route_id: RouteId,
        store: &PoolStateStore,
        max_size: u64,
    ) -> ArbResult<Option<(u64, u64)>>;  // (optimal_size, expected_profit)
}
```

### Required tests for Phase 7

#### Unit tests

| Test | What it verifies |
|------|------------------|
| `test_build_2_hop_routes_only` | Pathfinder limits cycles to ≤ 3 hops by default |
| `test_build_starts_and_ends_at_base` | All routes start and end at `base_token` |
| `test_build_no_self_loops` | No route uses the same pool twice |
| `test_routes_touching_returns_correct_set` | After build, `routes_touching(pool)` correct |
| `test_evaluate_unprofitable_returns_none` | Route with no price discrepancy → None |
| `test_evaluate_profitable_returns_opportunity` | Mock pools with 1% spread → Opportunity emitted |
| `test_evaluate_includes_all_fees` | Profit accounts for: swap fees, flash loan fee, expected priority fee, expected tip |
| `test_find_optimal_size_handles_concave_curve` | For mock pools, binary search finds within 1% of true optimum |
| `test_find_optimal_size_caps_at_max` | Returned size ≤ `max_size` |
| `test_find_optimal_size_returns_none_if_no_profit` | All sizes unprofitable → None |

#### Property tests

| Test | Property |
|------|----------|
| `prop_evaluate_deterministic` | Same inputs → same output |
| `prop_optimal_size_at_least_as_good_as_naive` | Optimal profit ≥ profit at half optimal size |

#### Integration tests

| Test | What it verifies | Environment |
|------|------------------|-------------|
| `pathfinder_finds_opportunity_against_mainnet_fork` | Set up a synthetic mispricing in mainnet-fork, verify pathfinder identifies it | fork |
| `pathfinder_paper_trading_opportunity_rate` | Run for 1 hour against live mainnet data; expect ≥ 5 opportunities/hour | mainnet (read-only) |

### 🛑 Phase 7 — Stop and ask if:
- The default opportunity rate (5/hour) seems unachievable with current DEX coverage. This may indicate either (a) buggy quote math (Phase 5 issue), (b) too-strict `min_profit` threshold, or (c) the bot is just less competitive than expected. Report numbers and ask.
- The graph blows up combinatorially (>10,000 candidate routes). Pathfinder should prune aggressively; if it doesn't, ask before adding heuristics.
- You're considering 4+ hop routes. Don't — stick with 2-3 unless operator approves.

### Phase 7 Pass Criteria

- [ ] All unit tests pass.
- [ ] All property tests pass.
- [ ] Mainnet-fork integration test produces correct opportunity for synthetic mispricing.
- [ ] 1-hour paper-trading run identifies ≥ 5 opportunities/hour with simulated profit > 50,000 lamports each.
- [ ] Output the Test Inventory table.

---


## 📦 Phase 8: Jito Bundle Submission (`arb-jito`)

**Goal:** Build and submit Jito bundles. Handle simulation, tip strategy, polling, and failure analysis.

### Bundle layout (final)

A bundle is up to 5 transactions. We use exactly **1 transaction** containing the full arb + assert + tip.

### Required functions

```rust
// crates/arb-jito/src/lib.rs

pub struct JitoClient {
    block_engine_url: String,
    http: reqwest::Client,
    tip_accounts: Vec<Pubkey>,        // refreshed periodically
}

impl JitoClient {
    pub async fn new(config: &JitoConfig) -> ArbResult<Self>;

    pub async fn refresh_tip_accounts(&mut self) -> ArbResult<()>;

    pub async fn get_tip_floor(&self) -> ArbResult<u64>;

    pub fn pick_tip_account(&self) -> Pubkey;     // round-robin

    pub async fn simulate_bundle(
        &self,
        transactions: &[VersionedTransaction],
    ) -> ArbResult<SimulationResult>;

    pub async fn send_bundle(
        &self,
        transactions: &[VersionedTransaction],
    ) -> ArbResult<BundleId>;

    pub async fn get_bundle_status(
        &self,
        id: &BundleId,
    ) -> ArbResult<BundleStatus>;
}

pub struct BundleBuilder {
    payer: Keypair,
    assert_program_id: Pubkey,
    user_token_account: Pubkey,
    base_token_decimals: u8,
}

impl BundleBuilder {
    pub fn build_arb_transaction(
        &self,
        opportunity: &Opportunity,
        flash_loan_envelope: &FlashLoanEnvelope,
        swap_ixs: &[Instruction],
        min_profit_lamports: u64,
        priority_fee_microlamports: u64,
        compute_unit_limit: u32,
        jito_tip_account: Pubkey,
        jito_tip_lamports: u64,
        recent_blockhash: Hash,
    ) -> ArbResult<VersionedTransaction>;
}
```

### Tip strategy

- Start with static tip from `.env` (default 10,000 lamports).
- Phase 5+: query `getTipFloor` (Jito API) every minute, cap tip at `min(tip_floor * 1.5, expected_profit * 0.30)`.
- Never tip more than 30% of expected profit. If tip floor exceeds that ceiling, skip the opportunity.
- Tip account: rotate through the 8 Jito tip account PDAs (`getTipAccounts` returns the list).

### Failed-bundle handling

- Log every attempt with: opportunity ID, route, expected vs actual, slot, reason.
- Track per-DEX-pair failure counter. If 5 consecutive failures of the same shape — pause that pair for 60 seconds.
- If 20 consecutive total failures — kill switch: pause bot, alert Telegram operator.

### Required tests for Phase 8

#### Unit tests

| Test | What it verifies |
|------|------------------|
| `test_bundle_builder_produces_valid_tx` | Output is a well-formed `VersionedTransaction` |
| `test_bundle_builder_includes_compute_budget` | First instructions are SetComputeUnitLimit + SetComputeUnitPrice |
| `test_bundle_builder_includes_assert_last_before_tip` | `assert_min_profit` is second-to-last; tip transfer is last |
| `test_bundle_builder_assert_uses_correct_min_balance` | min_balance = current_balance + min_profit_lamports |
| `test_bundle_builder_tip_transfer_to_jito_account` | Tip transfer destination is one of the 8 Jito tip accounts |
| `test_pick_tip_account_round_robin` | 8 calls return all 8 accounts |
| `test_get_tip_floor_handles_response` | Mock HTTP response → returns u64 |
| `test_get_tip_floor_handles_error` | HTTP error → returns ArbError, doesn't panic |

#### Property tests

| Test | Property |
|------|----------|
| `prop_assert_min_balance_equals_pre_plus_profit` | For random profits, min_balance encoding correct |

#### Integration tests

| Test | What it verifies | Environment |
|------|------------------|-------------|
| `simulate_profitable_bundle_succeeds` | Build bundle that should profit, simulate → success | mainnet-fork |
| `simulate_unprofitable_bundle_assert_fails` | Build bundle that won't profit, simulate → assert ix fails | mainnet-fork |
| `send_bundle_to_devnet_get_id` | Send a bundle, receive bundle ID from Jito | devnet (if Jito devnet available) OR mainnet small test |
| `bundle_status_polling_terminates` | After send, poll status; eventually returns terminal state | live |
| `bundle_with_failing_swap_doesnt_lose_principal` | Bundle where swap B fails — final balance unchanged | mainnet-fork |

### 🛑 Phase 8 — Stop and ask if:
- Jito doesn't operate on devnet (it likely doesn't). Confirm with operator how to run integration tests — options are: (a) skip integration on devnet, run only on mainnet with tiny amounts, (b) mock Jito's HTTP API.
- The 30% tip cap conflicts with operator's preferred strategy.
- A bundle simulation succeeds but the actual send fails repeatedly with the same error — possible Jito region issue. Ask before changing endpoints.
- You see "uncled block" / "skipped slot" failures — these are expected occasionally, but >5% rate is concerning.

### Phase 8 Pass Criteria

- [ ] All unit tests pass.
- [ ] All property tests pass.
- [ ] Mainnet-fork integration tests pass.
- [ ] Live integration tests pass (with operator approval for any mainnet expenditure).
- [ ] 100% of unprofitable bundles correctly fail at the assert step in simulation.
- [ ] Output the Test Inventory table.

---

## 🤖 Phase 9: Telemetry & Telegram Bot

**Goal:** Operator can monitor and control the bot from Telegram.

### Phase 9A: `arb-telemetry`

```rust
// crates/arb-telemetry/src/lib.rs

pub struct Telemetry {
    log_dir: PathBuf,
    state_dir: PathBuf,
    pnl: Arc<RwLock<PnlState>>,
}

impl Telemetry {
    pub fn record_attempt(&self, attempt: &BundleAttempt);
    pub fn record_outcome(&self, attempt_id: AttemptId, outcome: BundleOutcome);
    pub fn pnl_for(&self, period: Period) -> PnlSummary;
    pub fn recent_attempts(&self, n: usize) -> Vec<BundleAttempt>;
    pub fn dropped_update_count(&self) -> u64;
    pub fn streaming_latency_p99(&self) -> Duration;
}
```

Persist to `state/pnl.json` (atomic write). Logs to `logs/bot-YYYY-MM-DD.jsonl` (append-only).

### Phase 9B: `arb-telegram`

#### Commands

| Command | Description |
|---------|-------------|
| `/status` | Bot uptime, # opportunities/hour, last bundle status |
| `/pnl [today\|week\|month\|all]` | Net P&L for period |
| `/pause` | Stop submitting bundles (keep streaming for analytics) |
| `/resume` | Resume submission |
| `/kill` | Cancel all in-flight bundles, exit safely |
| `/tune <param> <value>` | Adjust e.g. `min_profit_lamports`, `default_tip`, `max_flashloan` |
| `/whitelist <token>` | Add token to allowed list |
| `/blacklist <token>` | Remove token from allowed list |
| `/dexes` | List active DEXs and their last-update timestamps |
| `/balance` | Wallet balance breakdown |
| `/recent [n]` | Last n bundle results (default 10) |
| `/help` | List all commands |

#### Security

- Hardcode `TELEGRAM_OPERATOR_CHAT_ID` check on every message handler.
- Reject all messages not from the configured chat ID (silent reject — do not log them, do not respond).
- Never log keypair contents to Telegram.
- For `/tune`, require typed confirmation: bot replies with current value, operator must reply `confirm` within 30s.
- For `/kill`, log the action and the chat ID even though it's authorized.

### Required tests for Phase 9

#### Unit tests (telemetry)

| Test | What it verifies |
|------|------------------|
| `test_record_attempt_persists` | Write attempt → readable from log file |
| `test_pnl_summation_correct` | 5 attempts with known outcomes → correct net P&L |
| `test_pnl_period_filtering` | `today` excludes yesterday's attempts |
| `test_state_save_atomic` | Crash mid-write doesn't corrupt state file (use tempfile + rename) |
| `test_recent_attempts_returns_in_reverse_chronological` | Most recent first |
| `test_dropped_update_count_increments` | Counter correctly tracks |

#### Unit tests (telegram)

| Test | What it verifies |
|------|------------------|
| `test_command_unauthorized_chat_id_silently_rejected` | Other chat ID → no response, no log |
| `test_command_status_format` | `/status` produces well-formatted message |
| `test_command_pnl_today` | `/pnl today` returns today's P&L |
| `test_command_pause_changes_state` | `/pause` flips the global submit-enabled flag |
| `test_command_kill_cancels_in_flight` | `/kill` cancels pending bundles |
| `test_command_tune_requires_confirmation` | `/tune` replies and waits; un-confirmed change doesn't apply |
| `test_command_tune_confirmation_timeout` | After 30s without `confirm`, change is dropped |
| `test_command_unknown_returns_help` | Unrecognized command → reply with `/help` reference |

#### Integration tests

| Test | What it verifies | Environment |
|------|------------------|-------------|
| `telegram_e2e_status_command` | Real bot responds to `/status` from the configured chat ID | local |
| `telegram_e2e_unauthorized_silent` | Real bot ignores message from a different chat ID | local |

### 🛑 Phase 9 — Stop and ask if:
- Telegram bot token isn't configured; you can't run integration tests.
- The operator wants additional commands not listed above.
- The `/kill` semantics are unclear (graceful shutdown vs immediate exit).

### Phase 9 Pass Criteria

- [ ] All telemetry tests pass.
- [ ] All telegram tests pass.
- [ ] All commands listed work in dryrun mode.
- [ ] Unauthorized chat ID receives no response (silent reject confirmed).
- [ ] Output the Test Inventory table.

---

## 🚀 Phase 10: Orchestrator & Main Binary (`arb-bot`)

**Goal:** Wire everything together into the runnable binary.

### Required structure

```rust
// crates/arb-bot/src/main.rs

#[tokio::main]
async fn main() -> ArbResult<()> {
    setup_logging()?;
    let config = Config::load()?;
    config.validate()?;

    let store = PoolStateStore::new();
    let parsers = build_parsers(&config);                 // Vec<Arc<dyn DexQuoter>>
    let pathfinder = Pathfinder::build(&config.pools, USDC_MINT)?;
    let jito = JitoClient::new(&config.jito).await?;
    let telemetry = Telemetry::new(&config)?;

    let stream = LaserStreamClient::new(&config, parsers.clone(), store.clone());
    let strategy = StrategyEngine::new(pathfinder, store.clone(), parsers.clone());
    let executor = Executor::new(jito, telemetry.clone(), config.clone());
    let telegram = TelegramBot::new(&config, telemetry.clone());

    tokio::try_join!(
        stream.run(),
        strategy.run(executor.tx()),
        executor.run(),
        telegram.run(),
        health_check_server(),
    )?;

    Ok(())
}
```

### Strategy engine main loop

```rust
loop {
    let update = pool_update_rx.recv().await?;
    let affected = pathfinder.routes_touching(&update.pool);
    for route_id in affected {
        match pathfinder.evaluate_route(route_id, &store, BASE_AMOUNT) {
            Ok(Some(opportunity)) => {
                if executor_can_accept() {
                    executor_tx.send(opportunity).await?;
                }
            }
            Ok(None) => {} // unprofitable, skip
            Err(e) => tracing::warn!(?e, "evaluate_route failed"),
        }
    }
}
```

### Executor main loop

```rust
loop {
    let opportunity = opportunity_rx.recv().await?;
    if !submit_enabled.load() { continue; }    // operator-paused

    let attempt_id = telemetry.record_attempt(&opportunity);
    match build_and_simulate(&opportunity).await {
        Ok(bundle) if bundle.profit_lamports >= config.min_profit_lamports => {
            let outcome = jito.send_and_wait(&bundle).await;
            telemetry.record_outcome(attempt_id, outcome);
        }
        Ok(_) => telemetry.record_outcome(attempt_id, BundleOutcome::SkippedUnprofitable),
        Err(e) => telemetry.record_outcome(attempt_id, BundleOutcome::SimulationFailed(e)),
    }
}
```

### Required tests for Phase 10

#### Integration tests (`tests/integration/full_arb_dryrun.rs`)

| Test | What it verifies | Environment |
|------|------------------|-------------|
| `full_pipeline_dryrun_24h` | Bot runs in dryrun for 24 hours; identifies ≥ 100 simulated opportunities | mainnet (read-only) |
| `full_pipeline_handles_disconnect` | Force-disconnect LaserStream; bot recovers without restart | mainnet |
| `full_pipeline_telegram_pause_works` | Pause via Telegram; verify no bundles submitted; resume → bundles flow | mainnet (read-only mode) |
| `full_pipeline_health_endpoint` | `GET http://localhost:9090/health` returns 200 with status | local |
| `full_pipeline_graceful_shutdown` | SIGTERM → bot finishes in-flight, exits cleanly within 10s | local |

### 🛑 Phase 10 — Stop and ask if:
- The full pipeline test fails due to one component but it's unclear which.
- The 24-hour dryrun finds < 100 opportunities — likely a quote-math bug elsewhere; ask before changing thresholds.
- Memory usage grows unbounded over the 24-hour run.

### Phase 10 Pass Criteria

- [ ] All integration tests pass.
- [ ] 24-hour dryrun shows: ≥ 100 opportunities identified; zero panics; memory stable.
- [ ] Telegram control verified end-to-end.
- [ ] Output the Test Inventory table.

---

## 🧪 Phase 11: Acceptance Phases (sequential gate, no skipping)

This is where we move from "compiles and tests pass" to "ready to risk real SOL."

### Phase 11.1 — Mainnet-fork dry-run (free, local)

```bash
./scripts/run_dryrun.sh
```

- Run `solana-test-validator` with `--clone <pool_address>` for top 50 monitored pools.
- Run bot in `dryrun` mode against this fork.
- Run for 24 hours.

**Pass criteria:**
- ✅ No panics.
- ✅ ≥ 100 simulated opportunities logged.
- ✅ All simulations show profit ≥ threshold.
- ✅ Memory usage stable (< 2 GB).
- ✅ CPU usage reasonable (< 50% on 4 cores).

### Phase 11.2 — Devnet end-to-end (free, devnet airdrop)

- Deploy assert program to devnet (Phase 3 already did this).
- Set up devnet-equivalent token mints and pools (limited DEX coverage on devnet — accept this).
- Run bot in mainnet mode but pointed at devnet RPC.
- Submit ≥ 10 live bundles.

**Pass criteria:**
- ✅ ≥ 10 bundles submitted.
- ✅ All 10 either succeed with profit ≥ 0 OR fail cleanly at the assert step.
- ✅ Zero principal lost (final balance ≥ initial balance).
- ✅ All bundle attempts logged correctly.
- ✅ Telegram `/recent 10` shows correct outcomes.

### Phase 11.3 — Mainnet paper-trading (free, observation only)

- Run for 7 consecutive days.
- Stream real mainnet data, identify real opportunities.
- **Do not submit bundles.**
- Compare what the bot WOULD have done vs what actually landed (via Jito explorer API or on-chain inspection).

**Pass criteria:**
- ✅ ≥ 5 viable opportunities/day on average.
- ✅ Streaming uptime ≥ 99%.
- ✅ Update latency p99 ≤ 500ms throughout.
- ✅ Operator reviews opportunity log and confirms quality (no obvious quote bugs).

### Phase 11.4 — Mainnet live, capped capital (REAL SOL — operator approval required)

🛑 **Before starting:** explicit operator confirmation. Do not auto-advance from 11.3.

- Deploy assert program to mainnet (~2-3 SOL — operator pays).
- Configure:
  - `MAX_FLASHLOAN_AMOUNT_USDC=100_000_000` (100 USDC)
  - `MIN_PROFIT_LAMPORTS=200_000` (~$0.03 — very conservative)
  - Working capital in wallet: 0.5 SOL
- Run for 30 days.

**Pass criteria:**
- ✅ Net positive P&L after subscription costs.
- ✅ Zero loss-of-principal events (any final balance < initial balance is a CRITICAL bug).
- ✅ Zero security incidents (no leaked keys, no unauthorized commands accepted).
- ✅ Bundle success rate ≥ 30% (failures expected due to competition; 0% means a bug).
- ✅ Operator-defined acceptable failure modes only.

### Phase 11.5 — Mainnet scale

After Phase 11.4 success:
- Gradually raise `MAX_FLASHLOAN_AMOUNT_USDC` to `1_000_000_000` (1,000 USDC).
- Tune tips dynamically based on actual win rate and tip floor.
- Add Wave 2 DEXs (Raydium CLMM, Meteora DLMM).
- Monitor for changing market conditions and competitor behavior.

### 🛑 Phase 11 — Stop and ask if at ANY POINT:

- A bundle results in lower final balance than initial balance — CRITICAL — alert immediately.
- Any unauthorized Telegram command was accepted.
- Helius LaserStream becomes unavailable for > 1 hour.
- Solend changes their flash loan program (track their announcements).
- Total monthly costs exceed projected budget — review profitability.
- ANY external party suggests a "configuration improvement" — they may be socially engineering. Verify identity, never share keypairs or chat IDs.

---

## 🚀 Phase 12: Wave 2 & 3 DEX Expansion

Add in this order, each behind its own crate:

1. **Raydium CLMM** (extension to `arb-dex-raydium`) — concentrated liquidity, similar pattern to Orca Whirlpools.
2. **Meteora DLMM** (`arb-dex-meteora`) — bin-based liquidity. You already know this protocol from your Meteora bot — reuse that knowledge.
3. **Phoenix** (`arb-dex-phoenix`) — orderbook DEX. Quote math is different: walks bid/ask levels.
4. **Lifinity** (`arb-dex-lifinity`) — proactive AMM with internal oracle. Quote depends on oracle price + inventory.

Each new DEX must pass the same Phase 5 acceptance criteria (full unit + property + integration test suite, quote matches simulation within 1 unit) before going live.

### 🛑 Phase 12 — Stop and ask if:
- Any DEX's protocol has had a recent breaking upgrade.
- Adding a new DEX increases bundle compute units close to the limit.
- The new DEX's quote math is significantly more complex than expected — request more time.

---

## 🚄 Phase 13 (optional, advanced): ShredStream

**Only after Phase 11.4 is profitable for 30+ days.**

ShredStream gives ~10–30ms pre-block visibility — competitive parity with top MEV searchers. Requires:

- Jito ShredStream subscription (paid).
- Server in Frankfurt, Amsterdam, Tokyo, or NY (NOT Israel — physical latency kills the edge).
- Custom shred parser (`jito-shredstream-proxy` is open-source).
- Strategy update: backrun pending swaps instead of (or in addition to) reactive arb.

Cost: $1–3k/month server + Jito sub. Justify with paper-trading data showing ≥ $5k/month additional profit.

🛑 **Phase 13 starts with operator decision.** Do not auto-advance.

---

## 📊 Logging & Observability

- `tracing` for all logs. Levels: `error`, `warn`, `info`, `debug`, `trace`.
- Structured logs: every bundle attempt logged with `bundle_id`, `route_id`, `expected_profit`, `actual_profit`, `tip`, `slot`, `outcome`.
- Persist to `logs/bot-YYYY-MM-DD.jsonl`. Telegram `/recent` reads from these.
- P&L computation runs every 5 minutes; cached in memory + persisted to `state/pnl.json`.
- Health endpoint: `GET http://localhost:9090/health` → JSON with uptime, last bundle status, dropped update count.

---

## ✅ Final Acceptance Checklist

Before declaring "done" and running mainnet with real capital:

- [ ] **Phase −1 (paper-trading scout) completed; operator approved advancing.**
- [ ] All workspace crates build with `cargo build --release` (zero warnings).
- [ ] All unit tests pass (`cargo test --workspace`).
- [ ] All property tests pass.
- [ ] All integration tests pass (per phase).
- [ ] Anchor program tests pass (`anchor test`).
- [ ] Code coverage meets thresholds (Phase 0 standard).
- [ ] Phase 11.1 (mainnet-fork dryrun) passes.
- [ ] Phase 11.2 (devnet) passes.
- [ ] Phase 11.3 (paper-trading, 7 days) passes.
- [ ] Telegram bot responds to all commands; rejects unauthorized chats.
- [ ] Operator runbook (`docs/RUNBOOK.md`) written.
- [ ] Monitoring health endpoint operational.
- [ ] Backup keypair stored offline by operator.
- [ ] `MAX_FLASHLOAN_AMOUNT_USDC` set to conservative initial value.
- [ ] All Phase 11 stop-and-ask triggers acknowledged by operator.

When in doubt, raise it on Telegram before changing config. Manual approval > clever automation when real money is at stake.

---

## 🆘 Common Failures & Fixes

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| Bundle silently rejected by Jito | Tip too low for current floor | Query `getTipFloor`; raise tip |
| Bundle lands as failed tx | Race lost — pool moved between sim and execution | Expected; assert program ensures no loss. If frequent, increase `min_profit` buffer |
| `RefreshReserve` returns stale | Solend reserve not updated this slot | Add another `RefreshReserve` immediately before borrow |
| LaserStream disconnects every few minutes | Network instability or rate limit | Check Helius dashboard; consider region fallback |
| `Whirlpools` swap reverts with `TickArrayOutOfBounds` | Insufficient tick arrays passed | Recompute traversal; pass 3 arrays to be safe |
| Anchor program rejects valid balance | Wrong token account passed | Verify ATA derivation matches `spl-associated-token-account` |
| High simulation success but low landing rate | Latency to Jito region | Switch region; consider ShredStream (Phase 13) |
| `cargo test` fails with `solana-test-validator: command not found` | Missing toolchain | `sh -c "$(curl -sSfL https://release.anza.xyz/stable/install)"` |
| Anchor IDL mismatch error | Anchor CLI version ≠ Cargo dep | Match versions; `avm install 0.30.0 && avm use 0.30.0` |
| Property tests fail intermittently | Non-determinism leaking | Find and eliminate; never mark `#[ignore]` to "fix" |

---

## 🛑 What NOT to do

- ❌ Do not add CEX price feeds "to validate spread direction." DEX prices are the truth.
- ❌ Do not skip the assert instruction "to save 5,000 CU." This is the safety net.
- ❌ Do not increase capital before passing the phase gates.
- ❌ Do not run multiple bot instances on the same wallet — concurrent transactions cause nonce conflicts.
- ❌ Do not auto-upgrade dependencies without testing — Solana SDK breaking changes are common.
- ❌ Do not commit `.env`, keypair files, or anything in `state/` to git.
- ❌ Do not skip simulation. Even when latency-pressed, simulation costs nothing and prevents disasters.
- ❌ Do not mark failing tests as `#[ignore]` to "ship now and fix later." Failing tests block the phase gate.
- ❌ Do not "guess" Solend or DEX account layouts. ASK or read the source.
- ❌ Do not add a "fallback" RPC provider behind operator's back. Helius only.
- ❌ Do not auto-deploy to mainnet. Every mainnet deploy needs explicit operator approval.

---

## 📚 References

- Solend flash loan docs: https://docs.save.finance/developers/flash-loans
- Solend source: https://github.com/solendprotocol/solana-program-library
- Jito bundle docs: https://docs.jito.wtf/lowlatencytxnsend/
- Jito Rust SDK: https://github.com/jito-labs/jito-rust-rpc
- Helius LaserStream: https://docs.helius.dev/data-streaming/laserstream
- Anchor framework: https://www.anchor-lang.com/
- Yellowstone gRPC: https://github.com/rpcpool/yellowstone-grpc
- Raydium AMM source: https://github.com/raydium-io/raydium-amm
- Orca Whirlpools: https://github.com/orca-so/whirlpools
- Meteora DLMM: https://github.com/MeteoraAg/dlmm-sdk
- Phoenix: https://github.com/Ellipsis-Labs/phoenix-v1
- Solana program library: https://github.com/solana-program

---

## 🎯 Final Note to Claude Code

You are building real financial infrastructure. The bot will run with real money. The operator (Osher) is highly technical and has built similar systems before — he will catch corners you cut.

Three guiding principles:

1. **Test before you ship a phase.** No phase advances without its full test inventory passing.
2. **Ask before you guess.** Every guess on a blockchain is a potential SOL loss. Asking is cheap.
3. **Be honest about what you don't know.** "I'm not sure how Solend handles X — please clarify" is always the right answer when you're not sure.

When you finish each phase, output:
1. The Test Inventory table.
2. A one-paragraph summary of what was built.
3. Any open questions or assumptions made.
4. The phase gate status: ✅ READY FOR PHASE N+1 or ❌ BLOCKED.

If you're ever uncertain whether to proceed: STOP and ASK.

**End of instructions. Build in order. Test at every phase gate. Never skip the assert.**
