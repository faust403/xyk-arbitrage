//! JSONL logger for opportunities. Daily rotation by UTC date.
//!
//! Each line is a single `OpportunityRecord` JSON object. The format MUST
//! remain stable — `docs/SCOUT_REPORT.md` is generated from these files at
//! the end of Phase −1.

use std::path::PathBuf;
use std::sync::Arc;

use arb_core::{ArbError, ArbResult};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpportunityRecord {
    /// RFC3339 timestamp at the moment of detection (slot-confirmed).
    pub timestamp: DateTime<Utc>,
    /// On-chain slot of the triggering account update.
    pub slot: u64,
    /// Pretty route description: `"USDC -[raydium]-> SOL -[orca]-> USDC"`.
    pub route: String,
    /// Probe input amount in USDC base units (6 decimals).
    pub input_usdc: u64,
    /// Final output amount in USDC base units after all hops.
    pub output_usdc: u64,
    /// `output - input` in USDC base units (signed; negative = unprofitable).
    pub gross_profit_usdc: i64,
    pub flashloan_fee_usdc: u64,
    pub priority_fee_lamports: u64,
    pub jito_tip_lamports: u64,
    /// Net profit in USDC base units after subtracting all fees (signed).
    pub net_profit_usdc: i64,
    /// Pool addresses traversed, in hop order.
    pub pools: Vec<String>,
}

/// Append-only daily-rotating JSONL writer. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct JsonlLogger {
    log_dir: PathBuf,
    /// Guards the current file handle + the date it was opened on.
    state:   Arc<Mutex<LoggerState>>,
}

struct LoggerState {
    current_date: Option<chrono::NaiveDate>,
    file:         Option<tokio::fs::File>,
}

impl JsonlLogger {
    pub fn new(log_dir: PathBuf) -> Self {
        Self {
            log_dir,
            state: Arc::new(Mutex::new(LoggerState {
                current_date: None,
                file:         None,
            })),
        }
    }

    /// Append one record. Atomically creates `log_dir` if needed and rotates
    /// the file when the UTC date changes.
    pub async fn log(&self, record: &OpportunityRecord) -> ArbResult<()> {
        let mut line = serde_json::to_string(record)
            .map_err(|e| ArbError::Config(format!("serialize record: {e}")))?;
        line.push('\n');

        let today = record.timestamp.date_naive();
        let mut state = self.state.lock().await;

        if state.current_date != Some(today) || state.file.is_none() {
            fs::create_dir_all(&self.log_dir).await?;
            let path = self.log_dir.join(format!("scout-{}.jsonl", today.format("%Y-%m-%d")));
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await?;
            state.file = Some(f);
            state.current_date = Some(today);
        }

        let f = state.file.as_mut().expect("file just set above");
        f.write_all(line.as_bytes()).await?;
        f.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;
    use tokio::fs as tokio_fs;

    fn sample(now: DateTime<Utc>) -> OpportunityRecord {
        OpportunityRecord {
            timestamp: now,
            slot: 100,
            route: "USDC -[raydium]-> SOL -[orca]-> USDC".into(),
            input_usdc: 100_000_000,
            output_usdc: 100_500_000,
            gross_profit_usdc: 500_000,
            flashloan_fee_usdc: 90_000,
            priority_fee_lamports: 30_000,
            jito_tip_lamports: 10_000,
            net_profit_usdc: 410_000,
            pools: vec!["pool1".into(), "pool2".into()],
        }
    }

    #[tokio::test]
    async fn test_scout_logs_jsonl_format() {
        let dir = TempDir::new().unwrap();
        let logger = JsonlLogger::new(dir.path().to_path_buf());
        let rec = sample(Utc::now());
        logger.log(&rec).await.unwrap();

        // Find the file.
        let entries = tokio_fs::read_dir(dir.path()).await.unwrap();
        let mut entries = entries;
        let mut paths = vec![];
        while let Some(e) = entries.next_entry().await.unwrap() {
            paths.push(e.path());
        }
        assert_eq!(paths.len(), 1);
        let contents = tokio_fs::read_to_string(&paths[0]).await.unwrap();
        // Each line is parseable JSON of the right type.
        for line in contents.lines() {
            let _back: OpportunityRecord = serde_json::from_str(line).unwrap();
        }
    }

    #[tokio::test]
    async fn rotates_file_when_date_changes() {
        let dir = TempDir::new().unwrap();
        let logger = JsonlLogger::new(dir.path().to_path_buf());
        let day1 = Utc.with_ymd_and_hms(2026, 5, 9, 12, 0, 0).unwrap();
        let day2 = Utc.with_ymd_and_hms(2026, 5, 10, 12, 0, 0).unwrap();
        logger.log(&sample(day1)).await.unwrap();
        logger.log(&sample(day2)).await.unwrap();

        let mut entries = tokio_fs::read_dir(dir.path()).await.unwrap();
        let mut count = 0;
        while entries.next_entry().await.unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 2, "expected one file per UTC day");
    }
}
