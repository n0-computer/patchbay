use anyhow::Result;
use patchbay::{Lab, LinkCondition, Nat};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn main() -> Result<()> {
    // We need to enter a user namespace for rootless operation before any threads
    // are spawned. Therefore this needs to happen before the tokio initialization.
    patchbay::init_userns().expect("failed to enter user namespace");
    async_main()
}

#[tokio::main]
async fn async_main() -> Result<()> {
    // Create a lab with a global "internet switch" to which routers are connected.
    let lab = Lab::new().await;

    // A "datacenter" router: downstream devices get "public" IPs.
    let dc = lab.add_router("dc").build().await?;

    // A "home" router with a NAT: downstream devices get private IPs.
    let home = lab.add_router("home").nat(Nat::Home).build().await?;

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

    // Run a command inside a device's network namespace.
    let mut child = dev.spawn_command({
        let mut cmd = std::process::Command::new("ping");
        cmd.args(["-c1", &server.ip().unwrap().to_string()]);
        cmd
    })?;
    tokio::task::spawn_blocking(move || child.wait().expect("command failed")).await?;

    // You can spawn tasks on a single-threaded tokio runtime
    // running in a device's network namespace.
    //
    // Through this, you can use all async rust networking primitives,
    // but they run fully isolated to the networking stack of a simulated device.
    let addr = std::net::SocketAddr::from((server.ip().unwrap(), 8080));
    let server_task = server.spawn(async move |_| {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let (mut stream, peer) = listener.accept().await?;
        println!("got connection from {peer}");
        // Can also spawn tasks, it's full tokio in here.
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
