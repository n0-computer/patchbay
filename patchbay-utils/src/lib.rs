pub mod assets;
pub mod binary_cache;

/// Initialises tracing for the current process (idempotent).
///
/// Honors `RUST_LOG`; defaults to `info` if unset.
pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
