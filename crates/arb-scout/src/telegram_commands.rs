//! Telegram interactive commands for the running scout.
//!
//! Long-polls Telegram via raw HTTP (`getUpdates`), filters every incoming
//! message to the configured operator `chat_id`, parses `/command args`
//! syntax, and responds via `sendMessage`. No `teloxide` framework
//! dependency on the runtime path — just `reqwest`.
//!
//! Authorized commands (replies silently dropped for any other chat_id):
//! * `/status`        — current snapshot (updates, opps, capture-rate, etc.)
//! * `/recent [N]`    — last N opportunities from today's JSONL (default 5)
//! * `/pools`         — number of pools loaded + per-DEX breakdown
//! * `/help`          — list available commands

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arb_core::ArbResult;
use serde_json::{json, Value};
use tokio::fs;
use tokio::io::AsyncBufReadExt;
use tracing::{debug, info, warn};

use arb_telemetry::OpportunityRecord;
use crate::scout::ScoutMetrics;
use arb_stream::PoolStateStore;

#[derive(Clone)]
pub struct TelegramCommands {
    bot_token: String,
    chat_id:   i64,
    metrics:   Arc<ScoutMetrics>,
    store:     Arc<PoolStateStore>,
    log_dir:   PathBuf,
    http:      reqwest::Client,
}

impl TelegramCommands {
    pub fn new(
        bot_token: String,
        chat_id:   i64,
        metrics:   Arc<ScoutMetrics>,
        store:     Arc<PoolStateStore>,
        log_dir:   PathBuf,
    ) -> Self {
        Self {
            bot_token, chat_id, metrics, store, log_dir,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("reqwest client"),
        }
    }

    pub async fn run(self) -> ArbResult<()> {
        info!(target: "scout::telegram", "command listener started");
        let mut offset: Option<i64> = None;
        loop {
            match self.poll_updates(offset).await {
                Ok(updates) => {
                    for update in updates {
                        if let Some(uid) = update.get("update_id").and_then(|v| v.as_i64()) {
                            offset = Some(uid + 1);
                        }
                        self.handle_update(&update).await;
                    }
                }
                Err(e) => {
                    warn!(target: "scout::telegram", error = %e, "poll error; retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn poll_updates(&self, offset: Option<i64>) -> ArbResult<Vec<Value>> {
        let url = format!("https://api.telegram.org/bot{}/getUpdates", self.bot_token);
        let mut body = json!({ "timeout": 25, "allowed_updates": ["message"] });
        if let Some(o) = offset { body["offset"] = json!(o); }

        let resp: Value = self.http.post(&url).json(&body).send().await
            .map_err(|e| arb_core::ArbError::Rpc(format!("telegram getUpdates: {e}")))?
            .json().await
            .map_err(|e| arb_core::ArbError::Rpc(format!("telegram parse: {e}")))?;

        if resp.get("ok").and_then(|b| b.as_bool()) != Some(true) {
            return Err(arb_core::ArbError::Rpc(format!("telegram non-ok: {resp}")));
        }
        Ok(resp.get("result").and_then(|r| r.as_array()).cloned().unwrap_or_default())
    }

    async fn handle_update(&self, update: &Value) {
        let Some(msg) = update.get("message") else { return; };
        let from_chat = msg.pointer("/chat/id").and_then(|c| c.as_i64()).unwrap_or(0);
        if from_chat != self.chat_id {
            // Silent reject — never respond to unauthorized chats. This is
            // critical security: don't even acknowledge their existence.
            debug!(target: "scout::telegram", from = from_chat, "rejecting unauthorized chat");
            return;
        }

        let text = msg.get("text").and_then(|t| t.as_str()).unwrap_or("");
        let trimmed = text.trim();
        if !trimmed.starts_with('/') { return; }

        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("").to_lowercase();
        let arg = parts.next().unwrap_or("").trim();

        let reply = match cmd.as_str() {
            "/status"               => self.cmd_status(),
            "/recent"               => self.cmd_recent(arg).await,
            "/pools"                => self.cmd_pools(),
            "/help" | "/start"      => Self::cmd_help(),
            other                   => format!("unknown command: `{other}`\nuse /help"),
        };
        let _ = self.send_message(&reply).await;
    }

    fn cmd_status(&self) -> String {
        let s = self.metrics.snapshot();
        let dollars = |units: i64| (units as f64) / 1_000_000.0;
        let capture = if s.opps_correlated_total > 0 {
            format!("{:.1}%", 100.0 * s.opps_uncontested as f64 / s.opps_correlated_total as f64)
        } else { "n/a".into() };
        format!(
            "📊 SCOUT STATUS\n\
             ────────────\n\
             Updates received: {}\n\
             Routes evaluated: {}\n\
             Opportunities found: {}\n\
             ────────────\n\
             ≥ $0.001:  {}\n\
             ≥ $0.01:   {}\n\
             ≥ $0.10:   {}\n\
             ≥ $1:      {}\n\
             ≥ $5:      {}\n\
             ≥ $20:     {}\n\
             Largest: ${:.4}\n\
             ────────────\n\
             Pools in store: {}\n\
             Vaults loaded: {}\n\
             SOL price: ${:.2}\n\
             ────────────\n\
             Correlated: {}\n\
             Lost: {}\n\
             Uncontested: {}\n\
             Capture rate: {}\n\
             Via Jupiter: {}\n\
             ────────────\n\
             WSS reconnects: {}\n\
             Bootstraps: {}",
            s.updates_received, s.routes_evaluated, s.routes_eval_ok_some,
            s.opps_ge_001, s.opps_ge_01, s.opps_ge_10c,
            s.opps_ge_1d, s.opps_ge_5d, s.opps_ge_20d,
            dollars(s.largest_net_profit_usdc),
            self.store.len(), self.vaults_count(),
            s.sol_price_usd,
            s.opps_correlated_total, s.opps_lost_to_competitor, s.opps_uncontested,
            capture, s.competing_via_jupiter_total,
            s.wss_reconnects, s.bootstrap_count,
        )
    }

    fn vaults_count(&self) -> usize {
        // Workaround: ScoutMetrics doesn't expose vault count directly.
        // Caller could pass it in; for now return 0 as a placeholder.
        0
    }

    async fn cmd_recent(&self, arg: &str) -> String {
        let n: usize = arg.parse().unwrap_or(5).clamp(1, 20);
        let today = chrono::Utc::now().date_naive();
        let path = self.log_dir.join(format!("scout-{}.jsonl", today.format("%Y-%m-%d")));

        let file = match fs::File::open(&path).await {
            Ok(f) => f,
            Err(_) => return format!("no opportunity log for {today} yet"),
        };
        let reader = tokio::io::BufReader::new(file);
        let mut lines = Vec::new();
        let mut buf_lines = reader.lines();
        while let Ok(Some(line)) = buf_lines.next_line().await {
            if line.trim().is_empty() { continue; }
            lines.push(line);
        }
        if lines.is_empty() { return "no opportunities logged today yet".into(); }
        let take = lines.iter().rev().take(n);
        let mut out = format!("📋 LAST {n} OPPORTUNITIES (today)\n────────────\n");
        for line in take {
            if let Ok(rec) = serde_json::from_str::<OpportunityRecord>(line) {
                out.push_str(&format!(
                    "{}  ${:.4} net  via {}\n",
                    rec.timestamp.format("%H:%M:%S"),
                    (rec.net_profit_usdc as f64) / 1_000_000.0,
                    rec.route,
                ));
            }
        }
        out
    }

    fn cmd_pools(&self) -> String {
        let pools = self.store.snapshot();
        let mut by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
        for p in &pools {
            *by_kind.entry(p.kind.id()).or_insert(0) += 1;
        }
        let mut out = format!("🏦 POOLS IN STORE: {}\n────────────\n", pools.len());
        for (k, c) in by_kind { out.push_str(&format!("{k:18}  {c}\n")); }
        out
    }

    fn cmd_help() -> String {
        "🤖 SCOUT COMMANDS\n\
         ────────────\n\
         /status       current metrics + capture rate\n\
         /recent [N]   last N opportunities from today (default 5, max 20)\n\
         /pools        per-DEX pool count\n\
         /help         this message".into()
    }

    async fn send_message(&self, text: &str) -> ArbResult<()> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
        let body = json!({ "chat_id": self.chat_id, "text": text });
        let _ = self.http.post(&url).json(&body).send().await
            .map_err(|e| arb_core::ArbError::Rpc(format!("send: {e}")))?;
        Ok(())
    }
}
