//! UI-independent core of PIDdle.
//!
//! Nothing in this crate depends on a toolkit, so any front end (the GTK4
//! GUI today, a Ratatui TUI later) can share the same sampling, visual
//! encodings and process actions.
//!
//! * [`sampler`] — low-overhead `/proc` sampling, either driven directly via
//!   [`sampler::Sampler`] or on a background thread via [`sampler::spawn`].
//! * [`encode`] — CPU → color and memory → size mappings plus formatting
//!   (lives in `piddle-encode` from piddle-common, re-exported here).
//! * [`actions`] — signalling processes safely, state descriptions.
//! * [`logs`] — journal entries for a process's service, via journalctl.
//! * [`net`] — a process's network connections, looked up on demand.
//! * [`service`] — which systemd service, scope or container a process is in.
//! * [`containers`] — container names, Compose projects and pods, from the
//!   runtimes' read-only APIs, and the containers picker.
//! * [`GUI_UNAVAILABLE`] — exit code contract between `piddle-gui` and the
//!   `piddle` launcher.

pub mod actions;
pub mod containers;
pub mod logs;
pub mod net;
pub mod sampler;
pub mod service;

/// Moved to `piddle-encode` (piddle-common) so kpiddle can share it;
/// re-exported unchanged.
pub use piddle_encode as encode;

/// Exit code `piddle-gui` uses when it cannot open a display. The `piddle`
/// launcher then falls back to the terminal UI.
pub const GUI_UNAVAILABLE: i32 = 3;
