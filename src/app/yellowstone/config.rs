use serde::Deserialize;
use yellowstone_grpc_proto::geyser::CommitmentLevel;

#[derive(Clone, Debug, Deserialize)]
pub struct YellowstoneConfig {
    pub grpc: String,
    #[serde(default)]
    pub x_token: Option<String>,
    #[serde(default = "default_commitment")]
    pub commitment: String,
}

fn default_commitment() -> String {
    /* An arbitrage bot wants the earliest state the node will admit, not the safest one */
    "processed".into()
}

impl YellowstoneConfig {
    pub fn commitment_from_str(&self) -> CommitmentLevel {
        match self.commitment.to_ascii_lowercase().as_str() {
            "confirmed" => CommitmentLevel::Confirmed,
            "finalized" => CommitmentLevel::Finalized,
            _ => CommitmentLevel::Processed,
        }
    }
}
