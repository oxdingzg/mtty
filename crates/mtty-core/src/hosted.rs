//! A terminal whose PTY lives in a host process (ADR 0041), so the program
//! keeps running when the app quits and the app can attach to it again.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::aterm::ScreenState;

mod link;
pub(crate) use link::{connect_existing, start, HostLink};

/// How hosted panes are started.
#[derive(Clone, Debug)]
pub struct HostConfig {
    /// The installed host binary (see `mtty_ptyhost::launch::install`).
    pub binary: PathBuf,
    /// Output kept per pane for reattaching (ADR 0041: 8 MiB).
    pub ring: usize,
    /// How long a host waits for the app before ending its program.
    pub timeout: Duration,
}

/// What reattaching to a hosted pane needs, saved with the session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostSnapshot {
    pub id: String,
    pub socket: PathBuf,
    /// The output offset the screen state covers.
    pub offset: u64,
    /// The offset is on a sequence boundary (almost always).
    pub boundary: bool,
    /// OSC 0/2 is terminal metadata, not part of the saved cell grid. The
    /// replay begins after `offset`, so its earlier title will not be emitted.
    #[serde(default)]
    pub title: Option<String>,
    pub state: ScreenState,
}

/// What the reader thread hands the UI thread.
pub(crate) enum Incoming {
    /// Output from a local PTY or a byte pipe.
    Bytes(Vec<u8>),
    /// Host output ending at `end`.
    Hosted {
        bytes: Vec<u8>,
        end: u64,
        boundary: bool,
    },
    /// Output between the snapshot and here was lost; these are the host's
    /// modes where the replay starts.
    Truncated(Vec<u8>),
    /// Replay done: what follows is live.
    Live,
    /// The host could not be reached.
    HostFailed(String),
}
