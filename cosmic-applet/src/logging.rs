use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_logging() {
    let logger = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::from_default_env());

    #[cfg(not(debug_assertions))]
    logger
        .with(tracing_journald::layer().expect("Failed to open the journald connection"))
        .init();
    #[cfg(debug_assertions)]
    logger.init();
}
