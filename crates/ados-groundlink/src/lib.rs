//! Ground-station data-plane: WFB receive, channel acquisition, video fan-out,
//! and the self-healing mesh. Modules are added incrementally; this is the
//! crate root.
//!
//! The radio adapter lifecycle and the FHSS/TX-liveness machinery live in the
//! sibling `ados-radio` crate; this crate owns the receive-side glue: the video
//! UDP fan-out, the Contract-E sidecar files, and (in a later chunk) the
//! channel-acquisition receive loop and the mesh role manager.

pub mod fanout;
pub mod paths;
pub mod sidecars;

pub use fanout::{run_default_fanout, run_fanout};
pub use sidecars::write_json_atomic;
