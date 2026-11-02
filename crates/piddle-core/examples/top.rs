//! Minimal text front end over piddle-core: prints the busiest processes.
//! A starting point for a Ratatui TUI, and a quick way to check sampler cost.
//!
//!     cargo run -p piddle-core --example top --release

use std::time::Duration;

use piddle_core::encode::{fmt_bytes, fmt_cpu};
use piddle_core::sampler::Sampler;

fn main() {
    let mut s = Sampler::new();
    s.sample();
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(500));
        let mut snap = s.sample();
        snap.procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        println!(
            "{} processes · system CPU {} · sample took {:.2} ms",
            snap.procs.len(),
            snap.total_cpu.map_or("—".to_string(), |c| format!("{c:.1}%")),
            snap.sample_cost.as_secs_f64() * 1e3
        );
        for p in snap.procs.iter().take(5) {
            println!(
                "  {:>7}  {:<20} {:>6}  {:>10}  {:>3} thr",
                p.pid,
                p.name,
                fmt_cpu(p.cpu as f64),
                fmt_bytes(p.rss),
                p.threads
            );
        }
    }
}
