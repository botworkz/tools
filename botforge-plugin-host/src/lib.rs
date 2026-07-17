//! Plugin host for the botforge `.so` plugin system.
//!
//! # Safety policy
//!
//! This crate is the **sole sanctioned location** in the botforge workspace
//! where `unsafe` code is permitted.  All other workspace members
//! (`shasset`, `botforge`, `viscous`) declare `unsafe_code = "forbid"` in
//! their own `Cargo.toml`; this crate intentionally omits that gate.
//!
//! ## Why `unsafe` is allowed here
//!
//! Loading a native shared library (`.so`) and calling into it requires
//! `unsafe` Rust: the linker cannot verify that a dynamically-loaded symbol
//! has the expected type or calling convention, and the host must manage
//! raw pointers across the FFI boundary.  There is no safe abstraction that
//! removes this inherent unsafety — it must exist somewhere.
//!
//! ## Blast-radius containment
//!
//! Concentrating all FFI/loading `unsafe` in this one crate means:
//!
//! - Every other crate in the workspace remains entirely `unsafe`-free and
//!   can be audited without understanding FFI.
//! - The surface area for memory-safety review is bounded: only this crate
//!   needs the extra scrutiny that `unsafe` demands.
//! - Any future `unsafe` block that appears outside this crate is a policy
//!   violation, caught at compile time by those crates' `forbid` gates.
//!
//! ## What belongs here
//!
//! - Dynamic library discovery, opening, and symbol resolution.
//! - ABI version negotiation with loaded plugins.
//! - Any raw-pointer or FFI type conversions required at the plugin
//!   boundary.
//!
//! ## What does NOT belong here
//!
//! This crate is a policy boundary, not a dumping ground.  Business logic,
//! compressor implementations, and plan execution live in `botforge`.  This
//! crate only mediates the host↔plugin handshake.
//!
//! ## Status
//!
//! **Stub only.**  No `.so`/FFI/`libloading`/`dlopen` logic is implemented
//! yet.  This crate exists to establish the workspace policy (permitting
//! `unsafe` here and nowhere else) and to unblock downstream plugin work.
//! Functional plugin-loading code will be added in a follow-up PR under
//! tracking issue #432.
