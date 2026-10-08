//! Resource control for Agent-Interaction workloads: explicit domains, a policy that bounds each of them, and the
//! lifecycle (freeze, stop, kill, remove, recover) of what runs in them. See ../README.md.

#![forbid(unsafe_code)]

pub mod domain;
pub mod fs;
pub mod layout;
pub mod life;
pub mod ops;
pub mod policy;
pub mod store;
pub mod supervise;
