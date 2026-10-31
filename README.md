# PIDdle

[![CI](https://github.com/scott-hiemstra/piddle/actions/workflows/ci.yml/badge.svg)](https://github.com/scott-hiemstra/piddle/actions/workflows/ci.yml)

A visual process viewer for Linux, written in Rust with GTK4.

Each running process is a bubble:

- **Size is memory.** Bubble area grows with resident memory (RSS).
- **Color is CPU load.** Colors run from cool blue through yellow to hot red. **100% means one fully used core**, so on an 8-core machine a single process can reach 800%. The color scale saturates at 100%.

Hover a bubble for CPU, memory and thread count. Click it to open the details window, which has the **Terminate** (SIGTERM) and **Force Kill** (SIGKILL) actions. Both are protected against mistakes (see [Safety](#safety-when-stopping-processes)).

## Features

- Real data sampled from `/proc` every 250 ms, 500 ms, 1 s or 2 s (configurable), with a pause button.
- Smooth animation: sizes, colors, fade-in and fade-out ease between samples. A damped, position-based simulation keeps existing bubbles where they are and only nudges neighbours.
- Four layouts:
  - **Free**: one packed cloud.
  - **By application**: processes clustered by executable, for example all `chrome` processes. Each cluster shows its count, total CPU and total memory.
  - **By parent**: processes clustered by parent PID.
  - **Process tree**: a radial tree with the root in the center and one ring per generation. Parent→child links are drawn, and siblings are ordered by PID so the tree doesn't reshuffle. Every parent process gets a tag with its name, PID and child count, even when its bubble is tiny.
- Search by name, command line or PID prefix. Matches are ringed and everything else is dimmed, so the layout doesn't jump around.
- Filters: show or hide kernel threads (hidden by default), and **Active only**, which shows processes that used CPU in the last few seconds.
- Screenshots: the camera button or **Ctrl+S** saves the canvas and sidebar as a full-resolution PNG in `~/Pictures` and copies it to the clipboard. With Ctrl+S, the hover card for the process under the mouse is included.
- Details window with a live 60-second CPU history, CPU / memory / thread cards, parent, user, start time, priority, executable and command line.
- Sidebar with a color and size legend. The size references are drawn at the current zoom, so they match the bubbles on screen. Below the legend are system totals, a top-CPU list (click an entry to open its details), and PIDdle's own CPU use and per-sample cost.
- A dark interface that stays dark and readable even when the desktop forces a light GTK color scheme, as KDE's Breeze does. PIDdle also hides the harmless `Unknown key gtk-modules` warning that KDE's GTK4 settings cause.

## Keyboard

| Key | Action |
|---|---|
| Ctrl+S | Save a screenshot and copy it to the clipboard |
| Start typing | Search |
| Esc | Clear the search |

## Safety when stopping processes

- **Blocked:** PID 1 (init) and kernel threads can't be signalled at all.
- **Critical:** for desktop-session and core system processes (`gnome-shell`, `systemd`, `Xwayland`, `dbus`, `sddm`, `NetworkManager`, `pipewire` and so on), PIDdle itself, and any process PIDdle runs inside, you have to type the process name before the button unlocks. The details window shows a warning banner for these.
- **Everything else:** a confirmation dialog where Cancel is the default. It mentions how many child processes there are and suggests Terminate before Force Kill.
- **PID reuse:** the process's start time is checked again right before `kill(2)`, so a PID that now belongs to a different process is never signalled.
- These rules live in `piddle_core::actions::assess`, so every front end gets them.

## Low overhead

- Each sample does one small read of `/proc/<pid>/stat` per process. Command line, owner and application name are read once per process and cached by (PID, start time), so a reused PID is detected.
- Sampling runs on a background thread. Only the latest snapshot is kept, so the UI never processes a backlog.
- The canvas draws through GtkSnapshot/GSK render nodes, so the GPU does gradients, clipping, strokes and text rendering.
- The frame clock stops as soon as nothing visible is changing. While only colors are easing, the canvas redraws at about 20 fps instead of the full refresh rate.

## Install

Each script builds the release binary and wraps it, together with the launcher entry, README and license, in a native package:

| Distribution | Build | Install | Remove |
|---|---|---|---|
| Debian, Ubuntu | `packaging/build-deb.sh` | `sudo apt install ./target/debian/piddle_0.1.0_amd64.deb` | `sudo apt remove piddle` |
| Fedora, RHEL, openSUSE | `packaging/build-rpm.sh` | `sudo dnf install ./target/rpm/piddle-0.1.0-1.x86_64.rpm` | `sudo dnf remove piddle` |
| Arch Linux | `packaging/build-arch.sh` | `sudo pacman -U target/arch/piddle-0.1.0-1-x86_64.pkg.tar.zst` | `sudo pacman -R piddle` |

All three install `/usr/bin/piddle` and a "PIDdle" entry in the app menu. They need GTK 4.14 or newer and glibc 2.34 or newer.

- **Dependencies:** the `.deb` gets them from `dpkg-shlibdeps` and the RPM from rpmbuild's library scan. The Arch package declares `gtk4>=4.14`, `glib2`, `cairo`, `pango`, `graphene`, `glibc` and `gcc-libs`.
- **Building on any distro:** the scripts don't have to run on the target distribution. On Debian you need `rpm` for the RPM and `makepkg` plus `pacman-package-manager` for the Arch package.
- **Tested:** the RPM installs on Fedora 43 and the Arch package on current Arch Linux, each with all libraries resolved.

### Releases

Pushing a version tag builds all three packages on GitHub Actions and attaches them to a GitHub release:

```sh
git tag v0.1.0 && git push origin v0.1.0     # tag must match the version in Cargo.toml
```

The `.deb` and `.rpm` are built on Ubuntu 24.04. The Arch package is built from the same binary in an Arch Linux container. You can also run the **Release** workflow by hand from the Actions tab; it then only builds the packages as downloadable artifacts and publishes nothing.

## Build and run from source

Requirements: Rust (stable, 2024 edition) and GTK ≥ 4.14 development files (`libgtk-4-dev` on Debian/Ubuntu).

```sh
cargo run --release            # runs the GUI (binary name: piddle)
cargo test --workspace         # unit tests (piddle-core)
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings   # what CI checks
```

You can only signal other users' processes when PIDdle runs with the right privileges.

## Project layout

```
crates/
  piddle-core/      UI-independent; depends only on libc and async-channel
    sampler.rs      /proc sampling: Sampler (synchronous) or spawn() (background thread)
    encode.rs       CPU → color, memory → radius, formatting helpers
    actions.rs      risk assessment (assess), PID-reuse-safe signalling, state names
    examples/top.rs plain-text front end (starting point for a TUI)
  piddle-gtk/       GTK4 front end (binary `piddle`)
    view.rs         bubble model, layouts, physics, GSK rendering
    canvas.rs       custom widget that renders the view
    legend.rs       color/size legend
    details.rs      details window, CPU history, confirm / type-to-confirm dialogs
    screenshot.rs   capture the window contents to PNG and clipboard
    main.rs         window, header controls, sidebar, theming, wiring
.github/workflows/  ci.yml (fmt, clippy, test, build), release.yml (packages + GitHub release)
data/               desktop entry
packaging/          build-deb.sh, build-rpm.sh, build-arch.sh (+ common.sh)
```

A future Ratatui TUI only needs to depend on `piddle-core`, for example:

```sh
cargo run -p piddle-core --example top --release
```

## Developer tools

`piddle --snapshot out.png [free|app|parent|tree] [query]` renders the canvas off-screen to a PNG using live data. It still needs a display connection.

## Contributors

- **Scott Hiemstra** ([scott@hiemstra.us](mailto:scott@hiemstra.us)): author and maintainer; product direction, design and testing
- **Claude** (Anthropic): AI pair programmer; implementation

## License

PIDdle is released under the [MIT License](LICENSE). Copyright © 2026 Scott Hiemstra.
