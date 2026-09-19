use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct DiscoveryConfig {
    pub rpc: String,
    pub programs: Vec<String>,
}
