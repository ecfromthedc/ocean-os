# ocean-daemon freeze + intake wedge — diagnosis

Branch `fix/daemon-freeze`. All line numbers cite the pre-fix tree
(`6c0b84e`, the worktree's base) unless marked otherwise. Both symptomatic
builds (Jul-29, Aug-14) postdate every mechanism named here: OAuth refresh
landed 2026-07-07 (`d02c27a`), fire-and-ack 2026-07-11 (`705ed00`), the
session module (with the full-parse list) 2026-07-12 (`4cf8747`), the
heartbeat 30s timeout 2026-07-16 (`749e69a`).

---

## Bug A — total turn freeze

### What the evidence pins down

- Sessions stay `running`, `updated_ms` freezes → checkpoint saves stop
  (`updated_ms` only advances on `session::save`).
- Log completely silent, including span CLOSE lines (`FmtSpan::NEW | CLOSE`
  is on, `main.rs:831-833`), with **no `provider_stream` close and no
  error** → in-flight turns stopped mid-round *and no deadline ever fired*.
- Every provider round IS deadline-bounded in healthy conditions:
  `timeout_at(round_deadline, stream_work)` at
  `crates/ocean-runtime/src/agent_loop.rs:607-614,714`, default 300s
  (`types.rs:207`, env `OCEAN_TURN_TIMEOUT_SECS`,
  `ocean-agent/src/lib.rs:2487`), and the HTTP layer has connect 10s +
  read-idle 120s (`ocean-protocol/src/http.rs:49-57`; all four providers
  use it: `anthropic.rs:143`, `codex.rs:72`, `openai.rs:394`,
  `google.rs:358`).
- Therefore a persistent, silent, error-free freeze **cannot** be a hung
  provider socket on a healthy runtime. Either (a) the frozen turns are
  parked at an await that has *no* deadline, or (b) the runtime's worker
  pool is starved so even armed timers are never serviced.
- `sample` showing every thread in `_pthread_cond_wait` / `kevent` is what
  an *idle* tokio runtime looks like: parked tasks consume no threads. It
  fits (a) directly, and fits (b) at any instant when the blocking calls
  have momentarily drained (or when the one blocked thread's frame —
  `read`, `fsync`, `waitpid` — was lost in the summary).

### Mechanism 1 (primary suspect): the global OAuth single-flight queue at the top of every turn

`crates/ocean-agent/src/lib.rs:744` — **every** turn, before credential
resolution, awaits `oauth_refresh::ensure_fresh(&auth_file)`.

`crates/ocean-agent/src/oauth_refresh.rs` (pre-fix):

- `:92` `let mut guard = state().lock().await;` — ONE process-global
  `tokio::sync::Mutex`, taken with a **queueing** lock by every turn.
- `:94` `std::fs::read_to_string(auth_file)` — blocking file I/O **on the
  async worker, under the guard**.
- `:118` `refresh_token(...).await` — a network await **under the guard**
  (inner bound: 15s reqwest total timeout, `ocean-protocol/src/oauth.rs:18,49-52`).
- `:150` `write_atomically` — blocking write + `fsync(file)` + rename +
  `fsync(dir)` (`durable_rename`) **under the guard**.

Interleaving that parks the fleet:

1. Machine's `openai-codex` block is expired with a permanently-invalid
   refresh token → every 60s (cooldown, `:28,:109-113`) some turn's
   `ensure_fresh` re-attempts the exchange. This is exactly the observed
   co-signal: `oauth_refresh: OAuth refresh failed; leaving block as-is
   (60s cooldown) block=openai-codex` at turn start.
2. Turn T1 takes the guard and enters the blocking `read_to_string` /
   `fsync`, or the network await. If that call wedges — a hung filesystem
   under `auth_file`, a wedged volume on fsync, or any await that loses its
   bound — the guard is **held forever**. The blocking variants also pin
   T1's worker thread.
3. Turns T2…Tn — every subsequent turn, every session — hit
   `state().lock().await` and park **forever**: no deadline, no log line,
   no state change. Sessions show `running`, `updated_ms` frozen, log
   silent, process samples idle. HTTP that doesn't touch this lock still
   answers; anything queued behind a pinned worker intermittently hangs.

Status: the *defect* (global queueing lock held across network + blocking
file I/O on the hot path of every turn) is proven from the code; the
*specific wedge trigger* on the Mini (which call hung) is not directly
provable from source. It is the only code path found where all turns
serialize on one unbounded await at turn start — matching the freeze
topology and the co-signal exactly.

Fix shipped (commit `96baca2`):
- `try_lock` — single-flight now **skips** instead of queueing. At most one
  turn is ever exposed to the refresh; a wedged refresh strands itself, not
  the fleet. (No double-spend of rotating refresh tokens: still at most one
  exchange in flight.)
- A 20s outer `tokio::time::timeout` on the exchange, so the await under
  the guard is bounded even if the inner reqwest bound is ever lost.
- Regression test `in_flight_refresh_never_queues_other_turns` holds the
  guard and proves `ensure_fresh` returns immediately (it hangs pre-fix).

Residual (documented, not patched): the blocking `read_to_string` /
`write_atomically` still run on a worker thread inside the one refreshing
turn. Small local file; acceptable once the lock no longer amplifies it.

### Mechanism 2 (co-factor, proven defect): worker-pool starvation by synchronous I/O on the async runtime

The daemon runs a stock `#[tokio::main]` multi-thread runtime
(`main.rs:801`; workers = CPU count, no override, no `block_in_place`
anywhere). The following turn-correlated work all executed **directly on
runtime worker threads**:

| Work | Site | Cost profile |
|---|---|---|
| Full-archive session list per poll | `session::list`, `ocean-agent/src/session/mod.rs:888-948` — reads and `serde_json`-parses **every session file in every bucket**, then paginates *after* | seconds per call at fleet scale (the TUI's own comment cites "700+ files / 55MB"); `GET /v1/agent/sessions` is polled continuously by dashboards |
| Session hydration in turn intake | `session_detail` → `load_resumable`, `main.rs:6020-6029` → `session/mod.rs:475-539` (full read+parse of every duplicate candidate) | scales with transcript size, per resumed turn |
| Project/workspace resolution in intake | `resolved_lifecycle_project_id`, `main.rs:8261-8300` → blocking `git rev-parse` subprocess spawns (`session/mod.rs:144-162`, `ocean-agent/src/lib.rs:1311-1318`) + `canonicalize` per registered project | 10s of ms healthy; unbounded against a hung mount |
| Checkpoint persistence | `run_prompt`'s event pump, `ocean-agent/src/lib.rs:2294-2307` — full pretty-JSON serialize + write + `fsync(file)` + rename + `fsync(dir)` (`session/mod.rs:410-442`) **per tool round**, plus the final save at `:2339-2345` | up to seconds per checkpoint on big sessions; concurrent turns multiply |
| Session load at turn start | `run_prompt`, `ocean-agent/src/lib.rs:1985` + full `messages.clone()` at `:2010` | same as hydration |

With 5+ concurrent turns plus dashboard polling, enough workers sit in
blocking syscalls simultaneously that: the listener's accept loop starves
(observed: new TCP connects saturate for minutes), armed timers are
serviced late or never, and in-flight streams stall mid-round with no
timeout error — the mid-`provider_stream` silence. When every blocking
call eventually returns, the daemon limps back (observed recovery); if any
one of them is genuinely unbounded (git against a dead FUSE path, fsync on
a wedged volume, a `write(2)` to stdout when the log consumer stops
draining a full pipe — the tracing writer is synchronous stdout,
`main.rs:819-835`), the freeze is permanent.

Fixes shipped (commit `17964f9`): the three highest-traffic call sites
(`GET /v1/agent/sessions` listing; intake session hydration; intake
project/git resolution) now run under `tokio::task::spawn_blocking` with
identical fail contracts. Remaining instances are listed under
"What remains".

### Ruled out

- **Bounded channel deadlock (hypothesis 3): ruled out.** Every channel on
  the turn path is unbounded or non-blocking: the daemon→SSE bridge
  (`main.rs:6315` `unbounded_channel`), the runtime event pump
  (`ocean-agent/src/lib.rs:2216`), the loop's sink send
  (`agent_loop.rs:1726`, `send` on unbounded), and both buses are
  `broadcast` (send never blocks; `bus.rs`). The bounded channels found
  (`persistent_rooms.rs:1891`, `room_federation.rs:1587`,
  `extension_service.rs:2239`) are not on the turn path.
- **Tiny runtime / block_in_place starvation (hypothesis 4) as configured:
  ruled out.** Stock multi-thread runtime, no `block_in_place` in the tree,
  no `worker_threads` override. (Starvation via *blocking syscalls* is
  mechanism 2 above — a different cause than the hypothesized
  spawn_blocking exhaustion; the one heavy spawn_blocking user, the
  Longhouse consult, is single-flight with a 250ms abandon,
  `longhouse_turn_preparation.rs:25-30,183-270`.)
- **`reqwest::blocking`: absent** from the tree.
- **Unbounded provider streams:** ruled out for a *healthy* runtime —
  connect/read-idle timeouts plus the per-round `timeout_at` (see above).
  This is precisely why the silent forever-freeze indicts a no-deadline await
  (mechanism 1) or a starved runtime (mechanism 2), not the socket.
- **`std::sync::Mutex`/RwLock held across `.await` on the turn path: none
  found.** `request_control.rs` guards are all scoped and the TASK-61
  finalize hook is synchronous under the write lock;
  `capability.rs`/`todo.rs`/`hashline_edit.rs` guards drop before awaits;
  the room store mutex is documented and verified drop-before-await.
- **Retry storms:** bounded — 3 attempts, 60s ceiling
  (`ocean-protocol/src/retry.rs:34-38`).
- **SSE clients cancelling running turns:** the turn body is spawned
  detached (`main.rs:6731`) — post-ACK disconnects never cancel; only the
  pre-spawn window did (Bug B, below).

### Worth knowing (freeze-forever path that does NOT match tonight's trace)

The permission gate awaits its decision oneshot with **no deadline**
(`main.rs:2475-2501`, guarded only by the cancel token). On a headless
fleet, any tool that `requires_permission()` under `Automatic` mode parks
its turn forever, holding the session lease. It does not match tonight's
evidence (state would read `waiting_for_permission`, and the round would
have closed first), but it is a real silent-stall hazard for unattended
seats.

---

## Bug B — intake wedge and 32–55s ACKs

### What ran before the 202 (pre-fix), all inside the axum request future

`agent_turn` (`main.rs:5837-6944`), in order:

1. Turn permit try-acquire — sync, fast (`:5872`).
2. `resolve_cwd_for_turn` — project-index read on cwd-less turns (`:5910`).
3. **Session hydration** for resumed turns — full JSON read+parse of the
   persisted session (every duplicate-bucket candidate), synchronously on
   the worker (`:6020-6029` → `session/mod.rs:475-539`). Heartbeat wakes
   always resume a seat session, so every wake paid this.
4. Named-agent resolution — agents-dir scan (no-op for `agent: None`).
5. `try_session_operation` — sync try-lock (`:6113`).
6. **Project/workspace resolution** (`:6135` → `:8261-8300`) — one
   blocking `git rev-parse --show-toplevel` spawn
   (`session/mod.rs:144-162`), `projects.json` load, `canonicalize` of
   every registered project root, and up to another blocking
   `git rev-parse --git-common-dir` spawn
   (`ocean-agent/src/lib.rs:1294-1340`) — **twice** (workspace-root and raw
   workspace).
7. Longhouse consult — the one well-behaved step: `spawn_blocking`,
   single-flight, 250ms hard abandon (`:6174`).
8. Registration, lifecycle claim, spawn of the detached turn (`:6731`),
   `attach_request_handle` (`:6919`), then the 202 (`:6927`).

So the answer to "does it synchronously hydrate/load the session, scan
skills/longhouse, or refresh credentials before ACK?": it **hydrates the
session and resolves projects/git synchronously** (items 3 and 6); the
longhouse scan is bounded (250ms); credential refresh is *not* pre-ACK (it
runs post-spawn inside `prompt_inner`, `ocean-agent/src/lib.rs:744`).

None of 3/6 costs 32–55s on a healthy idle box by itself. The measured
latency is those blocking steps **plus queueing behind the chronically
blocked worker pool** — dominated by `GET /v1/agent/sessions` polls that
full-parse the entire session archive per request
(`session/mod.rs:888-948`) and by concurrent turns' checkpoint fsync
saves. That same worker-blocking is the accept starvation: 5+ concurrent
turn intakes pin 5+ workers on an ~8-10-core Mini and new TCP connects sit
in the backlog until a worker frees.

### Turn cancellation on client disconnect — CONFIRMED

The vulnerable window is precise. Pre-spawn, the request future suspends at
exactly two awaits: the Longhouse consult (`:6174`) and the registry write
in `register_running_request` (`:6220`; `:6239` is an error branch). Axum
drops the handler future on disconnect at its next suspension point.
Sequence observed by the fleet:

1. Wake POST arrives; intake grinds through the blocking work (30s+).
2. ocean-heartbeat's client timeout (30s hardcoded,
   `crates/ocean-heartbeat/src/main.rs:264-266`) fires; reqwest closes the
   connection.
3. The handler future, blocked in sync work, reaches its next await —
   typically the Longhouse consult — and is dropped there. **The turn
   evaporates: never registered, never spawned, nothing logged.** The wake
   reports failure; the seat never runs.
4. Variant: if the drop lands after both awaits, the turn spawns and runs
   but the 202 is lost — the wake reports failure while the turn actually
   executes (double-fire hazard on retry).

There was no window that leaks a registered-but-unspawned `Running` ghost:
between `register_running_request` returning and `tokio::spawn(:6731)`
there are no awaits, and the `TurnTerminalGuard` (`:6269`) moves into the
spawned task.

For contrast, the sibling intake `POST /v1/requests` (`create_request`)
already spawns **before** its Longhouse prep (verified by the
source-ordering test `longhouse_turn_preparation_call_sites_and_order_are_exact`,
`spawn < permit < prep`); `agent_turn` was the outlier.

### Fixes shipped

Commit `17964f9` (daemon):
- `agent_turn` is now a thin wrapper: the whole admission pipeline runs in
  its own spawned task (`agent_turn_admission`) and the response is relayed
  over a oneshot. A disconnect can no longer cancel a submitted turn at any
  point — it only loses the ACK bytes. HTTP contract unchanged (all six
  existing agent_turn tests pass unmodified).
- Intake session hydration, intake project/git resolution, and the
  `GET /v1/agent/sessions` archive scan moved to `spawn_blocking`.
- Regression test `agent_turn_survives_request_future_drop`: polls the
  handler future once, drops it, and asserts the turn still reaches a
  lifecycle `TurnFinished` with its permit released (times out pre-fix).

Commit `33f6275` (heartbeat, pre-authorized):
- Wake client timeout 30s → 300s default, overridable via
  `OCEAN_HEARTBEAT_TIMEOUT_SECS` (positive integer seconds; zero/garbage
  falls back). Unit-tested via a pure seam.

Commit `96baca2` (agent): OAuth single-flight skip + outer deadline (Bug A
mechanism 1, above).

---

## What remains (recommended, not patched here)

1. **`session::list` needs a summary index.** `spawn_blocking` stops the
   worker pinning, but each poll still parses the whole archive. A sidecar
   metadata file (id, title, updated_ms, workspace_root, count) maintained
   on save would make listing O(archive entries), not O(archive bytes).
2. **Checkpoint saves** (`ocean-agent/src/lib.rs:2294-2307`) still
   serialize + double-fsync the full session on a runtime worker after
   every tool round. Move to `spawn_blocking` (careful: saves must stay
   ordered per session — the session lease already serializes turns) or an
   append-style journal.
3. **Permission-gate deadline** for headless surfaces: a configurable
   timeout → deny, so unattended seats can't park forever
   (`main.rs:2475-2501`).
4. Sync loads still on workers in lower-traffic handlers:
   `GET /v1/agent/sessions/{id}`, `agent_sessions_create`'s project
   resolution, `/v1/requests` intake blocking steps.
5. **Forensics to settle Bug A's trigger on the Mini** (next freeze):
   a full symbolicated `sample`/`spindump` looking for one thread in
   `read`/`fsync`/`waitpid`/`write`; `lsof -p` on the daemon's fd 1/2 (is
   stdout a pipe, and is it full?); confirm `auth.json`, the session dir,
   and every seat cwd are on local APFS, not a FUSE/Drive mount; check
   whether the freeze onset aligns with the 60s OAuth retry cadence.
6. Consider `tokio-metrics` (or `--cfg tokio_unstable` runtime metrics) on
   the fleet build to alarm on worker-poll stalls before they become
   freezes.
