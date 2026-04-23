//! Server-side protocol wire formats.
//!
//! The `portmapper` crate implements the client side of NAT-PMP, PCP, and
//! UPnP IGD, but keeps its protocol types private. This module mirrors just
//! enough of that wire format on the server side for the patchbay router to
//! decode client requests and encode replies. Protocol-specific submodules
//! land alongside their server implementations (NAT-PMP in Step 3, PCP in
//! Step 4, UPnP in Step 5).
