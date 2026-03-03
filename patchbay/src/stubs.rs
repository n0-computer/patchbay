//! Stub types for non-Linux platforms.
//!
//! These types allow patchbay-server and patchbay-vm to compile on macOS/Windows
//! without pulling in Linux-specific dependencies. The actual network simulation
//! only works on Linux.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::event::LabEvent;

/// Unique identifier for a node (router or device) in the lab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub(crate) u64);

/// Stub Lab type for non-Linux platforms.
///
/// This type exists only to satisfy the compiler. Creating a Lab on non-Linux
/// platforms will panic.
#[derive(Clone)]
pub struct Lab {
    run_dir: Option<PathBuf>,
    events_tx: broadcast::Sender<LabEvent>,
}

impl Lab {
    /// Returns the run output directory, if set.
    pub fn run_dir(&self) -> Option<&Path> {
        self.run_dir.as_deref()
    }

    /// Subscribe to lab events.
    pub fn subscribe(&self) -> broadcast::Receiver<LabEvent> {
        self.events_tx.subscribe()
    }
}

/// Stub Router type for non-Linux platforms.
#[derive(Clone)]
pub struct Router {
    _private: (),
}

impl Router {
    /// Returns this router's node ID.
    pub fn id(&self) -> NodeId {
        unimplemented!("Router not supported on this platform")
    }
}

/// Stub Device type for non-Linux platforms.
#[derive(Clone)]
pub struct Device {
    _private: (),
}

/// Stub Ix (Internet Exchange) type for non-Linux platforms.
#[derive(Clone)]
pub struct Ix {
    _private: (),
}

/// Stub Region type for non-Linux platforms.
#[derive(Clone)]
pub struct Region {
    _private: (),
}

/// Stub DeviceIface type for non-Linux platforms.
#[derive(Clone)]
pub struct DeviceIface {
    _private: (),
}

/// Stub ObservedAddr type for non-Linux platforms.
#[derive(Clone, Debug)]
pub struct ObservedAddr {
    _private: (),
}

/// Stub RegionLink for non-Linux platforms.
#[derive(Clone, Debug)]
pub struct RegionLink {
    _private: (),
}

impl RegionLink {
    /// Creates a "good" region link with the given latency.
    pub fn good(_latency_ms: u32) -> Self {
        Self { _private: () }
    }
}

/// Stub DeviceBuilder for non-Linux platforms.
pub struct DeviceBuilder {
    _private: (),
}

/// Stub RouterBuilder for non-Linux platforms.
pub struct RouterBuilder {
    _private: (),
}

/// Stub LabOpts for non-Linux platforms.
#[derive(Clone, Debug, Default)]
pub struct LabOpts {
    /// Optional human-readable label for the lab.
    pub label: Option<String>,
    /// Output directory for events and state files.
    pub outdir: Option<PathBuf>,
}

/// Stub DefaultRegions for non-Linux platforms.
pub struct DefaultRegions {
    _private: (),
}

/// Bootstraps into a user namespace with networking capabilities.
///
/// On non-Linux platforms, this is a no-op.
pub fn init_userns() -> anyhow::Result<()> {
    Ok(())
}

/// Variant of [`init_userns`] for use with the `ctor` crate.
///
/// On non-Linux platforms, this is a no-op.
pub fn init_userns_for_ctor() {
    // no-op on non-Linux
}

/// Verifies the process has enough privileges to manage namespaces.
///
/// On non-Linux platforms, always returns an error.
pub fn check_caps() -> anyhow::Result<()> {
    anyhow::bail!("patchbay requires Linux")
}
