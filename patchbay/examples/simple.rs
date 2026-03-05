use anyhow::Result;
use patchbay::{Lab, LinkCondition, RouterPreset};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn main() -> Result<()> {
    // Enter a user namespace for rootless operation before any threads
    // are spawned — this must happen before the tokio runtime starts.
    patchbay::init_userns().expect("failed to enter user namespace");
    async_main()
}

#[tokio::main]
async fn async_main() -> Result<()> {
    let lab = Lab::new().await?;

    // A public router: downstream devices get globally routable IPs.
    let dc = lab
        .add_router("dc")
        .preset(RouterPreset::Public)
        .build()
        .await?;

    // A home router: downstream devices get private IPs behind NAT.
    let home = lab
        .add_router("home")
        .preset(RouterPreset::Home)
        .build()
        .await?;

    // A device behind the home router, with a lossy WiFi link.
    let dev = lab
        .add_device("laptop")
        .iface("eth0", home.id(), Some(LinkCondition::Wifi))
        .build()
        .await?;

    // A server in the datacenter.
    let server = lab
        .add_device("server")
        .iface("eth0", dc.id(), None)
        .build()
        .await?;

    // Run an OS command inside a device's network namespace.
    let mut child = dev.spawn_command({
        let mut cmd = tokio::process::Command::new("ping");
        cmd.args(["-c1", &server.ip().unwrap().to_string()]);
        cmd
    })?;
    child.wait().await?;

    // Spawn async tasks on a per-namespace tokio runtime. All async Rust
    // networking primitives work inside, fully isolated to the simulated
    // device's network stack.
    let addr = std::net::SocketAddr::from((server.ip().unwrap(), 8080));
    let server_task = server.spawn(async move |_| {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let (mut stream, peer) = listener.accept().await?;
        println!("got connection from {peer}");
        tokio::spawn(async move {
            let mut s = String::new();
            stream.read_to_string(&mut s).await?;
            println!("{peer} says: {s}");
            anyhow::Ok(())
        });
        anyhow::Ok(())
    });

    // Connect from the laptop — traffic is NATed through the home router.
    let client_task = dev.spawn(async move |_| {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        println!("local addr: {}", stream.local_addr()?);
        stream.write_all(b"hello server").await?;
        anyhow::Ok(())
    });

    client_task?.await??;
    server_task?.await??;

    Ok(())
}
