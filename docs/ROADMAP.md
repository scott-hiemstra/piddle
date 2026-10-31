# PIDdle roadmap

The PIDdle family shares one visual language: bubble size is memory, color is
CPU load, and related things are grouped.

| Tool       | Scope                             | Status                                |
|------------|-----------------------------------|---------------------------------------|
| PIDdle     | one machine (GUI + TUI)           | released                              |
| kpiddle    | a Kubernetes cluster              | design: `kpiddle` branch              |
| piddle-net | remote machines via agents        | design: `piddle-net` branch           |
| recording  | record and play back, for all three | planned                             |

## Order

1. **Service/cgroup grouping in PIDdle.** Add a "By service" view that groups
   processes by systemd unit, slice or container, read from
   `/proc/<pid>/cgroup`. It's small, and it lays groundwork for later items:
   - containers are cgroups, so a Docker/Podman view mostly falls out of it;
   - kpiddle's node view needs the same process → pod mapping;
   - the logs pane (below) needs the unit name.
2. **Logs pane in the details view.** Show the journal for the selected
   process's unit (or `_PID` for processes outside a unit). It appears as a
   tail in the details window/popup, following live.
   - Implemented by spawning `journalctl -o json`, not by linking libsystemd,
     so the static musl builds stay static.
   - Lines are run through `sanitize()`, because log text is arbitrary bytes
     and would otherwise be an escape-sequence injection vector in the TUI.
   - Without permission to read the system journal (not in `systemd-journal`
     or `adm`), the pane says so instead of showing nothing.
   - This is the "why is this bubble red?" companion to the picture. It is
     not a general log viewer.
   - kpiddle reuses the pane for container logs, and piddle-net carries it as
     a separate `logs` scope.
3. **kpiddle, read-only TUI first.** Extract shared view code into
   `piddle-view` only as kpiddle's first screen needs it.
4. **Snapshot format and recording/playback.** Define one serialized snapshot
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
5. **piddle-mcp.** A read-only MCP server exposing the current snapshot,
   history and the selected process or workload, so Rook (Claude) can
   investigate the same thing you are looking at. No actions.
6. **piddle-net.** Agents, the directory/key server, and client-bound tokens.

## Parked

- **Storage view** (size = disk space, color = I/O). This is a different
  problem, and good disk-usage tools already exist.
- **Connection map.** Hard to make readable, and it needs elevated access to
  see other users' sockets. Revisit after recording exists.
- **AUR and COPR packages.**
