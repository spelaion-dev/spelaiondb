//! The protocol between the core and an engine plugin (ADR-041), as `docs/plugin-protocol.md`
//! describes it: frames over the plugin's standard input and output, JSON-RPC 2.0 messages for
//! control and Arrow IPC for rows.
//!
//! This crate is only the wire: the types of the messages, the frames and the Arrow encoding.
//! The plugin's side is `spelaion-plugin-sdk`, the core's side is `spelaion-plugin-host`.

mod frame;
pub mod ipc;
mod message;

pub use frame::{Frame, FrameError, MAX_FRAME, read_frame, write_frame};
pub use message::{
    CatalogRequest, Connect, ConnectResult, CreateDatabase, ErrorData, Execute, Initialize,
    InitializeResult, InsertRows, InsertRowsResult, Message, PlanRun, Program, RpcError, Run,
    SessionRequest, StreamCredit, StreamDone, StreamError, StreamId, StreamNotice, Version, code,
    method,
};

/// The version this build of the protocol speaks.
pub const VERSION: Version = Version { major: 1, minor: 0 };

/// How many record batches a plugin may have sent on a stream without credit given back, when
/// it has no reason to choose otherwise.
pub const DEFAULT_CREDIT: u32 = 8;
