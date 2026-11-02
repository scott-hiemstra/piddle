//! PIDdle terminal front end.

mod app;
mod ui;

use std::io::IsTerminal;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use piddle_core::sampler::{self, Control};
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::{App, INTERVALS};

const HELP: &str = "\
piddle-tui: visual process viewer for the terminal

Usage: piddle-tui [--interval MS]

Options:
  -i, --interval MS   sampling interval in milliseconds (250, 500, 1000, 2000)
  -h, --help          show this help
  -V, --version       show the version

Inside the program, press ? for keys.";

fn main() -> std::io::Result<()> {
    let mut interval = 1000;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("piddle-tui {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-i" | "--interval" => {
                interval = args.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                if !INTERVALS.contains(&interval) {
                    eprintln!("piddle-tui: --interval must be one of 250, 500, 1000, 2000");
                    std::process::exit(2);
                }
            }
            other => {
                eprintln!("piddle-tui: unknown argument '{other}'\n\n{HELP}");
                std::process::exit(2);
            }
        }
    }
    if let Err(why) = sampler::check_proc() {
        eprintln!("piddle-tui: {why}");
        std::process::exit(1);
    }
    if !std::io::stdout().is_terminal() {
        eprintln!("piddle-tui: needs an interactive terminal");
        std::process::exit(2);
    }

    let control = Control::new(interval);
    control.collect_io.store(true, Ordering::Relaxed);
    let rx = sampler::spawn(control.clone());
    let mut app = App::new(control.clone());
    let palette = ui::Palette::detect();

    // ratatui::init installs a panic hook that restores the terminal.
    let mut terminal = ratatui::init();
    let result = (|| -> std::io::Result<()> {
        let mut dirty = true;
        let mut last_status = None;
        loop {
            while let Ok(snap) = rx.try_recv() {
                app.apply(snap);
                dirty = true;
            }
            dirty |= app.poll_logs();
            // Repaint once more when a status message expires.
            let status_live = app.status.as_ref().map(|(_, at)| at.elapsed() < Duration::from_secs(4));
            if status_live != last_status {
                last_status = status_live;
                dirty = true;
            }
            if dirty {
                terminal.draw(|f| ui::draw(f, &mut app, &palette))?;
                dirty = false;
            }
            let deadline = Instant::now() + Duration::from_millis(100);
            while event::poll(deadline.saturating_duration_since(Instant::now()))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                    Event::Resize(..) => {}
                    _ => continue,
                }
                dirty = true;
                if app.quit {
                    return Ok(());
                }
            }
        }
    })();
    ratatui::restore();
    control.paused.store(true, Ordering::Relaxed);
    result
}
