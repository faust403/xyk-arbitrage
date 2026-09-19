mod app;
mod logger;

use crate::app::App;
use crate::app::config::Config;
use crate::logger::LoggerTitle;
use crate::logger::info;
use crate::logger::setup;
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let mut app = init().await?;
    info(LoggerTitle::AppStarting, None::<String>);
    app.run().await;
    Ok(())
}

async fn init() -> Result<App> {
    setup();
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Error while setting up the TLS"))?;
    let config = Config::load()?;
    App::new(config).await
}
