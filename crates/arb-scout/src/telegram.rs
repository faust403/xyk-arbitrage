//! Hourly Telegram summary sender.
//!
//! Posts via Telegram's HTTP `sendMessage` endpoint directly (no `teloxide`
//! polling — the scout doesn't need to receive commands). Silently no-ops if
//! `telegram_bot_token` or `chat_id` aren't configured.

use arb_core::ArbResult;
use serde_json::json;
use tracing::{info, warn};

use crate::scout::ScoutMetrics;

#[derive(Clone)]
pub struct TelegramNotifier {
    bot_token: Option<String>,
    chat_id:   Option<i64>,
    http:      reqwest::Client,
}

impl TelegramNotifier {
    pub fn new(bot_token: Option<String>, chat_id: Option<i64>) -> Self {
        Self {
            bot_token,
            chat_id,
            http: reqwest::Client::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.bot_token.is_some() && self.chat_id.is_some()
    }

    pub async fn send(&self, text: &str) -> ArbResult<()> {
        let (Some(token), Some(chat_id)) = (&self.bot_token, &self.chat_id) else {
            // Not configured: log and skip, do not error.
            info!(target: "scout::telegram", "(disabled) {text}");
            return Ok(());
        };
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let body = json!({
            "chat_id":    chat_id,
            "text":       text,
            "parse_mode": "MarkdownV2",
        });
        match self.http.post(&url).json(&body).send().await {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                warn!(%status, %body, "Telegram sendMessage non-2xx");
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "Telegram sendMessage failed");
                Ok(())
            }
        }
    }
}

/// Render the periodic summary message Telegram receives. MarkdownV2 escaped.
pub fn render_summary(m: &ScoutMetrics, pools_in_store: usize, vaults_in_store: usize) -> String {
    let snap = m.snapshot();
    let format_dollars = |units: i64| {
        let dollars = (units as f64) / 1_000_000.0;
        format!("${dollars:.2}")
    };

    format!(
        "*Scout summary* \\(15min\\)\n\
         \n\
         📡 *Stream*\n\
         Updates: `{updates}`  \\| Vault upd: `{vaults}`\n\
         WSS reconnects: `{reconn}`  \\| Bootstraps: `{boots}`\n\
         \n\
         🏦 *Coverage*\n\
         Pools in store: `{pools}`  \\| Vaults: `{vlen}`\n\
         No\\-parser updates: `{noparser}`\n\
         SOL price: `${sol:.2}`\n\
         \n\
         🔍 *Routes*\n\
         Evaluated: `{eval}`\n\
         Ok\\(None\\): `{okn}`  \\| Ok\\(Some\\): `{oks}`  \\| Err: `{erc}`\n\
         \n\
         💰 *Opportunities*\n\
         ≥ \\$0\\.10: `{o10c}`  \\| ≥ \\$1: `{o1d}`\n\
         ≥ \\$5: `{o5d}`  \\| ≥ \\$20: `{o20d}`\n\
         Largest single: `{largest}`",
        updates  = snap.updates_received,
        vaults   = snap.vault_updates_received,
        reconn   = snap.wss_reconnects,
        boots    = snap.bootstrap_count,
        pools    = pools_in_store,
        vlen     = vaults_in_store,
        noparser = snap.no_parser_claimed,
        sol      = snap.sol_price_usd,
        eval     = snap.routes_evaluated,
        okn      = snap.routes_eval_ok_none,
        oks      = snap.routes_eval_ok_some,
        erc      = snap.routes_eval_err,
        o10c     = snap.opps_ge_10c,
        o1d      = snap.opps_ge_1d,
        o5d      = snap.opps_ge_5d,
        o20d     = snap.opps_ge_20d,
        largest  = format_dollars(snap.largest_net_profit_usdc),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scout_telegram_summary_format() {
        let m = ScoutMetrics::default();
        m.updates_received.fetch_add(42, std::sync::atomic::Ordering::Relaxed);
        m.opps_ge_1d.fetch_add(3, std::sync::atomic::Ordering::Relaxed);
        let s = render_summary(&m, 13, 6);
        assert!(s.contains("Scout summary"));
        assert!(s.contains("`42`"));
        assert!(s.contains("`3`"));
        assert!(s.contains("`13`"));
        assert!(s.contains("`6`"));
    }

    #[test]
    fn disabled_when_token_or_chat_missing() {
        assert!(!TelegramNotifier::new(None, Some(1)).enabled());
        assert!(!TelegramNotifier::new(Some("t".into()), None).enabled());
        assert!( TelegramNotifier::new(Some("t".into()), Some(1)).enabled());
    }
}
