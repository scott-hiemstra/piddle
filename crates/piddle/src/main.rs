//! `piddle`: a tiny launcher that links no GUI libraries.
//!
//! It starts `piddle-gui` when a display is available and falls back to
//! `piddle-tui` otherwise, including when the GUI cannot start at all (GTK
//! not installed, or no reachable display, e.g. a stale DISPLAY over SSH).

use std::ffi::OsString;
use std::io::IsTerminal;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

/// Must match `piddle_core::GUI_UNAVAILABLE`; kept local so this binary
/// has no dependencies.
const GUI_UNAVAILABLE: i32 = 3;
/// Exit code of the dynamic loader when shared libraries are missing.
const LOADER_FAILED: i32 = 127;

const HELP: &str = "\
piddle: visual process viewer

Usage: piddle [--gui | --tui] [OPTIONS]

  (no flag)    graphical interface if a display is available, else terminal
  -g, --gui    graphical interface (piddle-gui)
  -t, --tui    terminal interface (piddle-tui); remaining options go to it,
               e.g. piddle --tui --interval 500
  -h, --help   show this help
  -V, --version";

enum Mode {
    Auto,
    Gui,
    Tui,
}

fn main() {
    let mut mode = Mode::Auto;
    let mut rest: Vec<OsString> = Vec::new();
    for a in std::env::args_os().skip(1) {
        match a.to_str() {
            Some("-g" | "--gui") => mode = Mode::Gui,
            Some("-t" | "--tui") => mode = Mode::Tui,
            Some("-h" | "--help") => {
                println!("{HELP}");
                return;
            }
            Some("-V" | "--version") => {
                println!("piddle {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => rest.push(a),
        }
    }

    match mode {
        Mode::Tui => exec("piddle-tui", &rest),
        Mode::Gui => exec("piddle-gui", &rest),
        Mode::Auto => {
            if !has_display() {
                exec("piddle-tui", &rest);
            }
            let Some(gui) = find("piddle-gui") else {
                note("the graphical interface (piddle-gui) is not installed");
                exec("piddle-tui", &rest);
            };
            // Run the GUI as a child so we can still fall back if it can't start.
            let status = Command::new(&gui).status().unwrap_or_else(|e| {
                eprintln!("piddle: cannot run {}: {e}", gui.display());
                ExitStatus::from_raw(LOADER_FAILED << 8)
            });
            match status.code() {
                Some(GUI_UNAVAILABLE) => {
                    note("no display could be opened");
                    exec("piddle-tui", &rest);
                }
                Some(LOADER_FAILED) => {
                    note("its libraries (GTK 4) are missing");
                    exec("piddle-tui", &rest);
                }
                Some(code) => std::process::exit(code),
                None => std::process::exit(128 + status.signal().unwrap_or(0)),
            }
        }
    }
}

fn has_display() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"].iter().any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
}

/// Explain a fallback, but only to someone who is looking at a terminal.
fn note(why: &str) {
    if std::io::stderr().is_terminal() {
        eprintln!("piddle: graphical interface unavailable: {why}; starting the terminal interface.");
    }
}

/// Look next to this executable first, then on PATH.
fn find(name: &str) -> Option<PathBuf> {
    let beside = std::env::current_exe().ok().and_then(|p| Some(p.parent()?.join(name)));
    beside
        .into_iter()
        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).map(|d| d.join(name)))
        .find(|p| p.is_file())
}

/// Replace this process with `name`; only returns on failure (then exits).
fn exec(name: &str, args: &[OsString]) -> ! {
    let Some(path) = find(name) else {
        eprintln!("piddle: {name} is not installed");
        std::process::exit(LOADER_FAILED);
    };
    let err = Command::new(&path).args(args).exec();
    eprintln!("piddle: cannot run {}: {err}", path.display());
    std::process::exit(LOADER_FAILED);
}
