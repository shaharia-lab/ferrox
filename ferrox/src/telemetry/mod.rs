pub mod metrics;
pub mod otel;

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::TelemetryConfig;

/// Initialize the `tracing-subscriber` stack.
///
/// Optionally adds an OpenTelemetry OTLP layer when `config.tracing.enabled = true`.
/// Must be called exactly once at startup.
pub fn init_logging(config: &TelemetryConfig) -> Result<(), anyhow::Error> {
    let filter = EnvFilter::try_new(&config.log_level).unwrap_or_else(|_| EnvFilter::new("info"));

    // Build the optional OTEL tracer
    let otel_tracer = otel::init_tracer(&config.tracing)?;

    let json = config.log_format == "json";

    match (json, otel_tracer) {
        (true, Some(tracer)) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().json())
                .with(tracing_opentelemetry::layer().with_tracer(tracer))
                .init();
        }
        (false, Some(tracer)) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer())
                .with(tracing_opentelemetry::layer().with_tracer(tracer))
                .init();
        }
        (true, None) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().json())
                .init();
        }
        (false, None) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer())
                .init();
        }
    }

    Ok(())
}
