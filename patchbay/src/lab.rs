//! High-level lab API: [`Lab`], [`LabBuilder`], [`Ix`], topology types.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    thread,
};

use anyhow::{anyhow, bail, Context, Result};
use ipnet::{Ipv4Net, Ipv6Net};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span};

pub use crate::qdisc::LinkLimits;
use crate::{
    config::LabConfig,
    core::{
        self, CoreConfig, DeviceData, DownstreamPool, NetworkCore, NodeId, RouterData,
        RA_DEFAULT_ENABLED, RA_DEFAULT_INTERVAL_SECS, RA_DEFAULT_LIFETIME_SECS,
    },
    device::{Device, DeviceBuilder},
    event::{LabEvent, LabEventKind},
    iface::IfaceConfig,
    netlink::Netlink,
    nft::apply_or_remove_impair,
    router::{Router, RouterBuilder},
    wiring::{self, setup_root_ns_async, setup_router_async, RouterSetupData},
};

pub(crate) static LAB_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── address construction helpers ─────────────────────────────────────

/// Constructs a /`prefix` network from components, e.g. `net4(198, 18, 0, 0, 24)`.
fn net4(a: u8, b: u8, c: u8, d: u8, prefix: u8) -> Ipv4Net {
    Ipv4Net::new(Ipv4Addr::new(a, b, c, d), prefix).expect("valid prefix len")
}

pub(crate) fn net6(addr: Ipv6Addr, prefix: u8) -> Ipv6Net {
    Ipv6Net::new(addr, prefix).expect("valid prefix len")
}

/// Base address for a region's /20 block: `198.18.{idx*16}.0`.
fn region_base(idx: u8) -> Ipv4Addr {
    Ipv4Addr::new(
        198,
        18,
        idx.checked_mul(16).expect("region idx overflow"),
        0,
    )
}

pub use crate::{
    firewall::{Firewall, FirewallConfig, FirewallConfigBuilder},
    nat::{
        ConntrackTimeouts, IpSupport, Nat, NatConfig, NatConfigBuilder, NatFiltering, NatMapping,
        NatV6Mode,
    },
};

/// Direction for applying link impairment.
///
/// When set on a device interface, `Egress` applies the `tc netem` qdisc to the
/// device-side veth (affecting outgoing traffic), `Ingress` applies it to the
/// bridge-side veth in the router namespace (affecting incoming traffic to the
/// device), and `Both` applies impairment to both sides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkDirection {
    /// Apply impairment to both the device-side and bridge-side veths.
    #[default]
    Both,
    /// Apply impairment only to the device-side veth (outgoing traffic).
    Egress,
    /// Apply impairment only to the bridge-side veth (incoming traffic).
    Ingress,
}

/// Link-layer impairment profile applied via `tc netem`.
///
/// Named presets model common last-mile conditions. Use [`LinkCondition::Manual`]
/// with [`LinkLimits`] for full control over all `tc netem` parameters.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkCondition {
    /// Wired LAN (1G Ethernet). No impairment.
    ///
    /// Use for datacenter-local, same-rack communication.
    Lan,
    /// Good WiFi — 5 GHz band, close to AP, low contention.
    ///
    /// 5 ms one-way delay, 2 ms jitter, 0.1 % loss.
    Wifi,
    /// Congested WiFi — 2.4 GHz, far from AP, interference.
    ///
    /// 40 ms one-way delay, 15 ms jitter, 2 % loss, 20 Mbit.
    WifiBad,
    /// 4G/LTE good signal.
    ///
    /// 25 ms one-way delay, 8 ms jitter, 0.5 % loss.
    Mobile4G,
    /// 3G or degraded 4G.
    ///
    /// 100 ms one-way delay, 30 ms jitter, 2 % loss, 2 Mbit.
    Mobile3G,
    /// LEO satellite (Starlink-class).
    ///
    /// 40 ms one-way delay, 7 ms jitter, 1 % loss.
    Satellite,
    /// GEO satellite (HughesNet/Viasat).
    ///
    /// 300 ms one-way delay, 20 ms jitter, 0.5 % loss, 25 Mbit.
    SatelliteGeo,
    /// Fully custom impairment parameters.
    Manual(LinkLimits),
}

impl<'de> Deserialize<'de> for LinkCondition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Preset(String),
            Manual(LinkLimits),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Preset(s) => match s.as_str() {
                "lan" => Ok(LinkCondition::Lan),
                "wifi" => Ok(LinkCondition::Wifi),
                "wifi-bad" => Ok(LinkCondition::WifiBad),
                "mobile-4g" | "mobile" => Ok(LinkCondition::Mobile4G),
                "mobile-3g" => Ok(LinkCondition::Mobile3G),
                "satellite" => Ok(LinkCondition::Satellite),
                "satellite-geo" => Ok(LinkCondition::SatelliteGeo),
                _ => Err(serde::de::Error::custom(format!(
                    "unknown link condition preset '{s}'"
                ))),
            },
            Repr::Manual(limits) => Ok(LinkCondition::Manual(limits)),
        }
    }
}

impl LinkCondition {
    /// Converts this preset (or manual config) into concrete [`LinkLimits`].
    pub fn to_limits(self) -> LinkLimits {
        match self {
            LinkCondition::Lan => LinkLimits::default(),
            LinkCondition::Wifi => LinkLimits {
                latency_ms: 5,
                jitter_ms: 2,
                loss_pct: 0.1,
                ..Default::default()
            },
            LinkCondition::WifiBad => LinkLimits {
                latency_ms: 40,
                jitter_ms: 15,
                loss_pct: 2.0,
                rate_kbit: 20_000,
                ..Default::default()
            },
            LinkCondition::Mobile4G => LinkLimits {
                latency_ms: 25,
                jitter_ms: 8,
                loss_pct: 0.5,
                ..Default::default()
            },
            LinkCondition::Mobile3G => LinkLimits {
                latency_ms: 100,
                jitter_ms: 30,
                loss_pct: 2.0,
                rate_kbit: 2_000,
                ..Default::default()
            },
            LinkCondition::Satellite => LinkLimits {
                latency_ms: 40,
                jitter_ms: 7,
                loss_pct: 1.0,
                ..Default::default()
            },
            LinkCondition::SatelliteGeo => LinkLimits {
                latency_ms: 300,
                jitter_ms: 20,
                loss_pct: 0.5,
                rate_kbit: 25_000,
                ..Default::default()
            },
            LinkCondition::Manual(limits) => limits,
        }
    }
}

// ─────────────────────────────────────────────
// Region
// ─────────────────────────────────────────────

/// Handle for a network region backed by a real router namespace.
///
/// Regions model geographic proximity: routers within a region share a bridge,
/// and inter-region traffic flows over veths with configurable netem impairment.
#[derive(Clone)]
pub struct Region {
    pub(crate) name: Arc<str>,
    pub(crate) idx: u8,
    pub(crate) router_id: NodeId,
}

impl Region {
    /// Region name (e.g. "us", "eu").
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The underlying region router's node ID.
    pub fn router_id(&self) -> NodeId {
        self.router_id
    }
}

/// Parameters for an inter-region link passed to [`Lab::link_regions`].
#[derive(Clone, Debug)]
pub struct RegionLink {
    /// One-way latency in milliseconds (RTT = 2x).
    pub latency_ms: u32,
    /// Jitter in milliseconds (uniform distribution around `latency_ms`).
    pub jitter_ms: u32,
    /// Packet loss percentage (0.0–100.0).
    pub loss_pct: f64,
    /// Rate limit in Mbit/s (0 = unlimited).
    pub rate_mbit: u32,
}

impl RegionLink {
    /// Good inter-region link: only latency, no jitter or loss.
    pub fn good(latency_ms: u32) -> Self {
        Self {
            latency_ms,
            jitter_ms: 0,
            loss_pct: 0.0,
            rate_mbit: 0,
        }
    }

    /// Degraded link: jitter = latency/10, 0.5% loss, no rate limit.
    pub fn degraded(latency_ms: u32) -> Self {
        Self {
            latency_ms,
            jitter_ms: latency_ms / 10,
            loss_pct: 0.5,
            rate_mbit: 0,
        }
    }
}

/// Pre-built regions from [`Lab::add_default_regions`].
pub struct DefaultRegions {
    /// US region (198.18.0.0/20).
    pub us: Region,
    /// EU region (198.18.16.0/20).
    pub eu: Region,
    /// Asia region (198.18.32.0/20).
    pub asia: Region,
}

// ─────────────────────────────────────────────
// LabInner
// ─────────────────────────────────────────────

/// Shared lab interior — holds both the topology mutex and the namespace
/// manager. `netns` and `cancel` live here (not behind the mutex) because
/// they are `Arc`-shared and internally synchronized.
pub(crate) struct LabInner {
    pub core: std::sync::Mutex<NetworkCore>,
    pub netns: Arc<crate::netns::NetnsManager>,
    pub cancel: CancellationToken,
    /// Monotonically increasing event counter.
    pub opid: AtomicU64,
    /// Broadcast channel for lab events.
    pub events_tx: tokio::sync::broadcast::Sender<LabEvent>,
    /// Human-readable lab label (immutable after construction).
    pub label: Option<Arc<str>>,
    /// Namespace name → node name mapping (for log file naming).
    pub ns_to_name: std::sync::Mutex<HashMap<String, String>>,
    /// Resolved run output directory (e.g. `{base}/{ts}-{label}/`), if outdir was configured.
    pub run_dir: Option<PathBuf>,
    /// IPv6 duplicate address detection behavior.
    pub ipv6_dad_mode: Ipv6DadMode,
    /// IPv6 provisioning behavior.
    pub ipv6_provisioning_mode: Ipv6ProvisioningMode,
    /// In-process DNS server on the IX bridge (lazy, started on first access).
    pub dns_server: std::sync::Mutex<Option<crate::dns_server::DnsServer>>,
    /// Writer task handle (kept alive until lab is dropped).
    /// Writer task handle. The task listens to `cancel` for graceful shutdown
    /// (draining events + final flush). Stored as a bare JoinHandle so the
    /// task can complete its cleanup during Runtime::drop rather than being
    /// aborted immediately.
    pub writer_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Test outcome flag shared with the writer and [`TestGuard`].
    pub test_status: Arc<AtomicU8>,
    /// Accumulated lab state, shared with the background writer. Updated on
    /// every event so that `Drop` can always write a complete `state.json`
    /// synchronously, even if the async writer task never ran its shutdown path.
    pub shared_state: Arc<std::sync::Mutex<crate::event::LabState>>,
    /// Registry for flushing all buffered writers during lab shutdown.
    pub flush_registry: Arc<crate::writer::FlushRegistry>,
}

impl Drop for LabInner {
    fn drop(&mut self) {
        // Fallback cancellation in case the drop guard was bypassed.
        self.cancel.cancel();
    }
}

/// Shutdown guard that fires when the last [`Lab`] clone drops.
///
/// Holds `Arc<LabInner>` and derefs to it, so `Lab` methods access
/// LabInner fields transparently through the double deref chain
/// `Arc<LabDropGuard> -> LabDropGuard -> Arc<LabInner> -> LabInner`.
///
/// Device, Router, and Iface handles hold `Arc<LabInner>` directly,
/// bypassing this guard. This means the guard drops when the last Lab
/// drops, not when the last handle drops. The shutdown sequence:
///
/// 1. Cancel the lab-wide `CancellationToken`. This wakes the writer
///    task (for graceful event drain), RA workers, and NAT64 loops.
/// 2. Cancel all namespace workers (async and sync). Async worker
///    runtimes shut down, aborting all spawned tasks including user
///    tasks from `device.spawn()`. Sync workers exit after completing
///    any in-flight `run_sync` closure.
/// 3. Flush all registered writers via `FlushRegistry::flush_all()`.
///    This synchronously flushes events.jsonl and per-namespace tracing
///    logs (`.tracing.jsonl`, `.ansi`, `.events.jsonl`, `.metrics`).
/// 4. Write the final `state.json` with the test outcome status.
///
/// After this, handles remain functional (LabInner is still alive) but
/// the lab is logically shut down: the cancel token is fired, files are
/// flushed, and no new events will be written.
pub(crate) struct LabDropGuard(pub(crate) Arc<LabInner>);

impl std::ops::Deref for LabDropGuard {
    type Target = LabInner;
    fn deref(&self) -> &LabInner {
        &self.0
    }
}

impl Drop for LabDropGuard {
    fn drop(&mut self) {
        // Cancel the lab-wide token (wakes internal tasks like the
        // writer, RA workers, NAT64 loops).
        self.0.cancel.cancel();
        // Cancel all namespace worker tokens. This causes each worker's
        // tokio runtime to shut down, aborting all spawned tasks
        // (including user tasks from device.spawn()).
        self.0.netns.cancel_all_workers();
        // Flush all buffered file writers to disk.
        self.0.flush_registry.flush_all();

        // Determine final status from the test guard.
        let status = match self.0.test_status.load(Ordering::Acquire) {
            crate::writer::STATUS_SUCCESS => "success",
            crate::writer::STATUS_FAILED => "failed",
            _ => "stopped",
        };

        // Write the final state.json synchronously. The shared_state mutex
        // holds the fully accumulated LabState (updated by the writer on every
        // event), so this works even if the async task never flushed.
        if let Some(ref run_dir) = self.0.run_dir {
            crate::writer::write_final_state(run_dir, &self.0.shared_state, status);
        }
    }
}

impl LabInner {
    /// Returns a cloned tokio runtime handle for the given namespace.
    pub(crate) fn rt_handle_for(&self, ns: &str) -> Result<tokio::runtime::Handle> {
        self.netns.rt_handle_for(ns)
    }

    /// Spawns an async UDP reflector in the given namespace.
    ///
    /// Returns after the socket is confirmed bound. The returned
    /// [`ReflectorGuard`](core::ReflectorGuard) aborts the reflector task when dropped.
    pub(crate) async fn spawn_reflector_in(
        &self,
        ns: &str,
        bind: SocketAddr,
    ) -> Result<core::ReflectorGuard> {
        let cancel = self.cancel.clone();
        let rt = self.rt_handle_for(ns)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = rt.spawn(async move {
            if let Err(e) = crate::test_utils::run_reflector(bind, cancel, tx).await {
                tracing::error!(bind = %bind, error = %e, "reflector failed");
            }
        });
        rx.await
            .map_err(|_| anyhow!("reflector task exited before signalling bind"))?
            .context("reflector bind failed")?;
        Ok(core::ReflectorGuard(handle.abort_handle()))
    }

    // ── with() helpers ──────────────────────────────────────────────────

    pub(crate) fn with_device<R>(&self, id: NodeId, f: impl FnOnce(&DeviceData) -> R) -> Option<R> {
        let core = self.core.lock().unwrap();
        core.device(id).map(f)
    }

    pub(crate) fn with_router<R>(&self, id: NodeId, f: impl FnOnce(&RouterData) -> R) -> Option<R> {
        let core = self.core.lock().unwrap();
        core.router(id).map(f)
    }
}

// ─────────────────────────────────────────────
// Lab
// ─────────────────────────────────────────────

/// A virtual network lab running in Linux network namespaces.
///
/// `Lab` is cheaply cloneable. All methods take `&self` and use interior
/// mutability. Use [`Lab::new`] or [`Lab::builder()`] to create a lab,
/// then build routers and devices with [`Lab::add_router`] and
/// [`Lab::add_device`].
///
/// # Shutdown
///
/// When the last `Lab` clone is dropped, the lab shuts down:
///
/// - All namespace async runtimes are cancelled, aborting every task
///   spawned via [`Device::spawn`](crate::Device::spawn) or
///   [`Router::spawn`](crate::Router::spawn), as well as internal
///   tasks (RA workers, NAT64 translators). The `JoinHandle` returned
///   by `spawn` will resolve to a cancelled `JoinError`.
/// - All namespace sync workers are stopped. Any in-flight
///   [`Device::run_sync`](crate::Device::run_sync) closure completes
///   before the worker exits.
/// - The event writer task is cancelled.
/// - All buffered log files are flushed to disk: `events.jsonl` and
///   per-namespace tracing logs.
/// - The final `state.json` is written with the test outcome
///   (`success`, `failed`, or `stopped`).
///
/// [`Device`](crate::Device), [`Router`](crate::Router), and
/// [`Iface`](crate::Iface) handles do not prevent shutdown. They
/// remain usable for reading cached data after the lab drops, but
/// operations that require the namespace runtime will fail.
///
/// OS threads spawned via
/// [`Device::spawn_thread`](crate::Device::spawn_thread) are not
/// cancelled. They run until the closure returns. The caller is
/// responsible for arranging termination (e.g. via a shared flag).
///
/// Child processes spawned via
/// [`Device::spawn_command`](crate::Device::spawn_command) are not
/// killed. They continue running until they exit or the namespace is
/// reclaimed by the kernel when all file descriptors to it close.
#[derive(Clone)]
pub struct Lab {
    pub(crate) inner: Arc<LabDropGuard>,
}

/// Builder for constructing a [`Lab`] with custom configuration.
///
/// Use [`Lab::builder()`] to obtain a `LabBuilder`, configure output directory
/// and label with the builder methods, then call [`.build()`](LabBuilder::build)
/// to create the lab.
///
/// # Example
/// ```no_run
/// # use patchbay::{Lab, LabBuilder, OutDir};
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> anyhow::Result<()> {
/// let lab = Lab::builder()
///     .outdir(OutDir::Nested("/tmp/patchbay-out".into()))
///     .label("my-test")
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct LabBuilder {
    outdir: Option<OutDir>,
    label: Option<String>,
    ipv6_dad_mode: Ipv6DadMode,
    ipv6_provisioning_mode: Ipv6ProvisioningMode,
    allow_real_root: bool,
    config: Option<LabConfig>,
}

/// Where the lab writes event logs and state files.
#[derive(Clone, Debug)]
pub enum OutDir {
    /// Parent directory — lab creates a timestamped subdirectory inside it.
    Nested(PathBuf),
    /// Exact directory — lab writes directly here, no subdirectory created.
    Exact(PathBuf),
}

/// Controls IPv6 duplicate address detection behavior in created namespaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ipv6DadMode {
    /// Keep kernel default behavior, DAD enabled.
    Enabled,
    /// Disable DAD for deterministic fast tests.
    #[default]
    Disabled,
}

/// Controls how IPv6 routes are provisioned for hosts and routers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ipv6ProvisioningMode {
    /// Install routes directly from patchbay wiring logic.
    #[default]
    Static,
    /// RA-driven route provisioning mode.
    ///
    /// This mode follows RA and RS semantics for route installation and
    /// emits structured RA and RS events into patchbay logs. It does not
    /// emit raw ICMPv6 Router Advertisement or Router Solicitation packets.
    RaDriven,
}

/// IPv6 behavior profile for a lab, controlling DAD and route provisioning.
///
/// `Deterministic` keeps tests fast and reproducible by disabling DAD and
/// wiring routes statically. `Realistic` enables DAD and RA/RS-driven
/// provisioning, matching how real networks operate. Use `Realistic` when
/// your application depends on RA timing, default-route installation
/// order, or link-local gateway behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ipv6Profile {
    /// DAD disabled, static route wiring. Fast and reproducible for tests.
    Deterministic,
    /// DAD enabled, RA/RS-driven route provisioning. Matches real-world
    /// network behavior where routers announce prefixes and hosts learn
    /// routes through Router Advertisements.
    Realistic,
}

impl Ipv6Profile {
    fn modes(self) -> (Ipv6DadMode, Ipv6ProvisioningMode) {
        match self {
            Self::Deterministic => (Ipv6DadMode::Disabled, Ipv6ProvisioningMode::Static),
            Self::Realistic => (Ipv6DadMode::Enabled, Ipv6ProvisioningMode::RaDriven),
        }
    }
}

impl LabBuilder {
    /// Sets the output directory for event log and state files.
    pub fn outdir(mut self, outdir: OutDir) -> Self {
        self.outdir = Some(outdir);
        self
    }

    /// Sets a human-readable label for this lab (used in output directory naming).
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Reads the output directory from the `PATCHBAY_OUTDIR` environment variable
    /// as [`OutDir::Nested`]. Does nothing if the variable is absent.
    pub fn outdir_from_env(mut self) -> Self {
        if let Ok(v) = std::env::var("PATCHBAY_OUTDIR") {
            self.outdir = Some(OutDir::Nested(v.into()));
        }
        self
    }

    /// Sets IPv6 duplicate address detection behavior.
    pub fn ipv6_dad_mode(mut self, mode: Ipv6DadMode) -> Self {
        self.ipv6_dad_mode = mode;
        self
    }

    /// Allows running as real root without a user namespace.
    ///
    /// By default, `Lab::new` refuses to run as real root (UID 0 outside a
    /// user namespace) because mount operations could leak to the host
    /// filesystem. Call `init_userns` before creating a lab to enter a user
    /// namespace, or set this flag to bypass the check.
    pub fn allow_real_root(mut self) -> Self {
        self.allow_real_root = true;
        self
    }

    /// Sets IPv6 provisioning behavior.
    pub fn ipv6_provisioning_mode(mut self, mode: Ipv6ProvisioningMode) -> Self {
        self.ipv6_provisioning_mode = mode;
        self
    }

    /// Loads a TOML lab configuration. The config is applied during
    /// [`build`](Self::build), creating all routers and devices defined
    /// in the file. Builder options (outdir, label, etc.) take precedence
    /// over config defaults.
    pub fn config(mut self, cfg: LabConfig) -> Self {
        self.config = Some(cfg);
        self
    }

    /// Applies a deployment profile that sets both DAD and v6 provisioning mode.
    pub fn ipv6_profile(mut self, profile: Ipv6Profile) -> Self {
        let (dad, provisioning) = profile.modes();
        self.ipv6_dad_mode = dad;
        self.ipv6_provisioning_mode = provisioning;
        self
    }

    /// Builds the [`Lab`] with the configured options.
    ///
    /// If a TOML [`config`](Self::config) was set, all routers and devices
    /// defined in the config are created during build.
    pub async fn build(self) -> Result<Lab> {
        Lab::build(self).await
    }
}

impl Lab {
    // ── Constructors ────────────────────────────────────────────────────

    /// Refuses to run as real root (UID 0 in the initial user namespace).
    ///
    /// Running as real root without a user namespace is dangerous because
    /// mount operations can leak to the host filesystem. Call `init_userns`
    /// before creating a lab to enter a user namespace.
    fn refuse_real_root() -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            if !nix::unistd::Uid::effective().is_root() {
                return Ok(());
            }
            // Check if we're in a non-initial user namespace by comparing
            // our user namespace inode to the init namespace (pid 1).
            let my_userns = std::fs::read_link("/proc/self/ns/user")
                .ok()
                .and_then(|p| p.to_str().map(String::from));
            let init_userns = std::fs::read_link("/proc/1/ns/user")
                .ok()
                .and_then(|p| p.to_str().map(String::from));
            if my_userns == init_userns {
                anyhow::bail!(
                    "refusing to run as real root (not inside a user namespace). \
                     Call patchbay::init_userns() before creating a Lab, or use \
                     Lab::builder().allow_real_root() to bypass this check."
                );
            }
        }
        Ok(())
    }

    /// Returns a [`LabBuilder`] for configuring and constructing a [`Lab`].
    pub fn builder() -> LabBuilder {
        LabBuilder::default()
    }

    /// Creates a new lab with default address ranges and IX settings.
    ///
    /// Reads `PATCHBAY_OUTDIR` from the environment for event output.
    /// Use [`Lab::builder()`] for explicit configuration.
    pub async fn new() -> Result<Self> {
        Self::builder().outdir_from_env().build().await
    }

    /// Internal constructor used by [`LabBuilder::build`].
    async fn build(mut opts: LabBuilder) -> Result<Self> {
        if !opts.allow_real_root {
            Self::refuse_real_root()?;
        }
        let config = opts.config.take();
        let pid = std::process::id();
        let pid_tag = pid % 9999 + 1;
        let lab_seq = LAB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let uniq = format!("{lab_seq:x}");
        let prefix = format!("lab-p{}{}", pid_tag, uniq); // e.g. "lab-p12340"
        let root_ns = format!("lab{lab_seq}-root");
        let bridge_tag = format!("p{}{}", pid_tag, uniq);
        let ix_gw = Ipv4Addr::new(198, 18, 0, 1);
        let label: Option<Arc<str>> = opts.label.map(|s| Arc::from(s.as_str()));
        let lab_span = debug_span!("lab", id = lab_seq);
        {
            let _enter = lab_span.enter();
            debug!(prefix = %prefix, "lab: created");
        }
        let core = NetworkCore::new(CoreConfig {
            lab_id: lab_seq,
            prefix: prefix.clone().into(),
            root_ns: root_ns.into(),
            bridge_tag: bridge_tag.into(),
            ix_br: format!("br-p{}{}-1", pid_tag, uniq).into(),
            ix_gw,
            ix_cidr: net4(198, 18, 0, 0, 24),
            private_cidr: net4(10, 0, 0, 0, 16),
            public_cidr: net4(198, 18, 1, 0, 24),
            ix_gw_v6: Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
            ix_cidr_v6: net6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 64),
            private_cidr_v6: net6(Ipv6Addr::new(0xfd10, 0, 0, 0, 0, 0, 0, 0), 48),
            public_cidr_v6: net6(Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 0), 48),
            span: lab_span,
        })
        .context("failed to create DNS entries directory")?;

        // Compute run_dir before constructing LabInner (needed for writer + tracing).
        let run_dir = opts.outdir.map(|od| match od {
            OutDir::Exact(p) => p,
            OutDir::Nested(base) => {
                let label_or_prefix = label
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| prefix.clone());
                let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
                base.join(format!("{ts}-{label_or_prefix}"))
            }
        });

        let flush_registry = Arc::new(crate::writer::FlushRegistry::new());
        let mut netns_mgr = crate::netns::NetnsManager::new();
        if let Some(ref rd) = run_dir {
            netns_mgr.set_run_dir(rd.clone());
        }
        netns_mgr.set_flush_registry(Arc::clone(&flush_registry));
        let netns = Arc::new(netns_mgr);
        let cancel = CancellationToken::new();
        let (events_tx, _rx) = tokio::sync::broadcast::channel::<LabEvent>(256);
        drop(_rx);
        let test_status = Arc::new(AtomicU8::new(crate::writer::STATUS_UNKNOWN));
        let shared_state = Arc::new(std::sync::Mutex::new(crate::event::LabState::default()));

        let lab_inner = Arc::new(LabInner {
            core: std::sync::Mutex::new(core),
            netns: Arc::clone(&netns),
            cancel,
            opid: AtomicU64::new(0),
            events_tx,
            label: label.clone(),
            ns_to_name: std::sync::Mutex::new(HashMap::new()),
            run_dir: run_dir.clone(),
            ipv6_dad_mode: opts.ipv6_dad_mode,
            ipv6_provisioning_mode: opts.ipv6_provisioning_mode,
            dns_server: std::sync::Mutex::new(None),
            writer_handle: std::sync::Mutex::new(None),
            test_status: test_status.clone(),
            shared_state: shared_state.clone(),
            flush_registry,
        });
        let lab = Self {
            inner: Arc::new(LabDropGuard(lab_inner)),
        };
        // Initialize root namespace and IX bridge eagerly — no lazy-init race.
        let cfg = lab.inner.core.lock().unwrap().cfg.clone();
        setup_root_ns_async(&cfg, &netns, opts.ipv6_dad_mode)
            .await
            .context("failed to set up root namespace")?;

        // Spawn file writer if outdir is configured -- subscribe before emitting
        // initial events so the writer captures LabCreated and IxCreated.
        if let Some(ref run_dir) = run_dir {
            let events_file = Arc::new(
                crate::writer::SharedEventsFile::new(run_dir)
                    .context("failed to create events file")?,
            );
            lab.inner.flush_registry.register(events_file.clone());
            let handle = crate::writer::spawn_writer(
                run_dir.clone(),
                lab.inner.events_tx.subscribe(),
                lab.inner.cancel.clone(),
                shared_state,
                events_file,
            );
            *lab.inner.writer_handle.lock().unwrap() = Some(handle);
        }

        // Emit lifecycle events.
        lab.inner.emit(LabEventKind::LabCreated {
            lab_prefix: cfg.prefix.to_string(),
            label: label.as_ref().map(|s| s.to_string()),
        });
        lab.inner.emit(LabEventKind::IxCreated {
            bridge: cfg.ix_br.to_string(),
            cidr: cfg.ix_cidr,
            gw: cfg.ix_gw,
            cidr_v6: cfg.ix_cidr_v6,
            gw_v6: cfg.ix_gw_v6,
        });

        // Apply TOML config if provided.
        if let Some(cfg) = config {
            lab.apply_config(cfg).await?;
        }

        Ok(lab)
    }

    /// Returns the unique resource prefix associated with this lab instance.
    pub fn prefix(&self) -> String {
        self.inner.core.lock().unwrap().cfg.prefix.to_string()
    }

    /// Subscribe to the lab event stream.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<LabEvent> {
        self.inner.events_tx.subscribe()
    }

    /// Returns the resolved run output directory, if outdir was configured.
    ///
    /// This is the `{base}/{timestamp}-{label}` subdirectory where events, state,
    /// and per-namespace tracing logs are written.
    pub fn run_dir(&self) -> Option<&Path> {
        self.inner.run_dir.as_deref()
    }

    /// Returns the human-readable label, if one was set at construction.
    pub fn label(&self) -> Option<&str> {
        self.inner.label.as_deref()
    }

    /// Returns a guard that records whether the test passed or failed.
    ///
    /// On drop the guard checks [`std::thread::panicking`] and writes
    /// "failed" to state.json if a panic is unwinding. Call [`.ok()`](TestGuard::ok)
    /// at the end of a successful test to record "success" explicitly.
    /// If neither `.ok()` is called nor a panic occurs (e.g. the test returns
    /// `Err`), the status defaults to "failed" -- a safe default that avoids
    /// false positives.
    pub fn test_guard(&self) -> TestGuard {
        TestGuard {
            inner: Arc::clone(&self.inner.0),
            marked: false,
        }
    }

    /// Parses `lab.toml`, builds the network, and returns a ready-to-use lab.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("read lab config")?;
        let cfg: LabConfig = toml::from_str(&text).context("parse lab config")?;
        Self::builder().outdir_from_env().config(cfg).build().await
    }

    /// Applies a parsed TOML config to an already-built lab, creating all
    /// routers and devices defined in the config.
    async fn apply_config(&self, cfg: LabConfig) -> Result<()> {
        // Region latency pairs from TOML config are ignored in the new region API.
        // TODO: support regions in TOML config via add_region / link_regions.

        // Routers: topological sort — process any router whose upstream is already resolved.
        let mut pending: HashMap<&str, &crate::config::RouterConfig> =
            cfg.router.iter().map(|r| (r.name.as_str(), r)).collect();
        loop {
            let ready: Vec<&str> = pending
                .keys()
                .copied()
                .filter(|&name| {
                    pending[name]
                        .upstream
                        .as_deref()
                        .map(|up| !pending.contains_key(up))
                        .unwrap_or(true)
                })
                .collect();
            if ready.is_empty() {
                break;
            }
            // Sort for deterministic order (parent before child, stable within same depth).
            let mut ready = ready;
            ready.sort();
            for name in ready {
                let rcfg = pending.remove(name).unwrap();
                let upstream = {
                    let inner = self.inner.core.lock().unwrap();
                    rcfg.upstream
                        .as_deref()
                        .and_then(|n| inner.router_id_by_name(n))
                };
                let mut rb = self
                    .add_router(&rcfg.name)
                    .nat(rcfg.nat)
                    .ip_support(rcfg.ip_support)
                    .nat_v6(rcfg.nat_v6);
                if let Some(enabled) = rcfg.ra_enabled {
                    rb = rb.ra_enabled(enabled);
                }
                if let Some(interval) = rcfg.ra_interval_secs {
                    rb = rb.ra_interval_secs(interval);
                }
                if let Some(lifetime) = rcfg.ra_lifetime_secs {
                    rb = rb.ra_lifetime_secs(lifetime);
                }
                // TODO: support region assignment from TOML config via add_region.
                if let Some(u) = upstream {
                    rb = rb.upstream(u);
                }
                rb.build().await?;
            }
        }
        if !pending.is_empty() {
            let mut names: Vec<_> = pending.keys().copied().collect();
            names.sort();
            bail!(
                "unresolvable router upstreams (cycle?): {}",
                names.join(", ")
            );
        }

        // Devices — parse raw TOML, pre-resolve router IDs, then build.
        struct ParsedDev {
            name: String,
            default_via: Option<String>,
            ifaces: Vec<(String, NodeId, Option<LinkCondition>)>,
        }

        let dev_data: Vec<ParsedDev> = {
            let mut dev_names: Vec<&String> = cfg.device.keys().collect();
            dev_names.sort();
            let mut result = Vec::new();
            for dev_name in dev_names {
                let dev_val = &cfg.device[dev_name];
                let dev_table = dev_val
                    .as_table()
                    .ok_or_else(|| anyhow!("device '{}' must be a TOML table", dev_name))?;
                let default_via = dev_table
                    .get("default_via")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let count = match dev_table.get("count") {
                    None => 1usize,
                    Some(v) => {
                        let n = v.as_integer().ok_or_else(|| {
                            anyhow!("device '{}' count must be an integer", dev_name)
                        })?;
                        if n < 1 {
                            bail!("device '{}' count must be >= 1", dev_name);
                        }
                        usize::try_from(n)
                            .map_err(|_| anyhow!("device '{}' count out of range", dev_name))?
                    }
                };
                // Interface sub-tables: table-valued keys, excluding scalar device-level keys.
                let mut iface_keys: Vec<&String> = dev_table
                    .keys()
                    .filter(|k| dev_table[*k].is_table())
                    .collect();
                iface_keys.sort();
                if iface_keys.is_empty() {
                    bail!("device '{}' has no interface sub-tables", dev_name);
                }
                let mut ifaces = Vec::new();
                for ifname in iface_keys {
                    let iface_table = dev_table[ifname].as_table().ok_or_else(|| {
                        anyhow!("device '{}' iface '{}' must be a table", dev_name, ifname)
                    })?;
                    let gw_name = iface_table
                        .get("gateway")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            anyhow!("device '{}' iface '{}' missing 'gateway'", dev_name, ifname)
                        })?;
                    let router_id = self
                        .inner
                        .core
                        .lock()
                        .unwrap()
                        .router_id_by_name(gw_name)
                        .ok_or_else(|| {
                            anyhow!(
                                "device '{}' iface '{}' references unknown router '{}'",
                                dev_name,
                                ifname,
                                gw_name
                            )
                        })?;
                    let impair: Option<LinkCondition> = match iface_table.get("impair") {
                        None => None,
                        Some(v) => Some(v.clone().try_into().map_err(|e: toml::de::Error| {
                            anyhow!(
                                "device '{}' iface '{}' invalid impair: {}",
                                dev_name,
                                ifname,
                                e
                            )
                        })?),
                    };
                    ifaces.push((ifname.clone(), router_id, impair));
                }
                if dev_table.contains_key("count") {
                    for idx in 0..count {
                        result.push(ParsedDev {
                            name: format!("{dev_name}-{idx}"),
                            default_via: default_via.clone(),
                            ifaces: ifaces.clone(),
                        });
                    }
                } else {
                    result.push(ParsedDev {
                        name: dev_name.clone(),
                        default_via,
                        ifaces,
                    });
                }
            }
            result
        };
        for dev in dev_data {
            let mut builder = self.add_device(&dev.name);
            for (ifname, router_id, impair) in dev.ifaces {
                let config = IfaceConfig::routed(router_id);
                let config = if let Some(cond) = impair {
                    config.condition(cond, LinkDirection::Both)
                } else {
                    config
                };
                builder = builder.iface(&ifname, config);
            }
            if let Some(via) = dev.default_via {
                builder = builder.default_via(&via);
            }
            builder.build().await?;
        }

        Ok(())
    }

    // ── Builder methods (sync — just populate data structures) ──────────

    /// Begins building a router; returns a [`RouterBuilder`] to configure options.
    ///
    /// Call [`.nat()`][RouterBuilder::nat], [`.region()`][RouterBuilder::region], and/or
    /// [`.upstream()`][RouterBuilder::upstream] as needed, then
    /// [`.build()`][RouterBuilder::build] to finalize.
    ///
    /// Default NAT mode is [`Nat::None`] (public DC-style router, IX-connected).
    pub fn add_router(&self, name: &str) -> RouterBuilder {
        let inner = self.inner.core.lock().unwrap();
        let lab_span = inner.cfg.span.clone();
        if name.starts_with("region_") {
            return RouterBuilder::error(
                Arc::clone(&self.inner.0),
                lab_span,
                name,
                anyhow!("router names starting with 'region_' are reserved"),
            );
        }
        if inner.router_id_by_name(name).is_some() {
            return RouterBuilder::error(
                Arc::clone(&self.inner.0),
                lab_span,
                name,
                anyhow!("router '{}' already exists", name),
            );
        }
        RouterBuilder {
            inner: Arc::clone(&self.inner.0),
            lab_span,
            name: name.to_string(),
            region: None,
            upstream: None,
            nat: None,
            ip_support: IpSupport::V4Only,
            nat_v6: NatV6Mode::None,
            downstream_pool: None,
            downstream_cidr: None,
            downlink_condition: None,
            mtu: None,
            block_icmp_frag_needed: false,
            firewall: Firewall::None,
            ra_enabled: RA_DEFAULT_ENABLED,
            ra_interval_secs: RA_DEFAULT_INTERVAL_SECS,
            ra_lifetime_secs: RA_DEFAULT_LIFETIME_SECS,
            result: Ok(()),
        }
    }

    /// Begins building a device; returns a [`DeviceBuilder`] to configure interfaces.
    ///
    /// Call [`.iface()`][DeviceBuilder::iface] one or more times to attach network
    /// interfaces, then [`.build()`][DeviceBuilder::build] to finalize.
    pub fn add_device(&self, name: &str) -> DeviceBuilder {
        let mut inner = self.inner.core.lock().unwrap();
        let lab_span = inner.cfg.span.clone();
        if inner.device_id_by_name(name).is_some() {
            return DeviceBuilder {
                inner: Arc::clone(&self.inner.0),
                lab_span,
                id: NodeId(u64::MAX),
                mtu: None,
                provisioning_mode: None,
                result: Err(anyhow!("device '{}' already exists", name)),
            };
        }
        let id = inner.add_device(name);
        DeviceBuilder {
            inner: Arc::clone(&self.inner.0),
            lab_span,
            id,
            mtu: None,
            provisioning_mode: None,
            result: Ok(()),
        }
    }

    // ── removal ──────────────────────────────────────────────────────────

    /// Removes a device from the lab, destroying its namespace and all interfaces.
    ///
    /// The kernel automatically destroys veth pairs when the namespace closes.
    pub fn remove_device(&self, id: NodeId) -> Result<()> {
        let dev = self.inner.core.lock().unwrap().remove_device(id)?;
        self.inner.netns.remove_worker(&dev.ns);
        self.inner.emit(LabEventKind::DeviceRemoved {
            name: dev.name.to_string(),
        });
        Ok(())
    }

    /// Removes a router from the lab, destroying its namespace and all interfaces.
    ///
    /// Fails if any devices are still connected to this router's downstream switch.
    /// Remove or replug those devices first.
    pub fn remove_router(&self, id: NodeId) -> Result<()> {
        let router = self.inner.core.lock().unwrap().remove_router(id)?;
        self.inner.netns.remove_worker(&router.ns);
        self.inner.emit(LabEventKind::RouterRemoved {
            name: router.name.to_string(),
        });
        Ok(())
    }

    // ── build ────────────────────────────────────────────────────────────

    // ── User-facing API ─────────────────────────────────────────────────

    // ── Region API ────────────────────────────────────────────────────

    /// Creates a new network region backed by a real router namespace.
    ///
    /// Each region gets a /20 block from 198.18.0.0/15. Routers added with
    /// `.region(&region)` connect to the region's bridge as sub-routers.
    /// Inter-region latency is configured separately via [`link_regions`](Self::link_regions).
    pub async fn add_region(&self, name: &str) -> Result<Region> {
        if name.is_empty() {
            bail!("region name must not be empty");
        }
        let region_router_name = format!("region_{name}");

        // Phase 1: Lock → register topology → unlock.
        let (id, setup_data, idx) = {
            let mut inner = self.inner.core.lock().unwrap();
            if inner.regions.contains_key(name) {
                bail!("region '{name}' already exists");
            }
            let idx = inner.alloc_region_idx()?;

            // Region router: no NAT, public downstream, no region tag (it IS the region).
            // DualStack so it can forward v6 traffic from sub-routers.
            let id = inner.add_router(
                &region_router_name,
                None,
                DownstreamPool::Public,
                None,
                IpSupport::DualStack,
                NatV6Mode::None,
            );

            // Downstream switch: region's first /24 as override CIDR.
            // v6 /64 is auto-allocated by connect_router_downlink since region router is DualStack.
            let region_bridge_cidr = net4(198, 18, idx * 16, 0, 24);
            let sub_switch =
                inner.add_switch(&format!("{region_router_name}-sub"), None, None, None, None);
            inner.connect_router_downlink(id, sub_switch, Some(region_bridge_cidr))?;

            // Set next_host to 10 so sub-routers get .10, .11, ...
            if let Some(sw) = inner.switch_mut(sub_switch) {
                sw.next_host = 10;
                sw.next_host_v6 = 10;
            }

            // IX uplink: region router gets an IX IP (v4 + v6).
            let ix_ip = inner.alloc_ix_ip_low()?;
            let ix_ip_v6 = inner.alloc_ix_ip_v6_low()?;
            let ix_sw = inner.ix_sw();
            inner.connect_router_uplink(id, ix_sw, Some(ix_ip), Some(ix_ip_v6))?;

            // Store region info.
            inner.regions.insert(
                Arc::<str>::from(name),
                core::RegionInfo {
                    idx,
                    router_id: id,
                    next_downstream: 1,
                },
            );

            // Extract snapshot for async setup.
            let router = inner.router(id).unwrap().clone();
            let cfg = &inner.cfg;
            let ix_sw_id = inner.ix_sw();

            // Region router has a return route for its bridge /24 via its IX IP.
            // But it also needs the /20 aggregate in root NS.
            // The per-/24 return route for the bridge subnet is handled by the
            // standard return_route mechanism.
            let return_route =
                if let (Some(cidr), Some(via)) = (router.downstream_cidr, router.upstream_ip) {
                    Some((cidr.addr(), cidr.prefix_len(), via))
                } else {
                    None
                };

            let downlink_bridge = router.downlink.and_then(|sw_id| {
                let sw = inner.switch(sw_id)?;
                let br = sw.bridge.clone().unwrap_or_else(|| "br-lan".into());
                let v4 = sw.gw.and_then(|gw| Some((gw, sw.cidr?.prefix_len())));
                Some((br, v4))
            });
            let downlink_bridge_v6 = router.downlink.and_then(|sw_id| {
                let sw = inner.switch(sw_id)?;
                Some((sw.gw_v6?, sw.cidr_v6?.prefix_len()))
            });
            let ra_enabled = router.cfg.ra_enabled;

            let setup_data = RouterSetupData {
                router,
                root_ns: cfg.root_ns.clone(),
                prefix: cfg.prefix.clone(),
                ix_sw: ix_sw_id,
                ix_br: cfg.ix_br.clone(),
                ix_gw: cfg.ix_gw,
                ix_cidr_prefix: cfg.ix_cidr.prefix_len(),
                upstream_owner_ns: None,
                upstream_bridge: None,
                upstream_gw: None,
                upstream_cidr_prefix: None,
                return_route,
                downlink_bridge,
                ix_gw_v6: Some(cfg.ix_gw_v6),
                ix_cidr_v6_prefix: Some(cfg.ix_cidr_v6.prefix_len()),
                upstream_gw_v6: None,
                upstream_cidr_prefix_v6: None,
                return_route_v6: None,
                downlink_bridge_v6,
                parent_route_v6: None,
                parent_route_v4: None,
                cancel: self.inner.cancel.clone(),
                dad_mode: self.inner.ipv6_dad_mode,
                provisioning_mode: self.inner.ipv6_provisioning_mode,
                ra_enabled,
            };

            (id, setup_data, idx)
        }; // lock released

        // Phase 2: Async network setup (no lock held).
        let netns = &self.inner.netns;
        setup_router_async(netns, &setup_data).await?;

        // Phase 3: Add /20 aggregate route in root NS for the region (v4 + v6).
        let region_net = region_base(idx);
        let via = setup_data
            .router
            .upstream_ip
            .context("region router has no IX IP")?;
        let via_v6 = setup_data.router.upstream_ip_v6;
        let downstream_cidr_v6 = setup_data.router.downstream_cidr_v6;
        let root_ns = setup_data.root_ns.clone();
        wiring::nl_run(netns, &root_ns, move |h: Netlink| async move {
            h.add_route_v4(region_net, 20, via).await.ok();
            if let (Some(via6), Some(cidr6)) = (via_v6, downstream_cidr_v6) {
                h.add_route_v6(cidr6.addr(), cidr6.prefix_len(), via6)
                    .await
                    .ok();
            }
            Ok(())
        })
        .await?;

        self.inner.emit(LabEventKind::RegionAdded {
            name: name.to_string(),
            router: region_router_name,
        });

        Ok(Region {
            name: Arc::from(name),
            idx,
            router_id: id,
        })
    }

    /// Links two regions with a veth pair and applies netem impairment.
    ///
    /// Creates a point-to-point veth between the two region router namespaces,
    /// assigns /30 addresses from 203.0.113.0/24, applies tc netem on both ends,
    /// and adds /20 routes so each region can reach the other.
    pub async fn link_regions(&self, a: &Region, b: &Region, link: RegionLink) -> Result<()> {
        let s = self
            .inner
            .core
            .lock()
            .unwrap()
            .prepare_link_regions(&a.name, &b.name)?;

        let netns = &self.inner.netns;
        let veth_a = format!("vr-{}-{}", a.name, b.name);
        let veth_b = format!("vr-{}-{}", b.name, a.name);

        // Create veth pair in root NS, then move ends to region router NSes.
        let veth_a2 = veth_a.clone();
        let veth_b2 = veth_b.clone();
        let a_ns_fd = netns.ns_fd(&s.a.ns)?;
        let b_ns_fd = netns.ns_fd(&s.b.ns)?;
        wiring::nl_run(netns, &s.root_ns, move |h: Netlink| async move {
            h.ensure_link_deleted(&veth_a2).await.ok();
            h.add_veth(&veth_a2, &veth_b2).await?;
            h.move_link_to_netns(&veth_a2, &a_ns_fd).await?;
            h.move_link_to_netns(&veth_b2, &b_ns_fd).await?;
            Ok(())
        })
        .await?;

        // Copy out IP fields used by both closures.
        let (a_ip, a_ip6) = (s.a.ip, s.a.ip6);
        let (b_ip, b_ip6) = (s.b.ip, s.b.ip6);

        // Configure side A: assign IP, bring up, add route to B's /20.
        let veth_a3 = veth_a.clone();
        let b_region_net = region_base(s.b.idx);
        let b_sub_v6 = s.b.sub_v6;
        wiring::nl_run(netns, &s.a.ns, move |h: Netlink| async move {
            h.add_addr4(&veth_a3, a_ip, 30).await?;
            h.add_addr6(&veth_a3, a_ip6, 126).await?;
            h.set_link_up(&veth_a3).await?;
            h.add_route_v4(b_region_net, 20, b_ip).await?;
            if let Some(v6) = b_sub_v6 {
                h.add_route_v6(v6.addr(), v6.prefix_len(), b_ip6).await?;
            }
            Ok(())
        })
        .await?;

        // Configure side B: assign IP, bring up, add route to A's /20.
        let veth_b3 = veth_b.clone();
        let a_region_net = region_base(s.a.idx);
        let a_sub_v6 = s.a.sub_v6;
        wiring::nl_run(netns, &s.b.ns, move |h: Netlink| async move {
            h.add_addr4(&veth_b3, b_ip, 30).await?;
            h.add_addr6(&veth_b3, b_ip6, 126).await?;
            h.set_link_up(&veth_b3).await?;
            h.add_route_v4(a_region_net, 20, a_ip).await?;
            if let Some(v6) = a_sub_v6 {
                h.add_route_v6(v6.addr(), v6.prefix_len(), a_ip6).await?;
            }
            Ok(())
        })
        .await?;

        // Emit RegionLinkAdded event.
        {
            let inner = self.inner.core.lock().unwrap();
            let ra = inner
                .router(a.router_id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            let rb = inner
                .router(b.router_id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            drop(inner);
            self.inner.emit(LabEventKind::RegionLinkAdded {
                router_a: ra,
                router_b: rb,
            });
        }

        // Apply netem impairment on both veth ends.
        if link.latency_ms > 0 || link.jitter_ms > 0 || link.loss_pct > 0.0 {
            let limits = LinkLimits {
                latency_ms: link.latency_ms,
                jitter_ms: link.jitter_ms,
                loss_pct: link.loss_pct as f32,
                rate_kbit: if link.rate_mbit > 0 {
                    link.rate_mbit * 1000
                } else {
                    0
                },
                ..Default::default()
            };
            let veth_a4 = veth_a.clone();
            let limits_a = limits;
            let rt_a = netns.rt_handle_for(&s.a.ns)?;
            rt_a.spawn(async move { crate::qdisc::apply_impair(&veth_a4, limits_a).await })
                .await
                .context("tc impair task panicked")??;
            let veth_b4 = veth_b.clone();
            let rt_b = netns.rt_handle_for(&s.b.ns)?;
            rt_b.spawn(async move { crate::qdisc::apply_impair(&veth_b4, limits).await })
                .await
                .context("tc impair task panicked")??;
        }

        Ok(())
    }

    /// Breaks the direct link between two regions, rerouting through an intermediate.
    ///
    /// Finds a third region `m` that has non-broken links to both `a` and `b`,
    /// and replaces the direct routes with routes through `m`. Traffic will
    /// traverse two inter-region hops instead of one.
    pub async fn break_region_link(&self, a: &Region, b: &Region) -> Result<()> {
        let s = self
            .inner
            .core
            .lock()
            .unwrap()
            .prepare_break_region_link(&a.name, &b.name)?;

        let netns = &self.inner.netns;

        // On region_a: replace route to b's /20 via m (on a↔m veth)
        let b_net = region_base(b.idx);
        let a_via = s.m_ip_on_ma;
        wiring::nl_run(netns, &s.a_ns, move |nl| async move {
            nl.replace_route_v4(b_net, 20, a_via).await
        })
        .await?;

        // On region_b: replace route to a's /20 via m (on b↔m veth)
        let a_net = region_base(a.idx);
        let b_via = s.m_ip_on_mb;
        wiring::nl_run(netns, &s.b_ns, move |nl| async move {
            nl.replace_route_v4(a_net, 20, b_via).await
        })
        .await?;

        // Mark link as broken.
        self.inner
            .core
            .lock()
            .unwrap()
            .set_region_link_broken(&s.link_key, true);

        // Emit event.
        {
            let inner = self.inner.core.lock().unwrap();
            let ra = inner
                .router(a.router_id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            let rb = inner
                .router(b.router_id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            drop(inner);
            self.inner.emit(LabEventKind::RegionLinkBroken {
                router_a: ra,
                router_b: rb,
                condition: None,
            });
        }

        Ok(())
    }

    /// Restores a previously broken direct link between two regions.
    ///
    /// Reverses [`break_region_link`](Self::break_region_link): replaces the
    /// indirect route through the intermediate region with the original direct
    /// veth route.
    ///
    /// # Errors
    ///
    /// Returns an error if the link is not currently broken or if the regions
    /// are not connected.
    pub async fn restore_region_link(&self, a: &Region, b: &Region) -> Result<()> {
        let s = self
            .inner
            .core
            .lock()
            .unwrap()
            .prepare_restore_region_link(&a.name, &b.name)?;

        let netns = &self.inner.netns;

        // Direct route on a: b's /20 via b's IP on the a↔b veth.
        let b_net = region_base(b.idx);
        let b_direct_ip = s.b_direct_ip;
        wiring::nl_run(netns, &s.a_ns, move |nl| async move {
            nl.replace_route_v4(b_net, 20, b_direct_ip).await
        })
        .await?;

        // Direct route on b: a's /20 via a's IP on the a↔b veth.
        let a_net = region_base(a.idx);
        let a_direct_ip = s.a_direct_ip;
        wiring::nl_run(netns, &s.b_ns, move |nl| async move {
            nl.replace_route_v4(a_net, 20, a_direct_ip).await
        })
        .await?;

        // Mark link as restored.
        self.inner
            .core
            .lock()
            .unwrap()
            .set_region_link_broken(&s.link_key, false);

        // Emit event.
        {
            let inner = self.inner.core.lock().unwrap();
            let ra = inner
                .router(a.router_id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            let rb = inner
                .router(b.router_id)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            drop(inner);
            self.inner.emit(LabEventKind::RegionLinkRestored {
                router_a: ra,
                router_b: rb,
            });
        }

        Ok(())
    }

    /// Creates three default regions (us, eu, asia) with typical one-way latencies.
    ///
    /// One-way latencies (RTT = 2×):
    /// - us↔eu: 40ms (RTT ~80ms, real-world 70–100ms)
    /// - us↔asia: 95ms (RTT ~190ms, real-world 170–220ms US East↔East Asia)
    /// - eu↔asia: 120ms (RTT ~240ms, real-world 210–250ms EU↔East Asia)
    pub async fn add_default_regions(&self) -> Result<DefaultRegions> {
        let us = self.add_region("us").await?;
        let eu = self.add_region("eu").await?;
        let asia = self.add_region("asia").await?;
        self.link_regions(&us, &eu, RegionLink::good(40)).await?;
        self.link_regions(&us, &asia, RegionLink::good(95)).await?;
        self.link_regions(&eu, &asia, RegionLink::good(120)).await?;
        Ok(DefaultRegions { us, eu, asia })
    }

    /// Builds a map of `NETSIM_*` environment variables from the current lab state.
    ///
    /// Keys follow the pattern `NETSIM_IP_{DEVICE}` for the default interface
    /// and `NETSIM_IP_{DEVICE}_{IFACE}` for all interfaces. Names are
    /// uppercased with hyphens replaced by underscores.
    pub fn env_vars(&self) -> HashMap<String, String> {
        let inner = self.inner.core.lock().unwrap();
        let mut map = HashMap::new();
        for dev in inner.all_devices() {
            let norm = normalize_env_name(&dev.name);
            if let Some(ip) = dev.default_iface().ip {
                map.insert(format!("NETSIM_IP_{}", norm), ip.to_string());
            }
            for iface in &dev.interfaces {
                if let Some(ip) = iface.ip {
                    let ifnorm = normalize_env_name(&iface.ifname);
                    map.insert(format!("NETSIM_IP_{}_{}", norm, ifnorm), ip.to_string());
                }
            }
        }
        map
    }

    /// Returns a handle to the IX (Internet Exchange) root namespace.
    pub fn ix(&self) -> Ix {
        Ix::new(Arc::clone(&self.inner.0))
    }

    /// Safety-net cleanup: drops fd-registry entries for this lab's prefix.
    /// Normal cleanup happens in `NetworkCore::drop`.
    pub fn cleanup(&self) {
        let prefix = self.inner.core.lock().unwrap().cfg.prefix.clone();
        self.inner.netns.cleanup_prefix(&prefix);
    }

    // ── DNS entries ───────────────────────────────────────────────────────

    /// Returns the lab's DNS server, starting it on first call.
    ///
    /// The server binds to the IX bridge IP on port 53 inside the root
    /// namespace. Once started, all devices' `resolv.conf` overlay is updated
    /// to point at it (glibc picks up the change on the next `getaddrinfo()`).
    ///
    /// Use [`DnsServer::set_host`] and [`DnsServer::set_txt`] to add records.
    /// Records are immediately visible to DNS queries — no propagation delay.
    pub fn dns_server(&self) -> Result<crate::dns_server::DnsServer> {
        let mut guard = self.inner.dns_server.lock().unwrap();
        if let Some(ref server) = *guard {
            return Ok(server.clone());
        }
        let (root_ns, ix_gw, ix_gw_v6) = {
            let core = self.inner.core.lock().unwrap();
            (core.cfg.root_ns.clone(), core.cfg.ix_gw, core.cfg.ix_gw_v6)
        };
        let server = crate::dns_server::DnsServer::start(&self.inner.netns, &root_ns)?;
        // Point all devices' resolv.conf at the DNS server (v4 + v6).
        {
            let mut core = self.inner.core.lock().unwrap();
            core.dns.nameservers = vec![ix_gw.into(), ix_gw_v6.into()];
            core.dns.write_resolv_conf()?;
        }
        *guard = Some(server.clone());
        Ok(server)
    }

    /// Resolves a name via the DNS server (if started), or returns `None`.
    pub fn resolve(&self, name: &str) -> Option<IpAddr> {
        self.inner
            .dns_server
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|dns| dns.resolve(name))
    }

    // ── Dynamic operations ────────────────────────────────────────────────

    /// Applies or removes impairment on the link between two directly connected nodes.
    ///
    /// For **Device ↔ Router**: applies impairment on the device's interface in the
    /// device namespace (affecting both upload and download on that link).
    ///
    /// For **Router ↔ Router**: applies impairment on the downstream router's WAN
    /// interface (either "ix" for IX-connected or "wan" for sub-routers).
    ///
    /// This applies impairment to the resolved interface between the two nodes
    /// (netem on the egress qdisc). For **bidirectional** impairment between
    /// two communicating devices, impair each device's link to its router
    /// separately:
    ///
    /// ```ignore
    /// lab.set_link_condition(dev_a.id(), router.id(), Some(cond)).await?;
    /// lab.set_link_condition(dev_b.id(), router.id(), Some(cond)).await?;
    /// ```
    ///
    /// For per-interface directional control (egress vs ingress on a single
    /// device interface), use [`Iface::set_condition`] instead.
    pub async fn set_link_condition(
        &self,
        a: NodeId,
        b: NodeId,
        impair: Option<LinkCondition>,
    ) -> Result<()> {
        debug!(a = ?a, b = ?b, impair = ?impair, "lab: set_link_condition");
        let (ns, ifname) = self.inner.core.lock().unwrap().resolve_link_target(a, b)?;
        apply_or_remove_impair(&self.inner.netns, &ns, &ifname, impair).await;
        Ok(())
    }

    // ── Lookup helpers ───────────────────────────────────────────────────

    /// Returns a device handle by id, or `None` if the id is not a device.
    pub fn device(&self, id: NodeId) -> Option<Device> {
        let inner = self.inner.core.lock().unwrap();
        let d = inner.device(id)?;
        Some(Device::new(
            id,
            d.name.clone(),
            d.ns.clone(),
            Arc::clone(&self.inner.0),
        ))
    }

    /// Returns a router handle by id, or `None` if the id is not a router.
    pub fn router(&self, id: NodeId) -> Option<Router> {
        let inner = self.inner.core.lock().unwrap();
        let r = inner.router(id)?;
        Some(Router::new(
            id,
            r.name.clone(),
            r.ns.clone(),
            Arc::clone(&self.inner.0),
        ))
    }

    /// Looks up a device by name and returns a handle.
    pub fn device_by_name(&self, name: &str) -> Option<Device> {
        let inner = self.inner.core.lock().unwrap();
        let id = inner.device_id_by_name(name)?;
        let d = inner.device(id)?;
        Some(Device::new(
            id,
            d.name.clone(),
            d.ns.clone(),
            Arc::clone(&self.inner.0),
        ))
    }

    /// Looks up a router by name and returns a handle.
    pub fn router_by_name(&self, name: &str) -> Option<Router> {
        let inner = self.inner.core.lock().unwrap();
        let id = inner.router_id_by_name(name)?;
        let r = inner.router(id)?;
        Some(Router::new(
            id,
            r.name.clone(),
            r.ns.clone(),
            Arc::clone(&self.inner.0),
        ))
    }

    /// Returns handles for all devices.
    pub fn devices(&self) -> Vec<Device> {
        let inner = self.inner.core.lock().unwrap();
        inner
            .all_device_ids()
            .into_iter()
            .filter_map(|id| {
                let d = inner.device(id)?;
                Some(Device::new(
                    id,
                    d.name.clone(),
                    d.ns.clone(),
                    Arc::clone(&self.inner.0),
                ))
            })
            .collect()
    }

    /// Returns handles for all routers.
    pub fn routers(&self) -> Vec<Router> {
        let inner = self.inner.core.lock().unwrap();
        inner
            .all_router_ids()
            .into_iter()
            .filter_map(|id| {
                let r = inner.router(id)?;
                Some(Router::new(
                    id,
                    r.name.clone(),
                    r.ns.clone(),
                    Arc::clone(&self.inner.0),
                ))
            })
            .collect()
    }
}

// ─────────────────────────────────────────────
// Ix handle
// ─────────────────────────────────────────────

/// Handle to the Internet Exchange — the lab's root namespace that hosts
/// the shared bridge connecting all IX-level routers.
///
/// Same pattern as [`Device`] and [`Router`]: holds an `Arc` to the lab
/// interior. All accessor methods briefly lock the mutex.
pub struct Ix {
    lab: Arc<LabInner>,
}

impl Clone for Ix {
    fn clone(&self) -> Self {
        Self {
            lab: Arc::clone(&self.lab),
        }
    }
}

impl std::fmt::Debug for Ix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ix").finish()
    }
}

impl Ix {
    pub(crate) fn new(lab: Arc<LabInner>) -> Self {
        Self { lab }
    }

    /// Returns the root namespace name.
    pub fn ns(&self) -> String {
        self.lab.core.lock().unwrap().root_ns().to_string()
    }

    /// Returns the IX gateway IPv4 address (e.g. 203.0.113.1).
    pub fn gw(&self) -> Ipv4Addr {
        self.lab.core.lock().unwrap().ix_gw()
    }

    /// Returns the IX gateway IPv6 address (e.g. 2001:db8::1).
    pub fn gw_v6(&self) -> Ipv6Addr {
        self.lab.core.lock().unwrap().cfg.ix_gw_v6
    }

    /// Spawns an async task on the IX root namespace's tokio runtime.
    ///
    /// The closure receives a cloned [`Ix`] handle.
    pub fn spawn<F, Fut, T>(&self, f: F) -> tokio::task::JoinHandle<T>
    where
        F: FnOnce(Ix) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let ns = self.lab.core.lock().unwrap().root_ns().to_string();
        let rt = self
            .lab
            .rt_handle_for(&ns)
            .expect("root namespace has async worker");
        let handle = self.clone();
        rt.spawn(f(handle))
    }

    /// Runs a short-lived sync closure in the IX root namespace.
    ///
    /// Blocks the caller until the closure returns.
    pub fn run_sync<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let ns = self.lab.core.lock().unwrap().root_ns().to_string();
        self.lab.netns.run_closure_in(&ns, f)
    }

    /// Spawns a dedicated OS thread in the IX root namespace.
    pub fn spawn_thread<F, R>(&self, f: F) -> Result<thread::JoinHandle<Result<R>>>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let ns = self.lab.core.lock().unwrap().root_ns().to_string();
        self.lab.netns.spawn_thread_in(&ns, f)
    }

    /// Spawns a [`tokio::process::Command`] in the IX root namespace.
    pub fn spawn_command(&self, mut cmd: tokio::process::Command) -> Result<tokio::process::Child> {
        let ns = self.lab.core.lock().unwrap().root_ns().to_string();
        let rt = self.lab.rt_handle_for(&ns)?;
        self.lab.netns.run_closure_in(&ns, move || {
            let _guard = rt.enter();
            cmd.spawn().context("spawn async command in namespace")
        })
    }

    /// Spawns a [`std::process::Command`] in the IX root namespace.
    pub fn spawn_command_sync(&self, mut cmd: Command) -> Result<std::process::Child> {
        let ns = self.lab.core.lock().unwrap().root_ns().to_string();
        self.lab.netns.run_closure_in(&ns, move || {
            cmd.spawn().context("spawn command in namespace")
        })
    }

    /// Spawns a STUN-like UDP reflector in the IX root namespace.
    ///
    /// See [`Device::spawn_reflector`] for details.
    pub async fn spawn_reflector(&self, bind: SocketAddr) -> Result<core::ReflectorGuard> {
        let ns = self.lab.core.lock().unwrap().root_ns().to_string();
        self.lab.spawn_reflector_in(&ns, bind).await
    }
}

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

/// Normalizes a device/interface name for use in an environment variable name.
pub(crate) fn normalize_env_name(s: &str) -> String {
    s.to_uppercase().replace('-', "_")
}

// Everything below (RouterPreset, RouterBuilder, DeviceBuilder) has moved to
// router.rs and device.rs.  The rest of this file is TestGuard.

/// RAII guard that records test pass/fail into the lab's state.json.
///
/// Created by [`Lab::test_guard`]. Defaults to "failed" on drop unless
/// [`.ok()`](TestGuard::ok) was called. This means the only way to get
/// "success" is to explicitly call `.ok()`, avoiding false positives from
/// early `?` returns or panics.
///
/// Both `.ok()` and the failure path emit a [`LabEventKind::TestCompleted`]
/// event so the result is visible in the timeline.
pub struct TestGuard {
    inner: Arc<LabInner>,
    marked: bool,
}

impl TestGuard {
    /// Mark the test as successful.
    ///
    /// Call this at the end of a passing test, typically just before `Ok(())`.
    pub fn ok(mut self) {
        use std::sync::atomic::Ordering;
        self.inner
            .test_status
            .store(crate::writer::STATUS_SUCCESS, Ordering::Release);
        self.inner
            .emit(LabEventKind::TestCompleted { passed: true });
        self.marked = true;
    }
}

impl Drop for TestGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        if !self.marked {
            self.inner
                .test_status
                .store(crate::writer::STATUS_FAILED, Ordering::Release);
            self.inner
                .emit(LabEventKind::TestCompleted { passed: false });
        }
    }
}
