use std::sync::OnceLock;

use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::{RandomIdGenerator, Sampler, SdkTracerProvider};
use opentelemetry_sdk::Resource;

use crate::config::TracingConfig;

/// Holds the process-wide `SdkTracerProvider` so it can be flushed and shut
/// down cleanly from [`shutdown`] without relying on the (now removed)
/// `global::shutdown_tracer_provider` helper.
static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// Initialize the OTLP tracer and install it as the global provider.
///
/// Returns the tracer if enabled, `None` otherwise.
pub fn init_tracer(
    config: &TracingConfig,
) -> Result<Option<opentelemetry_sdk::trace::Tracer>, anyhow::Error> {
    if !config.enabled {
        return Ok(None);
    }

    let resource = Resource::builder()
        .with_attributes([
            KeyValue::new("service.name", config.service_name.clone()),
            KeyValue::new("service.version", config.service_version.clone()),
        ])
        .build();

    let sampler = if config.sample_rate >= 1.0 {
        Sampler::AlwaysOn
    } else if config.sample_rate <= 0.0 {
        Sampler::AlwaysOff
    } else {
        Sampler::TraceIdRatioBased(config.sample_rate)
    };

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&config.otlp_endpoint)
        .build()?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler)
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("ferrox");

    global::set_tracer_provider(provider.clone());
    // Keep a handle so `shutdown()` can flush/close the provider on exit.
    // Safe to ignore the error: it only fails if `init_tracer` is called
    // more than once, which never happens in normal operation.
    let _ = TRACER_PROVIDER.set(provider);

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
    if let Some(provider) = TRACER_PROVIDER.get() {
        if let Err(err) = provider.shutdown() {
            tracing::warn!(error = %err, "failed to shut down OpenTelemetry tracer provider");
        }
    }
}
