use opentelemetry::global;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    runtime,
    trace::{RandomIdGenerator, Sampler},
    Resource,
};

use crate::config::TracingConfig;

/// Initialize the OTLP tracer and install it as the global provider.
///
/// Returns the tracer if enabled, `None` otherwise.
pub fn init_tracer(
    config: &TracingConfig,
) -> Result<Option<opentelemetry_sdk::trace::Tracer>, anyhow::Error> {
    if !config.enabled {
        return Ok(None);
    }

    let resource = Resource::new(vec![
        KeyValue::new("service.name", config.service_name.clone()),
        KeyValue::new("service.version", config.service_version.clone()),
    ]);

    let sampler = if config.sample_rate >= 1.0 {
        Sampler::AlwaysOn
    } else if config.sample_rate <= 0.0 {
        Sampler::AlwaysOff
    } else {
        Sampler::TraceIdRatioBased(config.sample_rate)
    };

    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(&config.otlp_endpoint),
        )
        .with_trace_config(
            opentelemetry_sdk::trace::config()
                .with_sampler(sampler)
                .with_id_generator(RandomIdGenerator::default())
                .with_resource(resource),
        )
        .install_batch(runtime::Tokio)?;

    tracing::info!(
        endpoint = %config.otlp_endpoint,
        sample_rate = config.sample_rate,
        "OpenTelemetry tracing enabled"
    );

    Ok(Some(tracer))
}

/// Flush pending spans and shut down the global tracer provider.
/// Call this before process exit.
pub fn shutdown() {
    global::shutdown_tracer_provider();
}
