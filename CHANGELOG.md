# Changelog

Notable changes in each release. The section for a version is published as
the release notes on GitHub, and a release fails if its section is missing.

## [Unreleased]

### Changed
- The `piddle-gui` package installs on its own: it recommends `piddle`
  instead of requiring it, since only the `piddle` launcher uses the GUI.
  Installing just the downloaded `piddle-gui` .deb, .rpm or Arch package
  used to fail until `piddle` was installed first.

### Added
- A signed package repository on GitHub Pages,
  <https://scott-hiemstra.github.io/piddle/>, for apt (Debian, Ubuntu), dnf
  (Fedora, RHEL, openSUSE) and pacman (Arch), on x86_64 and ARM64. Once it
  is added, installing `piddle-gui` brings `piddle` along and updates come
  with system updates. Each release updates it.

## [0.4.3] - 2026-11-01

### Changed
- Code shared with kpiddle moved to the new
  [piddle-common](https://github.com/scott-hiemstra/piddle-common) crates:
  the bubble layout simulation (`piddle-scene`), the color and size
  encodings and formatting (`piddle-encode`, still re-exported as
  `piddle_core::encode`) and the GTK canvas (`piddle-gtk`). No visible
  change. Building from source now fetches piddle-common from GitHub.

## [0.4.2] - 2026-11-01

### Fixed
- TUI: Terminate and Force Kill pressed in the details popup could target
  the list's selected row instead of the process shown in the popup, if the
  list had changed underneath (for example a process hidden by *active
  only*). The confirmation always named the real target, so nothing was
  signalled silently. The popup also no longer switches to a different
  process that reuses an exited process's PID.
- Processes whose names contain invalid UTF-8 disappeared from both
  interfaces, and couldn't be signalled. Names are now decoded lossily.
- Turning disk I/O collection off and on again showed a false read/write
  spike for one sample.
- A systemd unit name with non-ASCII characters of a particular length could
  crash the sampler.
- The GUI logs pane could miss journalctl stopping, leaving a dead stream
  looking live.

## [0.4.1] - 2026-11-01

### Changed
- The logs pane in the GUI details window now opens expanded, so the logs
  are visible without hunting for them. If you collapse it, PIDdle
  remembers that (in `~/.local/state/piddle/`) until you expand it again.

### Added
- Escape closes the GUI details window.

### Fixed
- The logs pane's expand arrow was invisible on the dark theme, so the
  *LOGS* heading didn't look clickable. It now shows, and highlights on
  hover.

## [0.4.0] - 2026-11-01

### Added
- **By service** view in the GUI and the TUI. Processes are grouped by the
  systemd service, scope or container they run in, read from
  `/proc/<pid>/cgroup`:
  - kinds: system services, user services, scopes (login sessions and
    launched apps), containers (Docker, Podman including rootless,
    Kubernetes, LXC, systemd-nspawn, libvirt machines), OpenRC services,
    kernel, and an "other" fallback named after the cgroup;
  - on systems without systemd, processes fall back to their cgroup name, or
    to a single "root cgroup" group;
  - the GUI tints cluster outlines by kind, and the TUI shows colored kind
    tags.
- **Logs pane** in the GUI details window and the TUI details popup (`l`).
  It follows the journal of the process's service live, colored by
  priority. You can narrow it to one process with *This process only*
  (GUI) or `o` (TUI), and the equivalent `journalctl` command is shown.
  - If you can't read the system journal, the pane explains how to get
    access instead of showing nothing.
  - journalctl only runs while the pane is open.
  - Without systemd, the pane says there is no journal; nothing else is
    affected.
- Details now show each process's service and cgroup path.
- Search also matches service names.

### Changed
- TUI view keys: `3` is now *by service*; *by parent* and *tree* moved to
  `4` and `5`.

### Security
- Unit names and log lines go through the same sanitizer as process names,
  so they can't inject terminal escape sequences.
- Unit names are passed to journalctl as `--unit=…`, so a name that looks
  like an option is never treated as one.

## [0.3.2] - 2026-10-31

### Changed
- Upgraded gtk-rs as one coordinated set (gtk4 0.11; glib, cairo and pango
  0.22) and removed the unused cairo-rs dependency.

## [0.3.1] - 2026-10-31

### Security
- Untrusted process text (names, command lines, executable paths) is
  sanitized: control characters become U+FFFD and bidi overrides are
  dropped.
- Signals are race-free on Linux 5.3+: PIDdle opens a pidfd, verifies the
  start time, then uses `pidfd_send_signal`. Older kernels fall back to
  check-then-`kill`.
- The launcher prefers the terminal UI when running as root (`--gui`
  overrides).
- Workflow actions are pinned to commit SHAs, and Dependabot is set up.

### Changed
- CI actions moved to their Node.js 24 versions.

## [0.3.0] - 2026-10-31

### Added
- Per-process disk I/O (rates and totals) in the GUI and the TUI.
- Network connections of a process (TCP and UDP with state, plus a Unix
  socket count) in the details views.
- ARM64 (aarch64) packages.

## [0.2.1] - 2026-10-31

### Added
- README screenshots.

## [0.2.0] - 2026-10-31

### Added
- Terminal interface (`piddle-tui`), built with Ratatui.
- A GTK-free launcher, `piddle`. It starts the GUI when a display is
  available and the TUI otherwise.

### Changed
- Split packages: `piddle` (launcher and TUI, static binaries) and
  `piddle-gui`. Upgrades from 0.1.0 are handled.

## [0.1.0] - 2026-10-31

### Added
- First release: GTK4 bubble view of processes, where size is memory and
  color is CPU. Free, by-application, by-parent and radial-tree layouts.
- Search, hover cards, and a details window with confirmed Terminate and
  Force Kill.
- Screenshots.
- `.deb`, `.rpm` and Arch packages, built by GitHub Actions.

[0.4.3]: https://github.com/scott-hiemstra/piddle/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/scott-hiemstra/piddle/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/scott-hiemstra/piddle/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/scott-hiemstra/piddle/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/scott-hiemstra/piddle/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/scott-hiemstra/piddle/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/scott-hiemstra/piddle/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/scott-hiemstra/piddle/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/scott-hiemstra/piddle/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/scott-hiemstra/piddle/releases/tag/v0.1.0
