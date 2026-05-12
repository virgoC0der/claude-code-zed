# Design: peer-process cwd discovery for accurate per-client routing

## Context

The just-shipped `session-routing` change made every `at_mentioned`
notification route to exactly one WebSocket client, chosen by matching
the IPC frame's `workspace_root` against the registry's per-client
`workspace_root` values. The matching logic is correct; the problem is
that **the registry's `workspace_root` for every Claude client is
`$HOME`** under the recommended LaunchAgent deployment, because none of
the priority-1 / priority-2 sources actually fire against Claude CLI
v2.1.76, and priority 3 (the daemon's `--workspace` flag) is `$HOME`
under that deployment.

Pre-research (recorded in the team-lead's planning notes and verified
against the live log on the user's machine, summarised in
`proposal.md`):

- macOS exposes no direct "given an accepted TCP loopback socket, give
  me the peer PID" API (`LOCAL_PEERPID` is Unix-domain-socket-only;
  there is no documented TCP equivalent).
- Apple DTS's documented workaround is to enumerate PIDs with
  `proc_listpids`, walk each PID's fd table with
  `proc_pidfdinfo(_, PROC_PIDFDSOCKETINFO)`, and match the entry whose
  socket peer ephemeral port equals the value we're looking up.
- Once we have the PID, `proc_pidinfo(_, PROC_PIDVNODEPATHINFO, ...)`
  returns the vnode path of that process's cwd (also a documented API,
  stable across macOS 14/15, no entitlement required for same-UID
  targets).
- Cost: at most a few milliseconds on a 200-process system —
  enumeration is O(N_processes × N_fds_per_process). One-shot per
  WebSocket accept; the accept loop is already an `await` point.
- Rust wrappers: the `libproc` crate (current stable `0.14.11`) wraps
  both `proc_listpids` and `proc_pidinfo` behind safe Rust. Its
  `pidcwd()` convenience is Linux-only on macOS, so we call
  `pidinfo::<libproc::proc_pid::VNodePathInfo>(pid, 0)` directly per
  the crate's documented surface.
- The `netstat2` crate offers a higher-level "port → PID" lookup;
  rejected here because it pulls in extra dependencies for protocols
  we don't need and the libproc walk is short.

The peer port we look up is the **remote** side of the accepted TCP
socket — i.e. `stream.peer_addr().unwrap().port()`. The local side is
the sidecar's listener port; that's not what we want.

## Goals / Non-Goals

**Goals**

- Eliminate the live-machine bug where every at-mention drops because
  every client claims `$HOME` as its workspace.
- Insert the peer-cwd source at the correct priority (immediately after
  the WebSocket header, before `clientInfo.cwd`) so the existing
  priority semantics are preserved.
- Keep the new code testable without invoking `libproc` — production
  uses the libproc-backed resolver; tests use a deterministic mock.
- Keep the accept loop responsive: `libproc` is sync, so it must run
  inside `tokio::task::spawn_blocking`.
- Maintain wire compatibility — no JSON-RPC, MCP, or IPC frame changes.
- Keep our code `unsafe`-free.

**Non-Goals**

- We do not aim to ship a Linux peer-cwd resolver in v1. Linux is
  best-effort (`NoopCwdResolver` returns `None`), which preserves
  today's behaviour for Linux users — they continue to rely on the
  picker fallback. A future change can add `/proc/$pid/net/tcp` +
  `/proc/$pid/cwd` scanning.
- We do not aim to follow `chdir`s mid-session. Resolving once at
  connect is sufficient — if the user `cd`s in their Claude terminal
  mid-session, the registry entry stays at the connect-time cwd. This
  matches the behaviour every other priority signal has, and the user
  has accepted this in pre-research.
- We do not aim to handle cross-UID or sandboxed targets specially.
  Claude CLI runs as the same UID as the sidecar in every supported
  deployment; libproc has full access in that case.
- We do not aim to replace `--workspace` daemon fallback. It survives
  as the priority-4 last resort, exactly as today.

## Decisions

### D1 — `CwdResolver` trait

**Decision.** Introduce a single async trait in `transport/`:

```rust
#[async_trait::async_trait]
pub trait CwdResolver: Send + Sync + std::fmt::Debug {
    /// Resolve the cwd of the process that owns the peer end of an
    /// accepted TCP loopback socket. Returns `None` on any failure
    /// (process gone, permission denied, empty cwd, platform
    /// unsupported). MUST NOT panic; MUST NOT block the caller's
    /// async runtime for more than ~10 ms even on busy systems.
    async fn resolve(&self, peer: std::net::SocketAddr) -> Option<std::path::PathBuf>;
}
```

`Transport` holds an `Arc<dyn CwdResolver>` field, defaulting to the
platform-appropriate production resolver:

- `LibprocCwdResolver` on `target_os = "macos"`.
- `NoopCwdResolver` on every other target.

Tests inject `MockCwdResolver { map: HashMap<u16, PathBuf> }` (keyed
on the peer port, matching the mock's actual local port from
`stream.local_addr().port()` in the test).

The trait is async because:

- It allows future Linux variants (`/proc` scanning) to be naturally
  async via `tokio::fs`.
- It lets the macOS impl use `tokio::task::spawn_blocking` for the
  libproc call so the accept loop never blocks.
- It mirrors the IPC layer's existing async-by-default style.

**Alternatives considered.**

- *Sync trait, `block_in_place` at the call site.* Rejected — leaks
  the runtime concern (multi-thread vs current-thread) to callers and
  is fragile under `flavor = "current_thread"` in tests.
- *Inline the libproc call directly in `handle_connection`.*
  Rejected — untestable without spawning real processes, and forces
  every platform to compile the macOS code paths even with `cfg`
  gating (the trait + impl split keeps the boundary clean).
- *Use an existing crate like `netstat2`.* Rejected per the
  pre-research note: extra deps, no benefit.

### D2 — `LibprocCwdResolver` (macOS)

**Decision.** The macOS implementation lives in
`transport/cwd_resolver.rs` under `#[cfg(target_os = "macos")]`. The
hot path:

```rust
async fn resolve(&self, peer: SocketAddr) -> Option<PathBuf> {
    let peer_port = peer.port();
    let res = tokio::task::spawn_blocking(move || resolve_blocking(peer_port))
        .await
        .ok()
        .flatten();
    res
}

fn resolve_blocking(peer_port: u16) -> Option<PathBuf> {
    use libproc::libproc::proc_pid::{listpids, ProcType, pidinfo, ListFDs};
    use libproc::libproc::file_info::{ListFDs as _, ProcFDType};
    // 1) Enumerate every PID.
    let pids = listpids(ProcType::ProcAllPIDS).ok()?;
    // 2) For each PID, walk fd table looking for a socket whose peer
    //    port == peer_port.
    for pid in pids {
        let fds = match libproc::libproc::proc_pid::listpidinfo::<ListFDs>(pid as i32, 1024) {
            Ok(fds) => fds,
            Err(_) => continue,
        };
        for fd in fds {
            if fd.proc_fdtype != ProcFDType::Socket as u32 {
                continue;
            }
            let info = match libproc::libproc::file_info::pidfdinfo::<libproc::libproc::net_info::SocketFDInfo>(pid as i32, fd.proc_fd) {
                Ok(i) => i,
                Err(_) => continue,
            };
            // SocketFDInfo carries the local + foreign sockaddr_in pair.
            // We want the entry whose FOREIGN port (from the kernel's
            // POV, which is OUR sidecar's listener port) — actually,
            // the peer we got from accept().peer_addr() is the
            // CLIENT-SIDE local port (= the kernel's FOREIGN port for
            // the sidecar's socket; = the kernel's LOCAL port for the
            // CLAUDE process's socket). So in CLAUDE's fd table we
            // match on LOCAL port == peer_port. Document carefully
            // because this is the easy direction to flip.
            if extract_local_port_v4(&info) == Some(peer_port)
                || extract_local_port_v6(&info) == Some(peer_port)
            {
                // Found the owning PID. Fall through to step 3.
                return read_pid_cwd(pid as i32);
            }
        }
    }
    None
}

fn read_pid_cwd(pid: i32) -> Option<PathBuf> {
    use libproc::libproc::proc_pid::pidinfo;
    use libproc::libproc::proc_pid::VNodePathInfo;
    let info: VNodePathInfo = pidinfo::<VNodePathInfo>(pid, 0).ok()?;
    let cwd_bytes = info.pvi_cdir.vip_path;
    // pvi_cdir.vip_path is a fixed C-string buffer; treat as &CStr.
    let s = std::ffi::CStr::from_bytes_until_nul(unsafe_byte_view(&cwd_bytes)).ok()?;
    let path = std::path::PathBuf::from(s.to_str().ok()?);
    if path.as_os_str().is_empty() { None } else { Some(path) }
}
```

> **Correction (post-implementation, 2026-05-11):** the snippet above
> was based on a pre-implementation assumption that libproc 0.14 wraps
> both `pidinfo::<VNodePathInfo>` and the socket-fd union access in
> safe Rust. **It does not.** Inspection of
> `~/.cargo/registry/src/index.crates.io-…/libproc-0.14.11/src/`
> revealed:
>
> 1. No `impl PIDInfo for VNodePathInfo` exists in libproc 0.14.
>    `grep -rn 'impl PIDInfo for' libproc-0.14.11/src/` returns only
>    `BSDInfo`, `TaskAllInfo`, `TaskInfo`, `ThreadInfo`,
>    `WorkQueueInfo`. The `PidInfo::VNodePathInfo(String)` enum
>    variant and the `PidInfoFlavor::VNodePathInfo = 9` constant are
>    reserved but no struct is wrapped.
>    `libproc::libproc::proc_pid::pidcwd(_)` exists but is hard-coded
>    `Err("pidcwd is not implemented for macos")` (proc_pid.rs:563).
> 2. `libproc::libproc::net_info::SocketInfoProto` is declared
>    `pub union` (net_info.rs:196). Accessing `pri_in` or `pri_tcp`
>    requires `unsafe`. `InSIAddr` is also `pub union` (net_info.rs:293).
>    The crate's own doc-test in `file_info.rs:128` uses
>    `unsafe { socket.psi.soi_proto.pri_tcp }`.
>
> **Team-lead decision (recorded in dispatch reply):** authorize two
> narrow `unsafe` blocks in `transport/cwd_resolver.rs` only. The
> `.harness/project.md` rule was amended to allow up to two `unsafe`
> blocks at FFI/POD-union boundaries in that one file. The rest of the
> codebase remains `unsafe`-free.
>
> **The two unsafe sites and their SAFETY invariants:**
>
> - **Site A (FFI call to `proc_pidinfo`):** the libc call
>   `libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0,
>   &mut buf as *mut _ as *mut c_void,
>   std::mem::size_of::<libc::proc_vnodepathinfo>() as i32)`.
>   `libc 0.2` exposes `proc_vnodepathinfo` and
>   `PROC_PIDVNODEPATHINFO` directly, so we don't need to redeclare
>   the struct. SAFETY: `buf` is a stack-allocated, properly-aligned
>   `proc_vnodepathinfo` zero-initialised before the call; `pid` is an
>   `i32`; the flavor constant is a documented stable macOS API
>   (`<sys/proc_info.h>` from macOS 10.5+); the buffer-size argument
>   matches `size_of` of the struct; the return value is checked — on
>   `ret <= 0` we treat the result as `None` and never read the
>   buffer. The kernel either writes the full struct or returns ≤ 0;
>   no partial-write hazard for our purposes.
>
> - **Site B (union variant read on `SocketInfoProto.pri_in`):** to
>   read `insi_lport` we must enter the `SocketInfoProto` union.
>   We first inspect `socket.psi.soi_kind`
>   (`SocketInfoKind::In | Tcp` are the variants we care about); we
>   only enter the union when `soi_kind ∈ {SOCKINFO_IN, SOCKINFO_TCP}`.
>   SAFETY: the kernel guarantees that when `soi_kind` is set to one
>   of these values, the corresponding union variant is initialised
>   (this is the contract `proc_pidfdinfo` exposes — see
>   `<sys/proc_info.h>` `struct socket_fdinfo`). Reading the matching
>   variant is therefore reading a fully-initialised POD struct. We
>   read **only** the `insi_lport: c_int` field (and from `TcpSockInfo`,
>   only `tcpsi_ini.insi_lport`); we never touch `insi_laddr` (which
>   would require entering another union, `InSIAddr`, and we don't
>   need the IP because everything is `127.0.0.1` loopback).
>
> Both `unsafe` blocks are tightly scoped to a single expression and
> carry a verbatim `// SAFETY: ...` comment in the code matching the
> invariants above. No other `unsafe` in the project.

**Why `spawn_blocking`.** `proc_listpids` and `proc_pidinfo` are
synchronous syscalls. On a 300-process Mac the walk is fast (<10 ms in
practice), but we still wrap them in `spawn_blocking` so:

- the accept loop stays runnable for the next incoming connection,
- we don't surprise a `flavor = "current_thread"` test runtime,
- and any future libproc cost increase doesn't degrade WebSocket
  responsiveness.

**Error handling.**

- Any libproc call failing → return `None`. No `?`-bubbling out of the
  resolver; failures fall through to the next priority.
- The PID may have exited between the listpids enumeration and the
  fdinfo read. That's fine — we continue to the next PID.
- If we found the PID but `pidinfo::<VNodePathInfo>` returns an empty
  cwd (extremely rare — would require the directory to have been
  `rmdir`'d while still being the process's cwd) → return `None`.
  Logged at DEBUG, not WARN — this is a benign edge case.

**Caching.** None for v1. The resolver is invoked once per accept;
results are not shared between accepts. (A long-running interactive
shell could chdir, and we explicitly accept this trade-off; see
Non-Goals.)

### D3 — `NoopCwdResolver`

**Decision.** Lives in the same module under
`#[cfg(not(target_os = "macos"))]`. Its `resolve(..)` always returns
`None`. No allocations, no logging beyond a one-shot DEBUG at startup
("peer-cwd discovery unavailable on this platform; falling back to
existing priority chain"). This is a deliberate design — Linux users
keep today's behaviour, no surprises.

### D4 — `MockCwdResolver` (test-only)

**Decision.** Lives behind a `#[cfg(test)]` (or, for integration
tests, behind a `pub use` in a `tests/` helper module). Constructed
from `HashMap<u16, PathBuf>` where the key is the peer port that the
test's WebSocket client will be assigned.

The test obtains the peer port deterministically:

```rust
let stream = TcpStream::connect(server_addr).await?;
let peer_local_port = stream.local_addr()?.port();
// teach the mock about this port before kicking off the handshake.
mock.insert(peer_local_port, expected_cwd);
```

This is the pattern the new integration test uses — see `tests/peer_cwd_discovery.rs`.

### D5 — Priority insertion in the workspace identification chain

**Decision.** The 4-source chain becomes:

| Priority | Source                                 | Where applied                  | Log source tag         |
| -------- | -------------------------------------- | ------------------------------ | ---------------------- |
| 1        | `x-claude-code-workspace` header       | `handle_connection` callback   | `"header"`             |
| 2 (NEW)  | Peer-process cwd via `CwdResolver`     | `handle_connection`, post-auth | `"peer-cwd-libproc"`   |
| 3        | `clientInfo.cwd` in MCP `initialize`   | `dispatch_text`                | `"client-info-cwd"`    |
| 4        | `--workspace` daemon fallback          | `dispatch_text` (lazy fallback)| `"daemon-workspace"`   |

Priority 2 runs **after** the auth check (no point resolving cwd for a
rejected client) and **before** `serve_authorized` registers the
client. The header (priority 1) wins if set; otherwise the resolver's
result, if `Some(_)`, is canonicalised and assigned to
`initial_workspace`.

`dispatch_text`'s existing "if current is None, try priority N"
ladder is updated minimally: when priority 2 has populated the value
at accept time, `current.is_some()` becomes true and the dispatch_text
branches stay no-op. When priority 2 returned `None` (resolver failed,
or Noop on Linux), priority 3 and 4 retain today's behaviour.

### D6 — `Transport` builder shape

**Decision.** Today the crate exposes `Transport::new(auth, state)`
and `Transport::with_daemon_workspace(auth, state, daemon_workspace)`.
We introduce a builder method:

```rust
impl Transport {
    pub fn new(auth: AuthToken, state: SharedEditorState) -> Self {
        Self::builder(auth, state).build()
    }

    pub fn with_daemon_workspace(
        auth: AuthToken,
        state: SharedEditorState,
        daemon_workspace: PathBuf,
    ) -> Self {
        Self::builder(auth, state)
            .with_daemon_workspace(daemon_workspace)
            .build()
    }

    pub fn builder(auth: AuthToken, state: SharedEditorState) -> TransportBuilder { ... }
}

pub struct TransportBuilder { ... }

impl TransportBuilder {
    pub fn with_daemon_workspace(self, p: PathBuf) -> Self { ... }
    pub fn with_cwd_resolver(self, r: Arc<dyn CwdResolver>) -> Self { ... }
    pub fn build(self) -> Transport { ... }
}
```

The two original constructors keep their signatures and behaviour
(they pick the platform default resolver via `default_resolver()`),
so the existing handshake / session-routing tests don't have to
change. Tests that want injection use `Transport::builder(...).with_cwd_resolver(mock).build()`.

**Alternatives considered.**

- *Add `Transport::with_resolver(...)`.* Rejected — would require a
  matrix of `with_*` constructors for every combination of
  daemon-workspace × resolver. The builder is cleaner.
- *Make `cwd_resolver` always-`Some(Arc::new(NoopCwdResolver))` and
  exposed only as a field.* Rejected — the build-time platform
  default `LibprocCwdResolver` cannot be a field literal; we need a
  function call. A builder method is the natural place.

### D7 — Logging surface

**Decision.** The existing INFO log line in `serve_authorized`:

```rust
info!(
    client_id = %id,
    workspace = ?initial_workspace,
    workspace_source = if initial_workspace.is_some() { "header" } else { "pending-initialize" },
    "authorized websocket client registered"
);
```

becomes a function that takes the source tag as an argument, so
priority 1 logs `"header"`, priority 2 logs `"peer-cwd-libproc"`,
and a still-`None` value logs `"pending-initialize"` (priority 3 / 4
will be tagged from inside `dispatch_text` as today).

The DEBUG log inside `dispatch_text` already records the priority
number that fired. We update its wording so each branch carries an
explicit `workspace_source` field that grep-greps cleanly:

```rust
debug!(
    client_id = %id,
    workspace = %canonical.display(),
    workspace_source = "client-info-cwd",
    "captured workspace_root (priority 3)"
);
```

This is intentionally compatible with the existing field names — the
session-routing change already standardised on `workspace_source` —
so dashboards / log searches keep working.

### D8 — Failure modes & timeouts

**Decision (revised per team-lead OQ2, 2026-05-11).** The resolver
call from `handle_connection` is wrapped in
`tokio::time::timeout(Duration::from_millis(PEER_CWD_RESOLVER_TIMEOUT_MS), …)`
where the constant is **250 ms**, defined in `transport/ws.rs` next
to the existing `MIN_PORT`/`MAX_PORT` constants.

Rationale:

- The resolver IS the expected path. macOS libproc enumeration
  typically completes in < 10 ms on a 300-process Mac. We expect
  the timeout to fire approximately never in production.
- The timeout exists as a **safety net** for the case where libproc
  enumeration unexpectedly stalls (huge process tree from CI fork
  bombs, kernel slowness on busy systems, future libproc cost
  increases). Without the cap, a runaway enumeration would hold
  this connection's `serve_authorized` step open until the kernel
  eventually returned — and while the *global* accept loop is
  unaffected (each connection runs on its own task), the *per-
  connection* WebSocket handshake would visibly stall.
- On timeout we log a single WARN with `peer` and the timeout
  budget, then fall through to priority 3 (`clientInfo.cwd`) /
  priority 4 (`--workspace`). The WebSocket connection stays
  open — this is a graceful-degradation path, not a fatal error.
- The blocking thread spawned via `spawn_blocking` does NOT receive
  a cancellation signal — `JoinHandle` cancellation is cooperative.
  The blocking thread runs to completion and its result is silently
  dropped. This is the correct behaviour for a best-effort signal:
  we don't want to abort an in-flight syscall, and the next accept
  will spawn a fresh one.

Other failure modes (PID exited mid-walk, permission denied,
empty cwd) — all return `None` from the resolver, the same
fall-through path applies. See D2 for the resolver-internal error
handling.

### D9 — Test strategy

**Decision (revised post-implementation).** Four layers of test
coverage. Layer 1 was extended during task #8 to cover the macOS
implementation as well, since `LibprocCwdResolver` can be exercised
against the *current* process's own listener without spawning a
subprocess. Layer 2 was extended during task #10 to cover the
250 ms timeout fallthrough path (per OQ2):

1. **Unit tests in `transport/cwd_resolver.rs`** for `MockCwdResolver`
   (round-trip a HashMap of port → PathBuf), `NoopCwdResolver`
   always-None behaviour, AND macOS-gated `LibprocCwdResolver`
   tests that exercise the real libproc / libc FFI surface against
   `std::process::id()` itself (no subprocess needed — the test
   binds an in-process `TcpListener` and asks the resolver to
   identify the owner). These tests run only on macOS CI; the
   `#[cfg(target_os = "macos")] mod macos_impl::tests` gating
   ensures Linux CI is unaffected.

2. **Integration test `tests/peer_cwd_discovery.rs`** wiring a
   `MockCwdResolver` into the `Transport` via the new builder. Opens
   one and two WebSocket connections (no header), asserts the
   registry's `workspace_root` matches the mock's mapping for each
   client's peer port. This exercises the full
   `handle_connection` → resolver → `serve_authorized` →
   `registry.insert` chain end-to-end.

3. **`tests/session_routing.rs` regression test** that demonstrates
   the original bug is fixed: two WebSocket clients connect with no
   header, no `clientInfo.cwd`, and the mock resolver returns
   distinct cwds for the two clients. An IPC `at_mention` for one of
   the cwds routes to exactly that client, with the other receiving
   nothing within 500 ms.

4. **No `libproc` smoke test in CI.** We rely on:
   - the pre-research that confirmed libproc works on macOS 14/15,
     Apple Silicon, no entitlements;
   - the production smoke test the user runs locally (see
     `manual-smoke-procedure.md` below).

## Risks / Trade-offs

- **PID enumeration cost.** On a quiet Mac (~200 procs) the walk is
  <10 ms. On a CI runner with thousands of forked test processes it
  might be ~50 ms. Acceptable — we accept connections, we're not in a
  latency-critical path. Mitigation: documented in design, monitored
  via the existing `tracing` instrumentation at DEBUG.

- **TOCTOU between connect and cwd read.** If the Claude CLI `chdir`s
  in the ~10 ms between accept and `pidinfo`, we'd capture the new
  cwd. This is a non-issue in practice (Claude doesn't chdir during
  startup), and even if it did, the user's IPC frame carries the
  current cwd at the moment they pressed `cmd-ctrl-c` — the
  registry's cwd matters only at routing time. Documented in
  Non-Goals.

- **PID-collision in long-lived sidecar.** Across 64-bit PID space
  this is astronomically rare. Even if a PID has been recycled, we'd
  produce a stale cwd — at worst a routing miss. Same blast radius
  as today's bug; v1 doesn't address PID recycling.

- **`libproc` upstream churn.** The crate is at 0.14 (post-1.0 in
  spirit; the API has been stable since ~2022 per the changelog).
  Pinning to `0.14` (any 0.14.x patch) is appropriate: the major
  Apple APIs are decades old, so the surface won't change. If
  `libproc` cuts 0.15 with API breakage, we'll pin to the last 0.14
  and revisit.

- **Linux remains best-effort.** Acceptable for v1 because:
  - Linux users without LaunchAgent have a working single-cwd
    `--workspace` setup;
  - the picker fallback still works for multi-Claude on Linux;
  - the trait is designed to drop in a `ProcCwdResolver` later.

- **Operator confusion if the libproc walk legitimately returns
  `None`.** Could happen for a Claude session run under `sudo` (UID
  mismatch — libproc returns EPERM). The log includes
  `workspace_source = "client-info-cwd"` or `"daemon-workspace"` so
  the operator can see which fallback fired; documented in the
  troubleshooting paragraph in `README.md`.

## Migration Plan

1. Land the change in a single binary release.
2. Users `cargo install --path crates/zed-claude-bridge` per README.
3. LaunchAgent picks up the new binary on next login OR via
   `launchctl kickstart -k gui/$(id -u)/com.virgoC0der.zed-claude-bridge`.
4. No plist / no .zed/tasks.json / no README config changes needed
   for users — the change is fully internal to the sidecar.

Rollback: revert the binary. Wire-compat is preserved; old IPC
clients and old Claude CLIs both keep working.

## Open Questions

- **OQ1.** `libproc 0.14` vs `0.15`. The latest stable at planning
  time is `0.14.11`. If by implementation time `0.15` exists, the
  implementer should pin to the latest 0.14 unless 0.15 brings a
  documented bug fix relevant to `proc_pidfdinfo` / `VNodePathInfo`.
  Default decision: `libproc = "0.14"`. Rationale: stable, current,
  and pin-to-minor is the standard for system-FFI crates.

- **OQ2.** Should we add an INFO log line when the resolver returns
  `None` and we fall through to a later priority? Default is **yes**,
  at DEBUG (the priority-3/4 logs already say "applied workspace_root
  from <source> (priority N)"), with a new DEBUG line "peer-cwd
  resolution returned None" emitted by the resolver itself. The
  implementer should add it in the LibprocCwdResolver and skip it in
  the Noop variant (where the absence is constant).

- **OQ3.** Should the Noop variant log a one-shot warning at startup
  on non-macOS so Linux users know what's missing? Default is **no**
  for v1 — Linux without peer-cwd produces the same behaviour as the
  pre-change sidecar (`--workspace` fallback fires for everyone), so
  there's nothing user-actionable to warn about. The README's
  "Multiple Claude sessions" section can mention "macOS-only" in a
  parenthetical.
