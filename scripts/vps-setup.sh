#!/usr/bin/env bash
#
# arbbot VPS setup — single-shot bootstrap for a fresh Ubuntu 22.04 / Debian 12
# host. Run *as a non-root user with sudo* (Latitude.sh / Hetzner / OVH default
# user is `root` — `adduser bot && usermod -aG sudo bot && su - bot` first).
#
# Goal: get arbbot running in <10 minutes on a VPS that lives <10ms from
# Jito Block Engine (Amsterdam / Frankfurt). Replaces the Railway Frankfurt
# deployment (~150ms RTT) which can't win the bundle auction.
#
# ===========================================================================
# Pre-flight checklist (do these manually first)
# ===========================================================================
#   [ ] Provision a VPS in Amsterdam (Latitude.sh AMS / Hetzner Falkenstein /
#       OVH Gravelines), >= 4 vCPU, 16GB RAM, 100GB SSD, Ubuntu 22.04.
#   [ ] SSH in as root: `ssh root@<IP>`
#   [ ] Create a non-root user: `adduser bot && usermod -aG sudo bot`
#   [ ] Switch user: `su - bot`
#   [ ] Have ready: GitHub repo URL/access, all Railway env vars (`railway
#       variables --json` output saved locally), wallet keypair JSON array.
#   [ ] Run THIS script. Expect prompts for repo URL + paste env vars at /etc/arbbot/.env.
#
# ===========================================================================
#
# Usage:
#   wget https://raw.githubusercontent.com/<user>/arbbot/main/scripts/vps-setup.sh
#   chmod +x vps-setup.sh
#   ./vps-setup.sh
#
# Or, after cloning the repo manually:
#   bash scripts/vps-setup.sh

set -euo pipefail

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

step() { echo -e "\n${GREEN}▶ $1${NC}"; }
warn() { echo -e "${YELLOW}⚠ $1${NC}"; }
err()  { echo -e "${RED}✗ $1${NC}"; exit 1; }

# ────────────────────────────────────────────────────────────────────────
# 1. System dependencies
# ────────────────────────────────────────────────────────────────────────
step "1/9  Installing system dependencies"
sudo apt-get update -qq
sudo apt-get install -y -qq \
    build-essential pkg-config libssl-dev libudev-dev cmake \
    git tmux curl wget jq htop iotop net-tools \
    protobuf-compiler ca-certificates \
    || err "apt install failed — check internet / apt sources"

# ────────────────────────────────────────────────────────────────────────
# 2. Rust toolchain (rustup) — match rust-toolchain.toml in repo
# ────────────────────────────────────────────────────────────────────────
step "2/9  Installing Rust"
if ! command -v cargo &>/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
fi
rustc --version

# ────────────────────────────────────────────────────────────────────────
# 3. Clone repo (skip if already in repo dir)
# ────────────────────────────────────────────────────────────────────────
step "3/9  Repo setup"
if [ -f "Cargo.toml" ] && grep -q "arb-scout" Cargo.toml 2>/dev/null; then
    REPO_DIR="$PWD"
    echo "  Detected repo at $REPO_DIR (already inside)"
else
    cd "$HOME"
    if [ -d arbbot ]; then
        echo "  Repo exists at $HOME/arbbot — pulling latest"
        cd arbbot && git pull --rebase
    else
        read -r -p "  Repo URL (e.g. git@github.com:you/arbbot.git or https://...): " REPO_URL
        [ -z "$REPO_URL" ] && err "no repo URL given"
        # If https with token, advise gh
        git clone "$REPO_URL" arbbot || err "git clone failed"
        cd arbbot
    fi
    REPO_DIR="$PWD"
fi
echo "  Repo dir: $REPO_DIR"

# ────────────────────────────────────────────────────────────────────────
# 4. Env file at /etc/arbbot/.env (system path, not in repo)
# ────────────────────────────────────────────────────────────────────────
step "4/9  Env configuration"
sudo mkdir -p /etc/arbbot
if [ ! -f /etc/arbbot/.env ]; then
    sudo touch /etc/arbbot/.env
    sudo chmod 600 /etc/arbbot/.env
    sudo chown "$USER:$USER" /etc/arbbot/.env
    cat <<EOF
  /etc/arbbot/.env created (empty, 600 perms).
  Paste your Railway env vars into it now.
  TIP: locally run \`railway variables --json | jq -r 'to_entries | .[] | "\(.key)=\(.value)"'\`
       and paste the output below. Then press Ctrl+D.
EOF
    cat > /etc/arbbot/.env  # read stdin, write to file
    echo "  Wrote $(wc -l < /etc/arbbot/.env) lines."
else
    echo "  /etc/arbbot/.env already exists. Leaving as-is."
fi
# Symlink into repo so cargo run finds it
ln -sf /etc/arbbot/.env "$REPO_DIR/.env"

# Verify required vars
required_vars=(SOLANA_KEYPAIR_JSON HELIUS_API_KEY)
missing=()
for v in "${required_vars[@]}"; do
    if ! grep -q "^${v}=" /etc/arbbot/.env; then missing+=("$v"); fi
done
if [ ${#missing[@]} -gt 0 ]; then
    warn "Missing required env vars: ${missing[*]}"
    warn "Edit /etc/arbbot/.env and re-run from step 5 (cargo build)."
fi

# ────────────────────────────────────────────────────────────────────────
# 5. Build (release)
# ────────────────────────────────────────────────────────────────────────
step "5/9  Building arb-scout (release)"
cd "$REPO_DIR"
cargo build --release -p arb-scout || err "cargo build failed"
echo "  Binary: $REPO_DIR/target/release/arb-scout"

# ────────────────────────────────────────────────────────────────────────
# 6. Latency check — the whole point of this migration
# ────────────────────────────────────────────────────────────────────────
step "6/9  Latency to Jito Block Engine"
echo "  Goal: <10ms p50 to win the auction"
for host in frankfurt.mainnet.block-engine.jito.wtf amsterdam.mainnet.block-engine.jito.wtf ny.mainnet.block-engine.jito.wtf; do
    echo ""
    echo "  → ping $host:"
    ping -c 5 -i 0.2 -W 2 "$host" 2>/dev/null | tail -1 || echo "    (ping blocked or host unresolved — try TCP test instead)"
done

# ────────────────────────────────────────────────────────────────────────
# 7. (Optional) systemd unit for auto-restart on reboot/crash
# ────────────────────────────────────────────────────────────────────────
step "7/9  Installing systemd unit"
SYSTEMD_UNIT=/etc/systemd/system/arbbot.service
if [ ! -f "$SYSTEMD_UNIT" ]; then
    sudo tee "$SYSTEMD_UNIT" >/dev/null <<EOF
[Unit]
Description=arbbot (Solana MEV scout)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$USER
WorkingDirectory=$REPO_DIR
EnvironmentFile=/etc/arbbot/.env
ExecStart=$REPO_DIR/target/release/arb-scout $REPO_DIR/config/scout-pools.toml
Restart=on-failure
RestartSec=5
StandardOutput=append:/var/log/arbbot.log
StandardError=append:/var/log/arbbot.log
# Hardening
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/var/log /etc/arbbot $REPO_DIR/target $REPO_DIR

[Install]
WantedBy=multi-user.target
EOF
    sudo touch /var/log/arbbot.log
    sudo chown "$USER:$USER" /var/log/arbbot.log
    sudo systemctl daemon-reload
    echo "  Installed $SYSTEMD_UNIT"
else
    echo "  systemd unit already exists"
fi

# ────────────────────────────────────────────────────────────────────────
# 8. Start the bot
# ────────────────────────────────────────────────────────────────────────
step "8/9  Starting arbbot"
sudo systemctl enable arbbot
sudo systemctl restart arbbot
sleep 3
sudo systemctl status arbbot --no-pager -l | head -20

# ────────────────────────────────────────────────────────────────────────
# 9. Done — show next steps
# ────────────────────────────────────────────────────────────────────────
step "9/9  Done!"
cat <<EOF

  Monitor logs:
    tail -F /var/log/arbbot.log
    sudo journalctl -u arbbot -f

  Restart after env change:
    sudo systemctl restart arbbot

  Tail just liquidations:
    tail -F /var/log/arbbot.log | grep -E 'LIQUIDATION|LANDED|attempt failed'

  Latency-validate after a few minutes — bundle landing rate should
  go from 0% (Railway Frankfurt) to 20-50%+ once Jito accepts our auction.

  IMPORTANT: stop the Railway deployment ONLY after you've confirmed
  this VPS instance is landing bundles:
    railway down --service comfortable-contentment
EOF
