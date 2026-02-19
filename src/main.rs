use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use anyhow::Result;
use netsim::{udp_rtt_in_ns, Gateway, Impair, Lab};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    require_root();
    let mut lab = Lab::new();
    let dc_eu = lab.add_dc("dc-eu", "eu")?;
    let dc_us = lab.add_dc("dc-us", "us")?;
    lab.add_region_latency("eu", "us", 20);
    lab.add_region_latency("us", "eu", 20);
    let dev = lab.add_device(
        "dev1",
        Gateway::Dc(dc_eu),
        Some(Impair::Manual {
            rate: 10_000,
            loss: 0.0,
            latency: 60,
        }),
    )?;
    lab.build().await?;

    let dc_us_ip = lab.dc_ix_ip(dc_us)?;
    let r = SocketAddr::new(IpAddr::V4(dc_us_ip), 9020);
    let dc_us_ns = lab.node_ns(dc_us)?.to_string();
    lab.spawn_reflector(&dc_us_ns, r)?;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let dev_ns = lab.node_ns(dev)?.to_string();
    let rtt = udp_rtt_in_ns(&dev_ns, r)?;
    println!("rtt {rtt:?}");
    assert!(
        rtt >= Duration::from_millis(90),
        "expected manual latency >= 90ms RTT, got {rtt:?}"
    );
    Ok(())
}

fn require_root() {
    if !nix::unistd::Uid::effective().is_root() {
        panic!("test requires root / CAP_NET_ADMIN — run: sudo -E cargo test -- --nocapture");
    }
}
