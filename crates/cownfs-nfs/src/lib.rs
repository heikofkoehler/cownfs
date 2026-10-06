//! Hand-rolled ONC RPC / XDR / NFSv4.0 server (P4: read-only subset).
//!
//! No external crates: the wire format is implemented from scratch so the
//! protocol surface stays minimal and auditable.

pub mod backup_parse;
pub mod layouts;
pub mod log;
pub mod metrics;
pub mod nfs4;
pub mod referrals;
pub mod rpc;
pub mod server;
pub mod sessions;
pub mod snapshot_sched;
pub mod state;
pub mod state_log;
pub mod throttle;
pub mod xdr;
