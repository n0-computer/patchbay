//! Port mapping server: UPnP IGD, NAT-PMP, and PCP.
//!
//! Patchbay routers can run an in-process port mapping server inside their
//! namespace that implements the three protocols common on consumer routers.
//! Devices on the downstream LAN can request external port mappings with any
//! supported protocol; granted mappings install nftables DNAT rules in a
//! dedicated `ip portmap` table so inbound WAN traffic reaches the device.
//!
//! The module is split into:
//!
//! - [`config`] for user-facing builder types.
//! - [`registry`] for the shared mapping registry and dedup logic.
//! - [`wire`] for server-side protocol encoders and decoders that match the
//!   client-side wire format implemented in `portmapper`.
//!
//! Per-protocol server tasks (`nat_pmp`, `pcp`, `upnp`) live alongside once
//! implemented. The public API surface is intentionally small: [`PortmapMode`]
//! and [`PortmapConfig`] for configuration; everything else is `pub(crate)`.

pub use config::{PortmapConfig, PortmapMode};

mod config;
#[allow(dead_code)]
pub(crate) mod nat_pmp;
#[allow(dead_code)]
pub(crate) mod nft;
#[allow(dead_code)]
pub(crate) mod pcp;
#[allow(dead_code)]
pub(crate) mod registry;
#[allow(dead_code)]
pub(crate) mod server;
