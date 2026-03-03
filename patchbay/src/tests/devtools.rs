//! E2E test: creates a simple lab with outdir for devtools UI testing.
//!
//! Run with:
//! ```sh
//! PATCHBAY_OUTDIR=/tmp/patchbay-e2e cargo test -p patchbay simple_lab_for_e2e -- --ignored
//! ```

use tracing::{info_span, Instrument};

use super::*;
use crate::consts;

/// Creates a minimal lab with a DC server, home-NAT router, and client device.
/// Runs a TCP echo roundtrip and writes all events + state to `PATCHBAY_OUTDIR`.
///
/// This test is `#[ignore]`d by default — the playwright e2e test invokes it explicitly.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
#[ignore]
async fn simple_lab_for_e2e() -> Result<()> {
    check_caps()?;
    let lab = Lab::with_opts(LabOpts::default().outdir_from_env().label("e2e-test")).await?;

    // DC router (public, no NAT).
    let dc = lab.add_router("dc").build().await?;

    // Home NAT router behind an ISP.
    let isp = lab.add_router("isp").build().await?;
    let home = lab
        .add_router("home")
        .upstream(isp.id())
        .nat(Nat::Home)
        .build()
        .await?;

    // Client device behind the home router.
    let client = lab
        .add_device("client")
        .iface("eth0", home.id(), None)
        .build()
        .await?;

    // Server device in the DC.
    let server = lab
        .add_device("server")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    // Start TCP echo server on the server device.
    let server_ip = server.ip().context("no server ip")?;
    let echo_addr = SocketAddr::new(IpAddr::V4(server_ip), 9090);
    server
        .spawn(move |_| async move {
            tracing::info!(target: "patchbay::_events::TcpEchoStarted", addr = %echo_addr);
            spawn_tcp_echo_server(echo_addr).await
        })?
        .await
        .context("echo server start panicked")??;

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Run TCP echo from client to server.
    info!("starting TCP echo roundtrip");
    client
        .spawn(move |_| async move {
            let s1 = info_span!("s1", z = 3);
            let s2 = info_span!(parent: &s1, "s2", y = 2);
            async {
                tracing::info!(target: "foo", x = 1);
            }
            .instrument(s2)
            .await;

            let result = tcp_roundtrip(echo_addr).await;
            tracing::info!(target: "patchbay::_events::TcpRoundtripComplete", bytes = 5);
            result
        })?
        .await
        .context("tcp roundtrip panicked")??;
    info!("TCP echo roundtrip succeeded");

    // Capture run_dir before dropping lab.
    let run_dir = lab.run_dir().map(|p| p.to_path_buf());

    // Drop lab to flush the writer.
    drop(lab);

    // Give the writer task time to flush.
    tokio::time::sleep(Duration::from_millis(2000)).await;

    // Verify run directory and files.
    if let Some(run_dir) = run_dir {
        let events_path = run_dir.join(consts::EVENTS_JSONL);
        let state_path = run_dir.join(consts::STATE_JSON);
        assert!(events_path.exists(), "events.jsonl not found");
        assert!(state_path.exists(), "state.json not found");

        let events_content = std::fs::read_to_string(&events_path)?;
        assert!(
            events_content.contains("router_added"),
            "events.jsonl missing router_added"
        );
        assert!(
            events_content.contains("device_added"),
            "events.jsonl missing device_added"
        );

        let state_content = std::fs::read_to_string(&state_path)?;
        assert!(
            state_content.contains("\"home\""),
            "state.json missing home router"
        );
        assert!(
            state_content.contains("\"client\""),
            "state.json missing client device"
        );

        // Verify per-namespace tracing files (flat: {kind}.{name}.{ext}).
        // Files are created lazily on first write — only namespaces that actually log
        // will produce files. Devices always log (tracing::info! in spawn closures).
        let client_tracing =
            consts::node_file(consts::KIND_DEVICE, "client", consts::TRACING_JSONL_EXT);
        let server_tracing =
            consts::node_file(consts::KIND_DEVICE, "server", consts::TRACING_JSONL_EXT);
        assert!(
            run_dir.join(&client_tracing).exists(),
            "{client_tracing} not found"
        );
        assert!(
            run_dir.join(&server_tracing).exists(),
            "{server_tracing} not found"
        );

        // Verify extracted _events files.
        let client_events_file =
            consts::node_file(consts::KIND_DEVICE, "client", consts::EVENTS_JSONL_EXT);
        let client_events = std::fs::read_to_string(run_dir.join(&client_events_file))?;
        assert!(
            client_events.contains("TcpRoundtripComplete"),
            "{client_events_file} missing TcpRoundtripComplete"
        );

        let server_events_file =
            consts::node_file(consts::KIND_DEVICE, "server", consts::EVENTS_JSONL_EXT);
        let server_events = std::fs::read_to_string(run_dir.join(&server_events_file))?;
        assert!(
            server_events.contains("TcpEchoStarted"),
            "{server_events_file} missing TcpEchoStarted"
        );

        info!("outdir verified: {}", run_dir.display());
    } else {
        info!("no outdir configured, skipping file verification");
    }

    Ok(())
}
