use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct DiscoveryConfig {
    pub programs: Vec<String>,
}
