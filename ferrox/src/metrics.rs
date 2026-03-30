/// Compatibility shim — all metrics now live in `telemetry::metrics`.
///
/// The `Metrics` struct is kept for `AppState` but it holds no data;
/// all recording uses the global `telemetry::metrics::*` statics.
#[allow(dead_code)]
#[derive(Default)]
pub struct Metrics;

impl Metrics {
    pub fn new() -> Self {
        // Force registration of all Lazy metric statics at startup
        crate::telemetry::metrics::gather();
        Self
    }
}
