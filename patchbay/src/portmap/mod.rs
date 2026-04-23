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
//! - [`nft`] for the dedicated `ip portmap` nftables table.
//! - [`server`] for the lifecycle handle and the shared [`server::ServerContext`].
//! - [`nat_pmp`], [`pcp`], and [`upnp`] for per-protocol decoders, encoders,
//!   and request handlers.
//!
//! Public API surface is intentionally small: [`PortmapMode`] and
//! [`PortmapConfig`] for configuration. Every internal helper is
//! `pub(crate)`.
//!
//! # Threat model
//!
//! All three protocols authorize clients by source IPv4 address. That is
//! trivially spoofable on a real LAN and acceptable only inside the
//! patchbay simulator, where the downstream bridge is populated solely by
//! tests. Do not reuse this code in a production gateway without moving
//! authorization to a stronger primitive.

pub use config::{PortmapConfig, PortmapMode};

mod config;
pub(crate) mod nat_pmp;
pub(crate) mod nft;
pub(crate) mod pcp;
pub(crate) mod registry;
pub(crate) mod server;
pub(crate) mod upnp;
