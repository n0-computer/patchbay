//! Exercise local rootless Podman over Patchbay links and across lab sessions.

#![cfg(target_os = "linux")]

use std::{net::SocketAddr, process::Stdio, time::Duration};

use anyhow::{ensure, Context, Result};
use patchbay::{podman::Podman, Device, Lab, RouterPreset};

#[ctor::ctor(unsafe)]
fn init() {
    // SAFETY: The constructor runs before libtest threads start. Read the
    // environment through libc and leave the helper off for ordinary test runs.
    unsafe {
        if !libc::getenv(c"PATCHBAY_ETCD_IMAGE".as_ptr()).is_null() {
            patchbay::init_userns_with_podman_for_ctor();
        }
    }
}

const KEY: &str = "cGF0Y2hiYXk=";
const FIRST_VALUE: &str = "Zmlyc3Q=";
const SECOND_VALUE: &str = "c2Vjb25k";

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires rootless Podman, patchbay-podman-helper, curl, and PATCHBAY_ETCD_IMAGE"]
async fn etcd_across_a_patchbay_link() -> Result<()> {
    let etcd = Etcd::start().await?;
    etcd.put(FIRST_VALUE).await?;
    etcd.check_value(FIRST_VALUE).await?;

    let iface = etcd
        .server
        .default_iface()
        .context("missing server interface")?;
    iface.link_down().await?;
    ensure!(
        !can_connect(&etcd.client, etcd.addr).await?,
        "client could still connect with the etcd link down"
    );
    iface.link_up().await?;
    etcd.check_value(FIRST_VALUE).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires rootless Podman, patchbay-podman-helper, curl, and PATCHBAY_ETCD_IMAGE"]
async fn dropping_a_session_only_stops_its_own_container() -> Result<()> {
    let first = Etcd::start().await?;
    let second = Etcd::start().await?;
    first.put(FIRST_VALUE).await?;
    second.put(SECOND_VALUE).await?;
    first.check_value(FIRST_VALUE).await?;
    second.check_value(SECOND_VALUE).await?;

    drop(first.session);
    ensure!(
        !can_connect(&first.client, first.addr).await?,
        "the first container still accepts connections after session cleanup"
    );
    second.check_value(SECOND_VALUE).await
}

struct Etcd {
    // Keep the topology up while testing what happens after session drop.
    _lab: Lab,
    session: Podman,
    server: Device,
    client: Device,
    addr: SocketAddr,
}

impl Etcd {
    async fn start() -> Result<Self> {
        let image = std::env::var("PATCHBAY_ETCD_IMAGE")
            .context("set PATCHBAY_ETCD_IMAGE to a locally cached etcd v3 image")?;
        let token = patchbay::podman_session_token()?;
        let lab = Lab::new().await?;
        let session = lab.create_podman(token)?;
        let dc = lab
            .add_router("dc")
            .preset(RouterPreset::PublicV4)
            .build()
            .await?;
        let home = lab
            .add_router("home")
            .preset(RouterPreset::Home)
            .build()
            .await?;
        let server = lab
            .add_device("etcd")
            .iface("eth0", dc.id())
            .build()
            .await?;
        let client = lab
            .add_device("client")
            .iface("eth0", home.id())
            .build()
            .await?;
        let addr = SocketAddr::from((server.ip().context("missing server IPv4 address")?, 2379));

        let container = session
            .container(image)
            .entrypoint("/usr/local/bin/etcd")
            .arg("--listen-client-urls=http://0.0.0.0:2379")
            .arg("--advertise-client-urls=http://localhost:2379")
            .spawn_in(&server)
            .await?;
        let etcd = Self {
            _lab: lab,
            session,
            server,
            client,
            addr,
        };
        // Probe from a Patchbay device so readiness also checks the lab path.
        for _ in 0..20 {
            if etcd.request("/health", None).await.is_ok() {
                return Ok(etcd);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        etcd.request("/health", None)
            .await
            .with_context(|| format!("etcd container {} did not become healthy", container.id()))?;
        Ok(etcd)
    }

    async fn put(&self, value: &str) -> Result<()> {
        self.request(
            "/v3/kv/put",
            Some(serde_json::json!({ "key": KEY, "value": value })),
        )
        .await?;
        Ok(())
    }

    async fn check_value(&self, value: &str) -> Result<()> {
        let output = self
            .request("/v3/kv/range", Some(serde_json::json!({ "key": KEY })))
            .await?;
        let response: serde_json::Value = serde_json::from_slice(&output)?;
        ensure!(
            response["kvs"][0]["value"] == value,
            "unexpected etcd response: {response}"
        );
        Ok(())
    }

    async fn request(&self, path: &str, body: Option<serde_json::Value>) -> Result<Vec<u8>> {
        let url = format!("http://{}{path}", self.addr);
        let mut cmd = tokio::process::Command::new("curl");
        cmd.args([
            "--disable",
            "--noproxy",
            "*",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "2",
            &url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        if let Some(body) = body {
            cmd.args([
                "--header",
                "Content-Type: application/json",
                "--data",
                &body.to_string(),
            ]);
        }
        let output = self.client.spawn_command(cmd)?.wait_with_output().await?;
        ensure!(
            output.status.success(),
            "curl {url} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output.stdout)
    }
}

async fn can_connect(client: &Device, addr: SocketAddr) -> Result<bool> {
    Ok(client
        .spawn(move |_| async move {
            matches!(
                tokio::time::timeout(Duration::from_secs(1), tokio::net::TcpStream::connect(addr))
                    .await,
                Ok(Ok(_))
            )
        })?
        .await?)
}
