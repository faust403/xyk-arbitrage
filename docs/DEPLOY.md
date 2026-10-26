# Scout Deploy Guide — GitHub + Railway

**Goal:** ship the Phase −1 scout to Railway and let it run for 7 days,
collecting opportunity + correlation data. At end of week, `cargo run --bin
scout-report` produces `docs/SCOUT_REPORT.md` with the GO / RECONSIDER verdict.

This guide assumes:
- You have a GitHub account
- Rust + cargo work locally (`cargo --version` returns 1.86.0)
- `.env` is already populated and tested locally with a 5-minute smoke run

---

## Step 1 — Initialize git + push to GitHub (~5 min)

```powershell
# In C:\Users\ASUS\Documents\arbbot
cd C:\Users\ASUS\Documents\arbbot

# Initialize repo (skip if already done)
git init -b main

# Sanity: confirm .env and logs/ are gitignored (they should be — already in .gitignore)
git status --short
# .env should NOT appear. If it does, STOP and add it to .gitignore now.

# Stage and commit
git add .
git commit -m "Phase -1 scout — initial deploy build"

# Create private GitHub repo via gh CLI (faster than the web UI)
# If you don't have gh: https://cli.github.com/
gh auth login              # one-time; pick GitHub.com + HTTPS + browser
gh repo create arbbot --private --source=. --push --description "Solana arb scout — Phase -1"

# OR manually: create the repo on github.com first, then:
#   git remote add origin git@github.com:<your-username>/arbbot.git
#   git push -u origin main
```

After this step, `https://github.com/<you>/arbbot` exists and contains
everything except `.env`, `logs/`, `state/`, and `target/` (per `.gitignore`).

---

## Step 2 — Install Railway CLI (~3 min)

```powershell
# PowerShell installer (recommended on Windows)
iwr -useb https://railway.app/install.ps1 | iex

# Verify
railway --version

# Log in (opens browser; one-time)
railway login
```

---

## Step 3 — Create Railway project linked to your repo (~5 min)

```powershell
# In C:\Users\ASUS\Documents\arbbot
railway init
# Prompts:
#   - "Project name?" → arbbot-scout (or whatever you like)
#   - "Empty project / Template?" → choose "Empty Project"
#
# This creates a Railway project and writes .railway/ metadata locally.
```

Then in the Railway dashboard (https://railway.app/dashboard):

1. **Open your new project** → click "+ New" → "GitHub Repo"
2. Connect to the `arbbot` GitHub repo you just pushed
3. Railway auto-detects the `Dockerfile` in the repo root and starts a build

---

## Step 4 — Set Region + Volume + env vars (~5 min)

In the Railway dashboard, with your project open:

### 4a. Region (CRITICAL — must be EU-West)

- **Settings → Environment → Region** → **EU West (Netherlands)**
- This is the lowest-latency region to Solana validators outside Frankfurt
- ⛔ Do NOT pick a US region — adds ~100ms latency, kills the test's validity

### 4b. Persistent volume for logs

- **Settings → Volumes** → **+ New Volume**
- **Mount path:** `/app/logs`
- **Size:** `1 GB` (enough for ≥ 7 days of JSONL)

### 4c. Environment variables (**Variables** tab)

Add these (one row each):

| Key | Value | Notes |
|-----|-------|-------|
| `HELIUS_API_KEY` | `xxxxxxxx-xxxx-…` (your key) | Same key from local `.env` |
| `RUN_MODE` | `scout` | |
| `RUST_LOG` | `info,arb_scout=info` | |
| `SCOUT_LOG_DIR` | `/app/logs` | Matches the volume mount |
| `TELEGRAM_BOT_TOKEN` | `<bot_id>:<token>` | Optional — enables hourly Telegram |
| `TELEGRAM_OPERATOR_CHAT_ID` | `<your_chat_id>` | Optional |

Other defaults (`SCOUT_PROBE_AMOUNT_USDC`, `SCOUT_BASE_TIP_LAMPORTS`, etc.)
are read from code defaults if not set — leave blank unless you want to tune.

---

## Step 5 — Deploy + verify (~10 min)

Railway should already be building from the GitHub push. To force a fresh
deploy after the env-var changes:

```powershell
railway up
```

Or click **Deploy** in the dashboard.

**First build takes ~5 minutes** — Rust 1.86 image, full release build of
arb-core + arb-scout + transitive solana-* deps. Subsequent deploys are faster
(Docker layer caching).

When the build finishes, check the **Logs** tab. You should see (within ~30s):

```
INFO arb_scout: scout configured pools=17
INFO arb_scout::config: loaded .env path=...      (will fail — Railway sets via env, no .env file — that's OK)
INFO arb_scout::config: Telegram bot token loaded (87356141…, 46 chars)
INFO arb_scout::config: Telegram chat id loaded: 429136864
INFO arb_scout::scout: bootstrap: fetching pool accounts count=17
INFO arb_scout::scout: bootstrap: fetching vault accounts count=2
INFO arb_scout::scout: bootstrap: fetching CLMM AmmConfigs count=3
INFO arb_scout::scout: bootstrap complete pools_loaded=17 vaults_loaded=2 clmm_fees_loaded=3
INFO arb_scout::wss: WSS connected; subscribing to 19 accounts
INFO scout::sol_price: SOL price updated price_usd=...
INFO scout::stats: scout stats updates=N ... sol_price_usd=92.77 ...    (every 30s)
```

**Within 30 minutes you should also receive a Telegram message** with the
"Scout summary (15min)" header — confirms end-to-end delivery.

---

## Step 6 — Monitoring during the 7-day run

```powershell
# Stream live logs from your terminal:
railway logs

# Or in dashboard: Project → Deployments → latest → "View Logs"
```

What to watch:
- `pools_in_store` should equal `17` (or 16 if Lifinity address is wrong)
- `vault_updates` should keep climbing (~3-5/sec for active markets)
- `wss_reconnects` should stay near `0` (occasional reconnects are fine)
- `correlated > 0` after first opportunity (~30s after first Ok(Some))
- Telegram message every 15 min — if it stops, something crashed; check logs

---

## Step 7 — End of week: pull data + generate report (~5 min)

After 7 days:

```powershell
# Download the JSONL logs from Railway's volume.
# Option A — via railway CLI:
railway run cat /app/logs/scout-2026-05-09.jsonl > logs/scout-2026-05-09.jsonl
# Repeat for each day's file. Same for correlation-*.jsonl files.

# Option B — SSH-style:
railway shell
# Inside the shell: tar czf /tmp/all-logs.tar.gz /app/logs/
# Exit, then: railway run cat /tmp/all-logs.tar.gz > all-logs.tar.gz
# Locally: tar xzf all-logs.tar.gz

# Generate the verdict report:
cargo run --release --bin scout-report
# Output: docs/SCOUT_REPORT.md
```

Open `docs/SCOUT_REPORT.md` and read the **TL;DR — Verdict** at the top:

- ✅ **GO** — proceed to Phase 0 (build the full bot, ~3 months)
- 🟨 **INCONCLUSIVE** — extend run by 7 more days, OR add Phoenix + OpenBook
- ❌ **RECONSIDER** — at least one threshold failed; do NOT build the full bot

The verdict is computed from 5 metrics including the **capture-rate-adjusted
monthly profit estimate** (Section 6 + 8 of the report). This is the number
that should drive your decision.

---

## Step 8 — Tear down when done (~2 min)

If RECONSIDER:
```powershell
# Stop the Railway deployment to stop the meter:
# Dashboard → Settings → "Delete Service" (keeps the project, can revive later)
# or delete the whole project.
```

If GO:
- Keep the scout running while you build the full bot — comparison data is
  useful for sanity-checking the production bot's numbers
- Schedule a session to begin Phase 0

---

## Common issues

| Symptom | Cause | Fix |
|---------|-------|-----|
| `cargo build` fails on Railway with "linker error" | Rust 1.86 image missing libs | Check Dockerfile uses `rust:1.86-slim` (already correct) |
| All `vault_updates=0` after bootstrap | Helius RPC rate-limited mid-bootstrap | Bump Helius plan, or reduce pool count |
| `wss_reconnects > 50/day` | Network instability or Helius rejection | Check Helius dashboard for usage; possibly hit rate limit |
| Telegram silent | Most likely env var typo | `railway logs` → look for `INFO arb_scout::config: Telegram ... loaded` |
| `pools_in_store < 17` | One of the pool addresses wrong | Check WARN lines in startup logs — they name the bad pool |

---

## Cost expectations

- **Helius Developer plan:** $49/month (already set up)
- **Railway:** ~$5-15/month for the scout's resource footprint
- **Total during 7-day run:** ~$15-30 prorated

If the scout shows GO, switch to:
- **Helius Business:** $499/month (for LaserStream gRPC, Phase 6+)
- **Railway:** $20-40/month (for full bot resource needs)

If RECONSIDER: **stop now, save $500+/month plus 3 months of dev time.**
