//! Print what the containers picker would offer right now:
//!   cargo run -p piddle-core --example containers
use piddle_core::containers::{choices, place};
use piddle_core::sampler::Sampler;

fn main() {
    let mut s = Sampler::new();
    s.watch_containers();
    s.sample();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let snap = s.sample();
    for note in snap.containers.notes() {
        println!("note: {note}");
    }
    for c in choices(&snap.procs, &snap.containers) {
        println!("{}{}  [{}]", "    ".repeat(c.depth as usize), c.label, c.procs);
    }
    if let Some(p) = snap.procs.iter().find_map(|p| place(&p.service, &snap.containers)) {
        println!("\ne.g. {}: {}", p.name, p.detail);
    }
}
