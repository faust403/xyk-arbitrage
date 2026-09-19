use std::fmt::Display;
use std::fmt::Formatter;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::ChronoUtc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoggerTitle {
    /* Yellowstone */
    YellowstoneUnknownUpdate,
    YellowstoneConnectionRejected,
    YellowstoneStreamReconnected,
    YellowstoneEndpointSilence,
    YellowstoneStreamClosed,
    YellowstoneStreamError,

    AppStarting,
}

impl Display for LoggerTitle {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

pub fn setup() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_timer(ChronoUtc::new("%Y-%m-%d(%H:%M:%S)".to_owned()))
        .with_target(false)
        .init();
}

pub fn info(title: impl Display, message: impl valuable::Valuable) {
    tracing::info!(
        title = %title,
        message = tracing::field::valuable(&message)
    );
}

pub fn warn(title: impl Display, message: impl valuable::Valuable) {
    tracing::warn!(
        title = %title,
        message = tracing::field::valuable(&message)
    );
}

pub fn error(title: impl Display, message: impl valuable::Valuable) {
    tracing::error!(
        title = %title,
        message = tracing::field::valuable(&message)
    );
}
