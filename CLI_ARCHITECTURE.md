# `gruezi` CLI Architecture

The CLI separates argument parsing, action selection, execution, and logging.

## Directory Structure

```text
src/cli/
├── actions/           # start, status, and peers execution
├── commands/          # clap argument definitions
├── dispatch/          # ArgMatches to typed Action conversion
├── mod.rs             # Module exports
├── start.rs           # CLI startup orchestration
└── telemetry.rs       # Local logging and optional OTLP tracing
```

## Data Flow

```text
bin/gruezi.rs
    ↓
cli::start()
    ↓
1. Parse arguments with commands::new()
2. Convert verbosity to a tracing level
3. Initialize local logging and optional OTLP export
4. Convert arguments to an Action with dispatch::handler()
5. Execute the action through src/cli/actions/
6. Flush any initialized tracer before process exit
```

`actions` calls the domain modules under `src/gruezi/`; `commands` and
`dispatch` contain no HA runtime logic.

## Telemetry Feature

The default-off `telemetry` Cargo feature adds OpenTelemetry and OTLP
dependencies. Local logging is available in every build.

| Build | Startup configuration | Behavior |
| --- | --- | --- |
| `cargo build` | Any | Local logging only |
| `cargo build --features telemetry` | No `OTEL_EXPORTER_OTLP_ENDPOINT` | Local logging only |
| `cargo build --features telemetry` | `OTEL_EXPORTER_OTLP_ENDPOINT` set | Local logging and OTLP trace export |

The feature controls what is compiled into the binary. The endpoint controls
whether a telemetry-enabled binary creates an exporter at startup. Changing
the feature requires a rebuild; changing the endpoint requires a restart.
The exporter uses OTLP over gRPC with gzip and TLS support.
`OTEL_EXPORTER_OTLP_HEADERS` supplies optional request headers.

The release, package, and container build commands use the default feature
set, so those binaries contain local logging only. To export traces, build
`gruezi` explicitly with `--features telemetry`. The CLI flushes an
initialized tracer on normal exit and on action errors; HA state transitions
and status requests emit finite spans while a service is running.
