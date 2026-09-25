# PLAN: tetron-messageboard multi-board (one board per network)

Status: implemented 2026-09-25 (all three repos build/test clean). Date: 2026-09-25.

## Implementation summary (2026-09-25)

- **tetron-messageboard**: templated Linux unit `contrib/tetron-messageboard@.service`
  + per-instance macOS plist; per-network data dirs (`net/<token>/`) with a
  best-effort legacy migration; env-file registry
  (`~/.config/tetron-messageboard/<token>.env`) doubling as the instance list;
  new CLI `uninstall --network|--all` (require-a-flag when several), `list [--json]`,
  `restart-all`. `instance_token` (readable, hash-disambiguated) has unit tests.
- **tetron-webui**: `src/messageboard.rs` shells the binary (`list --json`,
  `install/uninstall --network`, `restart-all`); `/api/messageboard/{status,start,stop,restart-all}`;
  addon `installed` = binary-present for messageboard; `app.js` board-manager
  panel (per-network start/stop, add-board picker, restart-all, open links).
- **tetron**: `contrib/install-tetron-suite.sh` treats messageboard as
  binary-only (`component_binary_only`) -- places the binary, never runs
  `install`, so a multi-network node no longer aborts; runs `restart-all` after
  an upgrade.

Decisions as agreed: require-flag + `--all`; escaped (readable) instance name;
`restart-all`.

---


## Goal

A node in several tetron networks must be able to run a message board per
network, simultaneously. Install is decoupled: the suite script only places
the binary; boards are started per network from the tetron-webui Add-ons
panel (or an explicit CLI command). Adding a board to one network must not
disturb boards on other networks, and must never abort the suite install on
a multi-network node (the current failure).

## Current state (what exists today)

- Board binds one network mesh IP, never `0.0.0.0`/`127.0.0.1`. Mesh
  membership is the access control. See `src/service.rs::install(port, network, bind_ip)`.
- Single fixed unit `tetron-messageboard` (Linux `systemd --user`) or
  `com.tetron.messageboard` plist (macOS launchd). One instance per node.
- Single `config::InstallState` (one network pinned at install).
- CLI `install {port, network}` / `uninstall`. `--network` optional, resolved
  by `roster::select_network` (errors on ambiguity when >1 network).
- Suite `contrib/install-tetron-suite.sh` (in the `tetron` repo) runs
  `"$dest" install` for every selected component. For messageboard this runs
  with no `--network`, so on a multi-network node `select_network("")` errors
  and the whole suite install hits `fatal`. This is the bug.
- tetron-webui discovery `src/board_discovery.rs` probes each network's
  members on their mesh IP at the default port `28088` and matches
  `/health.network`. Addon status in `src/addons.rs` keys off a single
  `linux_unit` string.

## Key design fact: boards share one port

Each network gives this node a different mesh IP (`my_ip` in that network's
`/24`). A socket bind is keyed on `IP:port`, not port alone. So board A binds
`A_ip:28088` and board B binds `B_ip:28088` with no conflict. Consequences:

- No port-allocation scheme. Every board keeps default `28088`.
- Discovery already surfaces one board per network (it probes each network's
  mesh IPs at the default port and matches the `/health.network`). Only the
  stale "a node hosts a board for exactly one network" assumption in
  `board_discovery.rs::probe_one` needs correcting; the probe logic is fine.

Boards are distinguished purely by which network mesh IP they bind. The
`TETRON_MESSAGEBOARD_PORT` override remains for fleets that standardised on a
different port, but it is irrelevant to running several boards on one node.

## Rejected alternative: one board on a widened IP range (/16)

Considered and rejected. Reasons:

- You bind a socket to one address, not a range. A "/16 bind" collapses to
  either `0.0.0.0` (which also exposes the physical LAN/public NIC and breaks
  "the mesh is the access control") or listening on each mesh IP anyway
  (which is multi-listener, not a wider mask).
- The shared-supernet assumption is false in general. `default_subnet()` in
  the `tetron` repo is `10.88.0.0/24`, and auto-assigned networks are
  deliberately bumped to non-overlapping blocks (see
  `src/addressing.rs`). Networks sharing a `10.55/16` prefix only happen when
  an operator hand-picks `--subnet` for each; another operator's networks may
  share no supernet at all.
- It is a confidentiality regression: one board answering several networks
  lets a member of only network A read and post content from B and C, silently
  merging networks the operator kept separate on purpose.

A deliberately shared cross-network board is a separate future feature. If it
is ever built it must be an explicit opt-in that names the exact networks and
warns that it merges them, never a raw netmask, never a default. Out of scope
here.

## Work items

### 1. tetron-messageboard (core change)

- Templated Linux unit `tetron-messageboard@.service`; one launchd plist
  `com.tetron.messageboard.<instance>` per network on macOS (launchd has no
  `@` templating). Each instance binds its network's `my_ip:28088`.
- Per-network install state: one state file per network (or a keyed map)
  replacing the single `InstallState`. Records the concrete network name,
  `installed_by_coordinator`, and port.
- Per-instance env file `~/.config/tetron-messageboard/<inst>.env` carrying
  `TETRON_MESSAGEBOARD_NETWORK` (and port if overridden). Unit references it
  with `EnvironmentFile`.
- Instance identity: network names can contain characters unsafe for a
  systemd `%i`. Use the escaped network name (`systemd-escape`) as the
  instance token so `systemctl --user list-units` stays human-readable.
- CLI:
  - `install --network X [--port P]`: add or refresh one board, idempotent
    per network. `--network` is effectively required once the node has >1
    network (reuse `roster::select_network`).
  - `uninstall --network X`: remove one board. Bare `uninstall` with several
    boards present errors and asks for `--network` or `--all`. Add `--all`.
  - `list`: enumerate installed boards (network, port, mesh IP, running).
  - `restart-all`: restart every `tetron-messageboard@*` instance. Called
    after a binary upgrade to pick up the new exe (see suite section).

### 2. tetron-webui

- Give messageboard its own status/API path (mirrors `src/sync_receiver.rs`),
  because a single `linux_unit` string can no longer represent N boards.
  Routes:
  - `GET  /api/messageboard/boards`: local boards from install state, plus
    which of this node's networks have no board yet.
  - `POST /api/messageboard/start {network}`: runs
    `tetron-messageboard install --network X`. Per-user, no sudo.
  - `POST /api/messageboard/stop {network}`: runs `uninstall --network X`.
- Panel (`static/app.js`) replaces the static instructions with a live table
  built like `renderSyncReceiverPanel`: running boards (network to open link),
  plus an "Add board" dropdown listing networks that do not yet have one, with
  Start/Stop controls.
- Two-phase row state, distinct signals:
  - Binary present? Check `/usr/local/bin/tetron-messageboard`. If absent,
    show the sudo one-liner (binary placement needs root).
  - Boards running? From `/api/messageboard/boards`. If the binary is present,
    show the board manager instead of the install command.
  - Add a "binary present" field distinct from the current unit-based
    `installed` in `src/addons.rs`.
- Fix the stale single-board assumption comment in
  `src/board_discovery.rs::probe_one`.

### 3. tetron (suite script)

- `contrib/install-tetron-suite.sh`: make messageboard binary-only. Special
  case it the way `install_backup` is handled: place the binary, skip the
  `"$dest" install` service step. This alone removes the multi-network abort.
- The interactive default for messageboard is already `[y/N]` (No) for fresh
  installs; keep it. Upgrade still pre-answers Yes so an installed component is
  not left stale.
- Upgrade semantics: root replaces the binary, but boards are per-user units
  root should not reach into a user session. After a binary upgrade the webui
  (or the user) calls `tetron-messageboard restart-all` to pick up the new
  exe. Surface this in the addon panel.

## Decisions (defaults chosen, confirm if you disagree)

1. Bare `uninstall` with multiple boards: error and require `--network` or
   `--all`. Add `--all`.
2. Instance identity: escaped network name (not a hash), for readable
   `systemctl` output.
3. Upgrade restart: add `restart-all` to the binary, webui calls it after an
   upgrade.

## Build order

1. tetron-messageboard templated units + per-network state + CLI changes.
   Independently testable on a multi-network node with two `install --network`
   calls.
2. tetron-webui API routes + panel.
3. tetron suite one-line binary-only change.

## Security notes

- Each board still binds exactly one network mesh IP. The per-network
  isolation model is unchanged; multi-board is N isolated boards, not a merge.
- No board ever binds `0.0.0.0`. The webui start route must pass the concrete
  network so the bind IP is that network's `my_ip`.
