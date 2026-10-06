//! Resource control for Agent-Interaction workloads: explicit domains, a policy that bounds each of them, and the
//! lifecycle (freeze, stop, kill, remove, recover) of what runs in them. See ../README.md.

pub mod domain;
pub mod fs;
pub mod layout;
pub mod policy;
