# PIDdle roadmap

The PIDdle family shares one visual language: bubble size is memory, color is
CPU load, and related things are grouped.

| Tool       | Scope                               | Status                          |
|------------|-------------------------------------|---------------------------------|
| PIDdle     | one machine (GUI + TUI)             | released, 0.4.2                 |
| kpiddle    | a Kubernetes cluster                | next; design on `kpiddle` branch |
| recording  | record and play back, for all three | planned                         |
| piddle-mcp | read-only access for Rook (Claude)  | planned                         |
| piddle-net | remote machines via agents          | design on `piddle-net` branch   |

## Up next

1. **kpiddle, read-only TUI first.** Pods with CPU and memory from
   metrics-server, grouped by namespace and workload, with a details popup
   and no actions. Extract shared view code into `piddle-view` only as
   kpiddle's first screen needs it. Then the GUI bubbles, then actions with
   the safety tiers (evict by default, delete as a separate guarded choice).
   See `docs/kpiddle/DESIGN.md` on the `kpiddle` branch.
2. **Snapshot format and recording/playback.** Define one serialized snapshot
   stream (full, then deltas). It serves as:
   - a recording file,
   - the piddle-net wire format, and
   - the input for before/after comparison (two recordings side by side).

   Features: scrub backward, bookmarks, and exportable recordings that can be
   attached to bug reports.
   - Recordings carry periodic keyframes (full snapshots) and an index, so
     scrubbing jumps to the nearest keyframe instead of replaying every delta
     from the start.
   - Live streams never drop deltas. A slow viewer gets a delta computed from
     its own last-known state, or a fresh full snapshot.
3. **piddle-mcp.** A read-only MCP server exposing the current snapshot,
   history and the selected process or workload, so Rook (Claude) can
   investigate the same thing you are looking at. No actions.
4. **piddle-net.** Agents, the directory/key server, and client-bound tokens.
   See `docs/piddle-net/DESIGN.md` on the `piddle-net` branch.

## Small items

- **Missing `/proc` check.** If `/proc` isn't mounted (a chroot or rescue
  shell), say so and exit instead of showing an empty view.
- **README note on partial views.** `hidepid` mounts, containers and sandboxes
  (Flatpak, Snap) show only part of the system.

## Done

- **0.4.0: service grouping.** A "By service" view groups processes by
  systemd service, user service, scope or container (Docker, Podman,
  Kubernetes, LXC, nspawn, libvirt), plus OpenRC services, read from
  `/proc/<pid>/cgroup`. Membership is re-read periodically, so processes
  moved between cgroups follow. Without systemd, processes fall back to
  their cgroup name. This lays groundwork for kpiddle's node view and the
  logs pane.
- **0.4.0: logs pane.** Each process's details view follows the journal of
  its service live: an expander in the GUI, `l` in the TUI.
  - It spawns `journalctl -o json` instead of linking libsystemd, so the
    musl builds stay static.
  - Every line is sanitized.
  - Without access to the system journal, or without journalctl at all, the
    pane says so instead of showing nothing.
- **0.4.0: release notes from `CHANGELOG.md`.** The release workflow fails
  without a dated section; later, wrapped lines were joined for GitHub.
- **0.4.1: logs pane polish.** The pane opens expanded (collapsing it is
  remembered), the expand arrow is visible, and Escape closes the details
  window.
- **0.4.2: code-review fixes.**
  - TUI details actions target the details process.
  - Non-UTF-8 process names are handled.
  - Re-enabling disk I/O no longer shows a spike.
  - Unicode unit names no longer crash the sampler.
  - The GUI journal no longer misses an end-of-stream.

## Parked

- **Storage view** (size = disk space, color = I/O). This is a different
  problem, and good disk-usage tools already exist.
- **Connection map.** Hard to make readable, and it needs elevated access to
  see other users' sockets. Revisit after recording exists.
- **AUR and COPR packages.**
