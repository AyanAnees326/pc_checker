//! PawnIO integration — the successor to WinRing0 for ring-0 hardware access.
//!
//! WinRing0 (used by essentially every hardware monitor before this) carries
//! CVE-2020-14979 and is on Microsoft's vulnerable-driver blocklist, enforced by
//! default since the Windows 11 2022 Update. PawnIO replaces it: a signed driver
//! that runs small, auditable Pawn bytecode modules instead of exposing raw ring-0
//! primitives to userspace. See `bootstrap` for how this app detects it and
//! `intel_msr`/`amd_msr` for what each vendor's module actually permits — which,
//! importantly, turned out narrower than the original research assumed (see
//! `intel_msr`'s module doc comment).

pub mod amd_msr;
pub mod bootstrap;
pub mod ffi;
pub mod intel_msr;

pub use bootstrap::{status, PawnIoStatus};
pub use ffi::PawnIoLib;
