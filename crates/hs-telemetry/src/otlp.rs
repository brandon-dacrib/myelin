//! OpenTelemetry OTLP trace export, behind the `otlp` feature.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;

use crate::error::TelemetryError;
use crate::init::Options;

/// Builds an OTLP (gRPC/tonic) span exporter and a batching tracer provider tagged with this
/// service's name and version as OTLP resource attributes.
///
/// # Errors
/// Returns [`TelemetryError::Otlp`] if the exporter could not be built (for example, an
/// unparseable endpoint URL).
pub fn build_provider(
    options: &Options,
    endpoint: &str,
) -> Result<SdkTracerProvider, TelemetryError> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;

    let resource = Resource::builder()
        .with_service_name(options.service_name.clone())
        .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            options.service_version.clone(),
        ))
        .build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    Ok(provider)
}

/// Wraps `provider` in a `tracing-opentelemetry` layer that can be composed onto any
/// `tracing_subscriber::Registry`-based subscriber via `.with(...)`.
pub fn layer_from_provider<S>(
    provider: &SdkTracerProvider,
) -> tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    let tracer = provider.tracer("hs-telemetry");
    tracing_opentelemetry::layer().with_tracer(tracer)
}
