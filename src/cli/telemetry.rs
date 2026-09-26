use anyhow::Result;
#[cfg(feature = "telemetry")]
use anyhow::{Context, bail};
#[cfg(feature = "telemetry")]
use base64::{Engine, engine::general_purpose::STANDARD};
#[cfg(feature = "telemetry")]
use opentelemetry::{
    KeyValue, global, propagation::TextMapCompositePropagator, trace::TracerProvider as _,
};
#[cfg(feature = "telemetry")]
use opentelemetry_otlp::{Compression, WithExportConfig, WithTonicConfig};
#[cfg(feature = "telemetry")]
use opentelemetry_sdk::{
    Resource,
    propagation::{BaggagePropagator, TraceContextPropagator},
    trace::{SdkTracerProvider, Tracer},
};
use std::sync::OnceLock;
#[cfg(feature = "telemetry")]
use std::{env, time::Duration};
#[cfg(feature = "telemetry")]
use tonic::{
    metadata::{Ascii, Binary, MetadataKey, MetadataMap, MetadataValue},
    transport::ClientTlsConfig,
};
use tracing::Level;
#[cfg(feature = "telemetry")]
use tracing_subscriber::Layer as _;
use tracing_subscriber::{EnvFilter, Registry, fmt, layer::SubscriberExt};
#[cfg(feature = "telemetry")]
use ulid::Ulid;

static TELEMETRY_INIT: OnceLock<()> = OnceLock::new();
#[cfg(feature = "telemetry")]
static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

#[cfg(feature = "telemetry")]
fn parse_headers(value: &str) -> Result<MetadataMap> {
    let mut metadata = MetadataMap::new();
    for pair in value.split(',').filter(|pair| !pair.trim().is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .context("OTEL_EXPORTER_OTLP_HEADERS entries must be key=value")?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();

        if key.ends_with("-bin") {
            let key = MetadataKey::<Binary>::from_bytes(key.as_bytes())
                .context("invalid binary OTLP header name")?;
            let bytes = STANDARD
                .decode(value)
                .context("invalid base64 OTLP header value")?;
            metadata.insert_bin(key, MetadataValue::from_bytes(&bytes));
        } else {
            let key = MetadataKey::<Ascii>::from_bytes(key.as_bytes())
                .context("invalid OTLP header name")?;
            let value = value.parse().context("invalid OTLP header value")?;
            metadata.insert(key, value);
        }
    }
    Ok(metadata)
}

#[cfg(feature = "telemetry")]
fn normalize_endpoint(endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_owned()
    } else {
        format!("https://{}", endpoint.trim_end_matches('/'))
    }
}

#[cfg(feature = "telemetry")]
fn init_tracer(endpoint: &str) -> Result<(SdkTracerProvider, Tracer)> {
    if let Ok(protocol) = env::var("OTEL_EXPORTER_OTLP_PROTOCOL")
        && protocol != "grpc"
    {
        bail!("OTEL_EXPORTER_OTLP_PROTOCOL must be grpc");
    }

    let endpoint = normalize_endpoint(endpoint);
    let mut exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .with_compression(Compression::Gzip)
        .with_timeout(Duration::from_secs(3));

    if let Some(host) = endpoint
        .strip_prefix("https://")
        .and_then(|address| address.split('/').next())
        .and_then(|authority| authority.split(':').next())
    {
        exporter = exporter.with_tls_config(
            ClientTlsConfig::new()
                .domain_name(host.to_owned())
                .with_native_roots(),
        );
    }

    if let Ok(headers) = env::var("OTEL_EXPORTER_OTLP_HEADERS") {
        exporter = exporter.with_metadata(parse_headers(&headers)?);
    }

    let exporter = exporter.build()?;
    let instance_id =
        env::var("OTEL_SERVICE_INSTANCE_ID").unwrap_or_else(|_| Ulid::generate().to_string());
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder_empty()
                .with_attributes(vec![
                    KeyValue::new("service.name", env!("CARGO_PKG_NAME")),
                    KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                    KeyValue::new("service.instance.id", instance_id),
                ])
                .build(),
        )
        .build();
    let tracer = provider.tracer(env!("CARGO_PKG_NAME"));
    Ok((provider, tracer))
}

/// Initialize local logging and optional OTLP trace export.
///
/// An exporter is created only when the `telemetry` feature is enabled and
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set at startup.
///
/// # Errors
///
/// Returns an error if the tracing filter, exporter, or global subscriber
/// cannot be initialized.
pub fn init(verbosity_level: Option<Level>) -> Result<()> {
    if TELEMETRY_INIT.get().is_some() {
        return Ok(());
    }

    let default_level = verbosity_level.unwrap_or(Level::ERROR);
    let fmt_layer = fmt::layer()
        .with_file(false)
        .with_line_number(false)
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .compact();

    let filter = EnvFilter::builder()
        .with_default_directive(default_level.into())
        .from_env_lossy()
        .add_directive("hyper=error".parse()?)
        .add_directive("tokio=error".parse()?);

    #[cfg(feature = "telemetry")]
    if let Ok(endpoint) = env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        let (provider, tracer) = init_tracer(&endpoint)?;
        let subscriber = Registry::default()
            .with(fmt_layer.with_filter(filter))
            .with(
                tracing_opentelemetry::layer()
                    .with_tracer(tracer)
                    .with_filter(EnvFilter::new("gruezi=info")),
            );
        tracing::subscriber::set_global_default(subscriber)?;
        global::set_tracer_provider(provider.clone());
        global::set_text_map_propagator(TextMapCompositePropagator::new(vec![
            Box::new(TraceContextPropagator::new()),
            Box::new(BaggagePropagator::new()),
        ]));
        let _ = TRACER_PROVIDER.set(provider);
        let _ = TELEMETRY_INIT.set(());
        return Ok(());
    }

    #[cfg(feature = "telemetry")]
    {
        let subscriber = Registry::default().with(fmt_layer.with_filter(filter));
        tracing::subscriber::set_global_default(subscriber)?;
    }

    #[cfg(not(feature = "telemetry"))]
    {
        let subscriber = Registry::default().with(filter).with(fmt_layer);
        tracing::subscriber::set_global_default(subscriber)?;
    }

    let _ = TELEMETRY_INIT.set(());
    Ok(())
}

/// Flush and shut down an initialized tracer provider.
///
/// Returns whether OTLP export was initialized.
#[cfg(feature = "telemetry")]
#[must_use]
pub fn shutdown_tracer() -> bool {
    if let Some(provider) = TRACER_PROVIDER.get() {
        if let Err(error) = provider.force_flush() {
            tracing::warn!(%error, "failed to flush traces");
        }
        if let Err(error) = provider.shutdown() {
            tracing::warn!(%error, "failed to shut down tracer provider");
        }
        true
    } else {
        false
    }
}

/// Return immediately when OTLP export is not compiled in.
#[cfg(not(feature = "telemetry"))]
#[must_use]
pub const fn shutdown_tracer() -> bool {
    false
}

#[cfg(all(test, feature = "telemetry"))]
mod tests {
    use super::*;

    #[test]
    fn parses_ascii_and_binary_otlp_headers() {
        let headers = parse_headers("authorization=Bearer token,custom-bin=YmluYXJ5");
        assert!(headers.is_ok());
        if let Ok(headers) = headers {
            assert_eq!(headers.len(), 2);
        }
    }

    #[test]
    fn rejects_malformed_otlp_headers() {
        assert!(parse_headers("authorization").is_err());
        assert!(parse_headers("custom-bin=invalid!").is_err());
    }

    #[test]
    fn normalizes_scheme_less_endpoint_to_https() {
        assert_eq!(
            normalize_endpoint("collector.example:4317/"),
            "https://collector.example:4317"
        );
    }

    #[test]
    fn shutdown_without_exporter_returns_false() {
        assert!(!shutdown_tracer());
    }
}
