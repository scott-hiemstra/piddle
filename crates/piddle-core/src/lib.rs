//! UI-independent core of PIDdle.
//!
//! Nothing in this crate depends on a toolkit, so any front end (the GTK4
//! GUI today, a Ratatui TUI later) can share the same sampling, visual
//! encodings and process actions.
//!
//! * [`sampler`] — low-overhead `/proc` sampling, either driven directly via
//!   [`sampler::Sampler`] or on a background thread via [`sampler::spawn`].
//! * [`encode`] — CPU → color and memory → size mappings plus formatting.
//! * [`actions`] — signalling processes safely, state descriptions.

pub mod actions;
pub mod encode;
pub mod sampler;
