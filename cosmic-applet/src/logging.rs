use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_logging() {
    let logger = tracing_subscriber::registry().with(tracing_subscriber::fmt::layer());

    #[cfg(not(debug_assertions))]
    logger
        .with(tracing::level_filters::LevelFilter::INFO)
        .with(tracing_journald::layer().expect("Failed to open the journald connection"))
        .init();
    #[cfg(debug_assertions)]
    logger
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}
