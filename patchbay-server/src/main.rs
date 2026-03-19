//! Standalone patchbay server binary.
//!
//! Serves the devtools UI and optionally accepts pushed run results via HTTP.
//! Supports automatic TLS via ACME (Let's Encrypt).

use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(name = "patchbay-serve", about = "Serve patchbay run results")]
struct Cli {
    /// Directory containing run results to serve.
    #[arg(long)]
    run_dir: Option<PathBuf>,

    /// Bind address for HTTP server.
    #[arg(long, default_value = "0.0.0.0:8080")]
    bind: String,

    /// Domain for automatic TLS via ACME (Let's Encrypt).
    /// When set, serves HTTPS on port 443 and HTTP redirect on port 80.
    #[arg(long)]
    acme_domain: Option<String>,

    /// Contact email for ACME/Let's Encrypt (required with --acme-domain).
    #[arg(long)]
    acme_email: Option<String>,

    /// Enable accepting pushed run results.
    #[arg(long, default_value_t = false)]
    accept_push: bool,

    /// API key required for push requests (Authorization: Bearer <key>).
    #[arg(long, env = "PATCHBAY_API_KEY")]
    api_key: Option<String>,

    /// Data directory for storing pushed runs and ACME state.
    /// Defaults to platform data dir (e.g. ~/.local/share/patchbay-serve).
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// Maximum total size of stored runs (e.g. "10GB", "500MB").
    /// When exceeded, oldest runs are deleted.
    #[arg(long)]
    retention: Option<String>,
}

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("TB") {
        (n.trim(), 1_000_000_000_000u64)
    } else if let Some(n) = s.strip_suffix("GB") {
        (n.trim(), 1_000_000_000u64)
    } else if let Some(n) = s.strip_suffix("MB") {
        (n.trim(), 1_000_000u64)
    } else if let Some(n) = s.strip_suffix("KB") {
        (n.trim(), 1_000u64)
    } else {
        (s, 1u64)
    };
    let val: f64 = num.parse().context("invalid size number")?;
    Ok((val * mult as f64) as u64)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Resolve data dir
    let data_dir = match &cli.data_dir {
        Some(d) => d.clone(),
        None => dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("patchbay-serve"),
    };
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("create data dir {}", data_dir.display()))?;

    // Resolve run dir: explicit --run-dir, or data_dir/runs if push enabled, or cwd
    let run_dir = match &cli.run_dir {
        Some(d) => d.clone(),
        None if cli.accept_push => data_dir.join("runs"),
        None => std::env::current_dir().context("resolve cwd")?,
    };
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("create run dir {}", run_dir.display()))?;

    if cli.accept_push && cli.api_key.is_none() {
        bail!("--accept-push requires --api-key to be set");
    }

    if cli.acme_domain.is_some() && cli.acme_email.is_none() {
        bail!("--acme-domain requires --acme-email to be set");
    }

    // Parse retention
    let retention_bytes = cli
        .retention
        .as_deref()
        .map(parse_size)
        .transpose()
        .context("invalid --retention value")?;

    let push_config = if cli.accept_push {
        Some(patchbay_server::PushConfig {
            api_key: cli.api_key.clone().unwrap(),
            run_dir: run_dir.clone(),
        })
    } else {
        None
    };

    // Start retention watcher if configured
    if let Some(max_bytes) = retention_bytes {
        let retention_dir = run_dir.clone();
        tokio::spawn(async move {
            patchbay_server::retention_watcher(retention_dir, max_bytes).await;
        });
    }

    let app = patchbay_server::build_app(run_dir, push_config);

    if let Some(domain) = &cli.acme_domain {
        let email = cli.acme_email.as_deref().unwrap();
        serve_acme(app, domain, email, &data_dir).await
    } else {
        tracing::info!("listening on {}", cli.bind);
        let listener = tokio::net::TcpListener::bind(&cli.bind).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}

async fn serve_acme(
    app: axum::Router,
    domain: &str,
    email: &str,
    data_dir: &std::path::Path,
) -> Result<()> {
    use tokio_rustls_acme::caches::DirCache;
    use tokio_rustls_acme::AcmeConfig;
    use tokio_stream::StreamExt;

    let acme_dir = data_dir.join("acme");
    std::fs::create_dir_all(&acme_dir)?;

    let mut state = AcmeConfig::new([domain])
        .contact([format!("mailto:{email}")])
        .cache(DirCache::new(acme_dir))
        .directory_lets_encrypt(true)
        .state();

    let rustls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(state.resolver());
    let acceptor = state.axum_acceptor(Arc::new(rustls_config));

    // Spawn ACME event handler
    tokio::spawn(async move {
        loop {
            match state.next().await {
                Some(Ok(ok)) => tracing::info!("acme event: {:?}", ok),
                Some(Err(err)) => tracing::error!("acme error: {:?}", err),
                None => break,
            }
        }
    });

    tracing::info!("listening on [::]:443 with ACME TLS for {domain}");

    // HTTP redirect on port 80
    let redirect_domain = domain.to_string();
    tokio::spawn(async move {
        let redirect = axum::Router::new().fallback(axum::routing::any(
            move |req: axum::extract::Request| {
                let host = redirect_domain.clone();
                async move {
                    let uri = req.uri();
                    let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
                    axum::response::Redirect::permanent(&format!("https://{host}{path}"))
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("[::]:80").await.unwrap();
        let _ = axum::serve(listener, redirect).await;
    });

    // Serve HTTPS with axum-server and ACME acceptor
    let addr = std::net::SocketAddr::from((Ipv6Addr::UNSPECIFIED, 443));
    axum_server::bind(addr)
        .acceptor(acceptor)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}
