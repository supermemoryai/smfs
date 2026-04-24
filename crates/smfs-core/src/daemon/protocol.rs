//! IPC wire protocol — JSON line-based.
//!
//! One request per connection. Simple enough to drive with `nc` for
//! debugging:
//!
//! ```text
//! $ echo '{"cmd":"ping"}' | nc -U <sockets/tag.sock>
//! {"type":"pong"}
//! ```

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    Sync,
    Unmount,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Status {
        tag: String,
        mount_path: String,
        pid: u32,
        uptime_secs: u64,
        queue_len: usize,
        pull_enabled: bool,
        #[serde(default)]
        user_id: Option<String>,
        #[serde(default)]
        user_name: Option<String>,
        #[serde(default)]
        org_name: Option<String>,
    },
    SyncDone {
        pulled: usize,
        pushed_pending: usize,
    },
    UnmountAck,
    Error {
        message: String,
    },
}
