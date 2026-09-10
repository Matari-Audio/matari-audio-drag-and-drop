//! Drop-in hosts for toolkits that own their own native event queue.
//!
//! The facade in the crate root is deliberately unopinionated: it hands a
//! toolkit a [`StartTicket`](crate::StartTicket) and expects the toolkit to
//! commit it against the right native runtime. That is the correct seam for a
//! toolkit author, but it leaves every integration re-deriving the same
//! bookkeeping — which pointer event carries authority, whether a previous
//! drag is still transferring, when the inbound drop router must be installed,
//! and which raw events belong to Matari rather than to the toolkit.
//!
//! This module owns that bookkeeping so an integration does not have to. A
//! toolkit stores one host next to its event loop, feeds it every raw event,
//! and forwards its own start requests to it.
//!
//! Hosts are a convenience, not a requirement. Every type they use is public,
//! so a toolkit with unusual lifetime or threading needs can still drive the
//! facade directly.

#[cfg(all(target_family = "unix", not(target_os = "macos")))]
mod x11;

#[cfg(all(target_family = "unix", not(target_os = "macos")))]
pub use x11::{X11Host, X11HostError};
