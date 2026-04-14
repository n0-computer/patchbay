//! Fixture test: sends UDP packets between two patchbay devices.
//! PACKET_COUNT and THRESHOLD are compile-time constants that the
//! integration test modifies between commits to create regressions.

const PACKET_COUNT: u32 = 5;
const THRESHOLD: u32 = 3;

#[cfg(target_os = "linux")]
#[ctor::ctor]
fn init() {
    patchbay::init_userns().expect("init_userns");
}

#[tokio::test(flavor = "current_thread")]
async fn udp_counter() -> anyhow::Result<()> {
    let outdir = testdir::testdir!();
    let lab = patchbay::Lab::builder()
        .outdir(patchbay::OutDir::Nested(outdir))
        .label("udp-counter")
        .build()
        .await?;
    let dc = lab.add_router("dc").build().await?;
    let sender = lab
        .add_device("sender")
        .iface("eth0", dc.id())
        .build()
        .await?;
    let receiver = lab
        .add_device("receiver")
        .iface("eth0", dc.id())
        .build()
        .await?;

    let recv_ip = receiver.ip().unwrap();
    let port: u16 = 9999;

    // Spawn UDP listener in the receiver's namespace.
    let rx_handle = receiver.spawn(move |_dev| async move {
        let sock = tokio::net::UdpSocket::bind(format!("{recv_ip}:{port}")).await?;
        let mut count = 0u32;
        let mut buf = [0u8; 64];
        for _ in 0..PACKET_COUNT {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                sock.recv_from(&mut buf),
            )
            .await??;
            count += 1;
        }
        Ok::<_, anyhow::Error>(count)
    })?;

    // Give the listener a moment to bind.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send packets from the sender's namespace.
    let send_ip = sender.ip().unwrap();
    let tx_handle = sender.spawn(move |_dev| async move {
        let sock = tokio::net::UdpSocket::bind(format!("{send_ip}:0")).await?;
        for i in 0..PACKET_COUNT {
            sock.send_to(
                format!("pkt-{i}").as_bytes(),
                format!("{recv_ip}:{port}"),
            )
            .await?;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })?;

    tx_handle.await??;
    let received = rx_handle.await??;

    sender.record("packet_count", PACKET_COUNT as f64);
    assert_eq!(received, PACKET_COUNT);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn udp_threshold() -> anyhow::Result<()> {
    assert!(
        PACKET_COUNT >= THRESHOLD,
        "packet count {} below threshold {}",
        PACKET_COUNT,
        THRESHOLD
    );
    Ok(())
}
