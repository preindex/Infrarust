//! Docker Swarm provider.
//!
//! Scales one replicated, singleton service between 0 and 1 replica through the
//! manager node's typed Docker API. The service is addressed by its immutable ID,
//! and readiness is probed on the service VIP, never on a task IP.

mod ping;
mod swarm;

pub use swarm::{SCALE_OPT_IN_LABEL, SwarmServerProvider};
