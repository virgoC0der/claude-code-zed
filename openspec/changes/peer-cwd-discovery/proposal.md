# Proposal: Discover each Claude client's cwd from its peer process

## Why

The `session-routing` change shipped multi-client routing based on a
3-priority chain for the registry entry's `workspace_root`:

1. `x-claude-code-workspace` WebSocket request header,
2. `clientInfo.cwd` in the MCP `initialize` request,
3. the sidecar's `--workspace` daemon fallback.

In real-world deployment the **field reality is bug-causing**:

- The Claude Code CLI v2.1.76 does not emit (1) — see the audit recorded
  in `openspec/changes/session-routing/tasks.md` §0.1 (extension.js
  v2.1.76 inspects only `x-claude-code-ide-authorization`).
- It also does not emit (2) — Claude's `initialize` carries only
  `protocolVersion` / `capabilities` / `clientInfo = {name, version}`;
  `cwd` is absent.
- (3) is the sidecar's own `--workspace` flag. In the recommended
  LaunchAgent deployment (`scripts/com.virgoC0der.zed-claude-bridge.plist`)
  it is set to `$HOME`, so every connected client ends up with
  `workspace_root = /Users/<user>`.

The user's live sidecar log from
`~/Library/Logs/zed-claude-bridge.log` confirms the symptom:

```
applied workspace_root from daemon --workspace fallback (priority 3) client_id=44a79a8c… workspace=/Users/sx.chen
no matching client; dropping at_mention … workspace_root_canonical=Some("/Users/sx.chen/Code/personal/claude-code-zed") known_workspaces=["/Users/sx.chen"]
```

Routing **drops every at-mention** whenever the user has more than one
Claude session, because every connected client claims `$HOME` as its
workspace and the IPC frame's `workspace_root` is the actual project
path. The router lands on rule 5 ("no match") and logs the WARN above.

The fix the user has approved: **derive each Claude client's `cwd` from
its peer process** at WebSocket-accept time, before the daemon
`--workspace` fallback. macOS exposes this reliably via a documented
`libproc` enumeration; the sidecar already has the accepted TCP
socket's peer port, which is the input we need.

## What Changes

- **NEW (`websocket` capability):** a per-client `workspace_root`
  resolution step inserted **between** the `x-claude-code-workspace`
  header (priority 1) and `clientInfo.cwd` (was priority 2). The new
  step's source is the **cwd of the OS process that owns the peer end
  of the accepted TCP loopback socket**.

  Resolved priority chain (post-change):
  1. `x-claude-code-workspace` request header. (Unchanged; dead branch
     against Claude CLI v2.1.76 but kept for forward compatibility.)
  2. **NEW** peer-process cwd, discovered via `libproc` on macOS.
  3. `clientInfo.cwd` from the MCP `initialize` request. (Unchanged;
     also dead against Claude CLI v2.1.76.)
  4. The sidecar's `--workspace` daemon fallback. (Unchanged.)

  The resolved value is recorded in the registry entry's
  `workspace_root` (canonicalized) AND a new
  `workspace_source: &'static str` tag is logged at INFO when the
  client is registered, so an operator can grep the log and see
  `workspace_source="peer-cwd-libproc"`.

- **NEW (internal):** a `CwdResolver` trait in `transport/`:

  ```rust
  #[async_trait]
  pub trait CwdResolver: Send + Sync {
      async fn resolve(&self, peer: SocketAddr) -> Option<PathBuf>;
  }
  ```

  Plus two production implementations:

  - `LibprocCwdResolver` on macOS — uses `proc_listpids`,
    `proc_pidfdinfo(_, PROC_PIDFDSOCKETINFO)` to find the PID whose
    socket fd-table contains an entry with the given peer ephemeral
    port; then `proc_pidinfo(_, PROC_PIDVNODEPATHINFO, ...)` to read
    that PID's cwd. Wrapped in `tokio::task::spawn_blocking` so the
    accept loop is never stalled.
  - `NoopCwdResolver` on every other platform (Linux, Windows) —
    returns `None`. A real Linux implementation (TCP /proc scan +
    `/proc/<pid>/cwd`) is tracked as a follow-up; v1 ships macOS only.

  The `Transport` keeps a `Arc<dyn CwdResolver>` field. Production
  uses `LibprocCwdResolver` on macOS, `NoopCwdResolver` elsewhere. Tests
  use a deterministic `MockCwdResolver` whose `resolve()` returns a
  scripted `PathBuf` for the given peer `SocketAddr`.

- **NEW dependency:** `libproc = "0.14"` (pinned to the 0.14 minor;
  current stable 0.14.11). macOS-only — gated by a `cfg(target_os =
  "macos")` block in `Cargo.toml`. The crate uses `unsafe` internally
  to call the macOS `libproc.dylib`; **our own code stays `unsafe`-free**
  per the project's architectural rule.

- **NEW logging field:** the existing
  `"authorized websocket client registered"` INFO log gains a
  `workspace_source` field whose value is one of:
  - `"header"` — supplied by `x-claude-code-workspace`,
  - **`"peer-cwd-libproc"`** — NEW, supplied by the resolver,
  - `"client-info-cwd"` — supplied by `clientInfo.cwd`,
  - `"daemon-workspace"` — supplied by `--workspace`,
  - `"pending-initialize"` — none of the above yet; awaiting MCP
    `initialize` (rare in practice once peer-cwd is in place).

  The existing DEBUG log on `set_workspace` calls is augmented with
  the priority number that fired, mirroring the existing
  `"applied workspace_root from daemon --workspace fallback (priority 3)"`
  format.

- **No on-the-wire changes.** `protocol/` is untouched. The
  `IpcFrame`, JSON-RPC, and lock-file shapes do not move. Helpers,
  Zed task config, README "Multiple Claude sessions" instructions all
  continue to work as documented.

## Capabilities

### New Capabilities

(none — this change modifies one existing capability and adds one
internal trait; no new wire surface.)

### Modified Capabilities

- `websocket`: extends the **Workspace identification on connect**
  requirement's priority chain from 3 entries to 4 (inserts the new
  peer-cwd source at priority 2). Adds an **ADDED** requirement
  **Peer-process cwd discovery** that nails down the libproc-backed
  source's contract (success, failure, platform availability,
  cancellability).

## Impact

- **Affected source files (project-relative):**
  - `crates/zed-claude-bridge/Cargo.toml` — add `libproc = "0.14"`
    under a `[target.'cfg(target_os = "macos")'.dependencies]` block;
    no new workspace features needed.
  - `crates/zed-claude-bridge/src/transport/cwd_resolver.rs` — NEW
    module defining the `CwdResolver` trait, `LibprocCwdResolver`
    (macOS), `NoopCwdResolver` (other platforms), and the test-only
    `MockCwdResolver`.
  - `crates/zed-claude-bridge/src/transport/mod.rs` — re-export
    `CwdResolver`, `LibprocCwdResolver`, `NoopCwdResolver`.
  - `crates/zed-claude-bridge/src/transport/ws.rs`:
    - Replace `Transport::new` / `Transport::with_daemon_workspace`
      with a single builder that accepts an
      `Arc<dyn CwdResolver>`, keeping the daemon-workspace as an
      optional field. Existing tests continue to call the simpler
      constructors via a default-resolver shim.
    - In `handle_connection`, capture the **peer** `SocketAddr`
      returned by `listener.accept()` and pass it down with the
      stream. After auth succeeds and BEFORE
      `serve_authorized` runs, call
      `cwd_resolver.resolve(peer).await`. If the result is
      `Some(p)`, canonicalise and set `initial_workspace =
      Some(canonical(p))` with `source = "peer-cwd-libproc"` unless
      the header already provided a value (header wins per priority 1).
    - In `serve_authorized`, the INFO log gains a
      `workspace_source` field carrying the priority that fired.
    - In `dispatch_text`, the priority-3 (clientInfo.cwd) and
      priority-4 (daemon-workspace) branches are unchanged
      semantically; the lazy-application logic just falls through
      when peer-cwd already set the value at accept time.
  - `crates/zed-claude-bridge/src/app/lifecycle.rs` — `run_daemon`
    constructs the production resolver (`LibprocCwdResolver` on
    macOS, `NoopCwdResolver` elsewhere) and threads it into
    `Transport`.
  - `crates/zed-claude-bridge/tests/handshake.rs` — verify the
    existing handshake tests still pass with the default
    `NoopCwdResolver` (they don't depend on peer-cwd discovery).
  - `crates/zed-claude-bridge/tests/peer_cwd_discovery.rs` — NEW
    integration test. Builds a `Transport` with a `MockCwdResolver`
    that maps the test client's actual local port to a scripted
    `PathBuf`. Opens an authorized WebSocket connection without any
    workspace header. Asserts via the registry snapshot that the
    client's `workspace_root` is the mock's scripted path and
    `workspace_source` (observable via log capture) is
    `"peer-cwd-libproc"` or the test's chosen source-tag value.
  - `crates/zed-claude-bridge/tests/session_routing.rs` — add ONE
    new test demonstrating the original bug is fixed end-to-end with
    the mock resolver: two clients whose mock-resolved cwds differ
    each receive only their own at-mention, even when neither
    supplies a workspace header.
- **Dependencies:** one new crate, `libproc = "0.14"`, macOS-only.
  No other workspace changes.
- **No protocol break.** Lock-file JSON, JSON-RPC envelope, MCP
  handshake, IPC frame shape — all unchanged.
- **Observable field behaviour changes** (intended): with the
  LaunchAgent deployment, multi-Claude-session workflows that were
  previously dropping every at-mention will now succeed without a
  picker round-trip — peer-cwd disambiguates by cwd.
- **No `unsafe` in our code.** `libproc` itself uses `unsafe` to call
  out to macOS APIs; that lives inside the external crate and does
  not violate our architectural rule.
- **Deployment story:** users running the LaunchAgent need to
  `cargo install --path crates/zed-claude-bridge` and either
  `launchctl kickstart -k gui/$(id -u)/com.virgoC0der.zed-claude-bridge`
  or relog. No new entitlements; no plist changes.
- **Linux (best-effort, follow-up):** Linux ships with the
  `NoopCwdResolver` for v1 — same behaviour as today. A future change
  can add `/proc/$pid/net/tcp` + `/proc/$pid/cwd` scanning; the trait
  surface is already designed to swap in.
