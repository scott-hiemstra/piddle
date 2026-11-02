# PIDdle roadmap

The PIDdle family shares one visual language: bubble size is memory, color is
CPU load, and related things are grouped.

| Tool       | Scope                               | Status                          |
|------------|-------------------------------------|---------------------------------|
| PIDdle     | one machine (GUI + TUI)             | released, 0.4.5                 |
| kpiddle    | a Kubernetes cluster                | released, 0.2.0, in its own repo |
| recording  | record and play back, for all three | planned                         |
| piddle-mcp | read-only access for AI agents      | planned                         |
| piddle-net | remote machines via agents          | design, in its own repo         |

Each tool has its own repo (kpiddle, piddle-net), and code they share is in
piddle-common. This file is the family overview; kpiddle's and piddle-net's
detailed plans live in their repos.

## Up next

1. **kpiddle.** 0.2.0 has the TUI and GUI with pod details, evict, rollout
   restart and delete behind `--allow-actions`, and packages. Next there: a
   labeled requests/limits mode for clusters without metrics-server, one
   bubble per workload for big clusters, and pod events and logs in the
   details. See "Next" in the kpiddle repo's
   [docs/DESIGN.md](https://github.com/scott-hiemstra/kpiddle/blob/main/docs/DESIGN.md).
2. **Containers view.** A container picker that limits the view to one
   container's processes, or to all containers, laid out by whatever
   manages them.
   - **Picker:** a dropdown in the GUI header (a key in the TUI) listing the
     running containers by name, with "All containers" first. Choosing one
     shows only its processes, with the same bubbles, details and actions;
     choosing nothing gives today's full view.
   - **Only when there are containers:** on a machine without any, the
     dropdown, the key and its help line don't appear at all. They show up
     when the first container starts, and if the container being viewed
     stops, the view says so and offers to go back to all processes.
   - **Layout by manager:** Docker Compose project → service → containers
     (replicas side by side); Podman pods → containers; Kubernetes pods →
     containers (with a pointer to kpiddle for the cluster view);
     standalone containers, LXC, nspawn and libvirt machines on their own.
     Totals per project, pod and container, like the app groups today.
   - **Data:** 0.4.0's cgroup reading already finds Docker, Podman,
     Kubernetes, LXC, nspawn and libvirt containers, but knows Docker and
     Podman ones only by short ID. Names, the Compose project and service
     (`com.docker.compose.*` labels) and Podman pods come from the
     runtime's API socket: read-only list and inspect calls only, never
     anything that changes a container. Labels are sanitized like process
     names.
   - **Access:** the Docker socket needs root or the `docker` group, which
     is root-equivalent, so PIDdle must not suggest joining it; rootless
     Podman's user socket works as the user. Without access, containers
     keep their short IDs and the picker says why, as the logs pane does
     for the journal.
   - **Out of scope:** starting, stopping or restarting containers (that's
     the runtime's job); per-process actions work as they do now.
3. **Snapshot format and recording/playback.** Define one serialized snapshot
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
4. **piddle-mcp.** A read-only MCP server exposing the current snapshot,
   history and the selected process or workload. Any MCP-capable agent or
   client can then investigate the same thing you are looking at. No actions.
5. **piddle-net.** Agents, the directory/key server, and client-bound tokens.
   See `docs/DESIGN.md` in the
   [piddle-net repo](https://github.com/scott-hiemstra/piddle-net).

## Small items

- **AUR packages** for `piddle` and `piddle-gui` (and kpiddle's), alongside the
  pacman repository on GitHub Pages. Needs an AUR account and SSH key.

## Done

- **0.4.5: `subset=pid` fix.** PIDdle runs under a `/proc` mounted with
  `subset=pid` again, showing the whole-machine figures it can't read as
  unavailable instead of zero.
- **piddle-common 0.1.1: shared details windows.** The details-window
  building blocks and the type-the-name confirmation moved to
  `piddle_gtk::details`, shared with kpiddle. piddle-common also holds the
  one `use-local-common.sh` (the apps keep thin wrappers), tested in CI.
- **kpiddle 0.2.0.** GUI pod details and actions, packages and a package
  repository; see the kpiddle repo.
- **0.4.4: install and edge cases.** A signed package repository on GitHub
  Pages; `piddle-gui` installs without `piddle`; a clear message when `/proc`
  isn't mounted; a README section on partial views.
- **0.4.3: repo split.** kpiddle and piddle-net moved to their own repos, and the
  code they share with PIDdle (bubble scene, encodings, GTK canvas) to
  piddle-common.
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
