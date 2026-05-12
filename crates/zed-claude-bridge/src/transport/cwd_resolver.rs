//! Peer-process cwd discovery — abstraction layer for resolving the
//! current working directory of the OS process that owns the peer end
//! of an accepted TCP loopback socket.
//!
//! Layer position: this module lives in layer 4 (`transport/`). It
//! depends only on the standard library plus `tokio` for runtime
//! primitives. It does NOT depend on `mcp`, `ipc`, or `app`.
//!
//! ## `unsafe` exemption
//!
//! This is the ONLY file in the project permitted to use `unsafe`
//! blocks, per the exception recorded in `.harness/project.md` and
//! authorised by the team-lead in response to the task #8 blocker
//! report (libproc 0.14 does not provide safe wrappers for
//! `VNodePathInfo` or the `SocketInfoProto`/`InSIAddr` unions). The
//! file carries a module-level `#![allow(unsafe_code)]` because the
//! workspace `[workspace.lints.rust] unsafe_code = "deny"` would
//! otherwise reject any `unsafe` block.
//!
//! There are exactly **two `unsafe` sites** in this file, both inside
//! `mod macos_impl`:
//!
//! 1. Reading the `pri_tcp` variant of the `SocketInfoProto` union
//!    after checking `soi_kind == SOCKINFO_TCP` (function
//!    `socket_local_port_matches`).
//! 2. Calling `libc::proc_pidinfo(_, PROC_PIDVNODEPATHINFO, _, _)`
//!    on a stack-allocated `MaybeUninit<proc_vnodepathinfo>` AND the
//!    paired `MaybeUninit::assume_init()` after the kernel reports
//!    success (function `read_pid_cwd`).
//!
//! See design.md D2 for the rationale and SAFETY invariants. Adding
//! any other `unsafe` block requires the team-lead's approval AND an
//! update to `.harness/project.md`.
//!
//! ## Why this exists
//!
//! The Claude Code CLI v2.1.76 does not emit the `x-claude-code-workspace`
//! request header and does not include a `cwd` in its `clientInfo`
//! object. Without an independent signal, every Claude session shares
//! the sidecar's `--workspace` daemon fallback as its workspace, which
//! collapses multi-session routing. This trait is the independent
//! signal: at WebSocket-accept time, after auth, the transport asks
//! the configured resolver for the peer process's cwd.
//!
//! See `openspec/changes/peer-cwd-discovery/specs/websocket/spec.md`
//! → **Peer-process cwd discovery** for the authoritative contract.
//!
//! ## Trait shape choice
//!
//! Per the team-lead decision OQ3 the trait avoids the `async_trait`
//! crate. We first tried **native async-fn-in-trait** (Rust 1.85 /
//! edition 2024 supports it without macros), but the resulting trait
//! is NOT `dyn`-compatible: the compiler reports E0038 because the
//! method's `impl Future` return type cannot be entered into a
//! vtable. We need `Arc<dyn CwdResolver>` (the `Transport` field is a
//! trait object so it can be swapped at construction time without
//! generic-explosion across `Transport`, `TransportBuilder`,
//! `handle_connection`, and the per-connection state). Falling back to
//! the explicit boxed-future form per rustc's E0038 hint:
//!
//! ```ignore
//! fn resolve<'a>(&'a self, peer: SocketAddr)
//!     -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PathBuf>> + Send + 'a>>;
//! ```
//!
//! This is what `#[async_trait::async_trait]` would have generated;
//! we just write it by hand to keep the `async-trait` crate off the
//! dependency list (team-lead OQ3 decision).

// Module-scoped exemption from the workspace `unsafe_code = "deny"`
// lint. The exemption is justified at the file level above; do not
// remove without team-lead sign-off.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

/// Boxed future returned by [`CwdResolver::resolve`]. Type alias kept
/// short so the impl signatures stay readable.
pub type BoxResolveFuture<'a> = Pin<Box<dyn Future<Output = Option<PathBuf>> + Send + 'a>>;

/// Resolve the cwd of the OS process that owns the peer end of an
/// accepted TCP loopback socket.
///
/// Production implementations:
///
/// - [`LibprocCwdResolver`] on `target_os = "macos"` — walks the
///   per-process socket fd tables via `libproc` and reads the owning
///   PID's cwd via `proc_pidinfo(_, PROC_PIDVNODEPATHINFO, _, _)`.
///   Wraps the synchronous libproc calls in
///   `tokio::task::spawn_blocking` so the WebSocket accept loop is
///   not stalled.
/// - [`NoopCwdResolver`] on every other `target_os` — always returns
///   `None`. Linux peer-cwd discovery is tracked as a follow-up
///   change; for v1 the trait surface is in place so the resolver
///   can be swapped in without touching call sites.
///
/// Test double:
///
/// - [`MockCwdResolver`] — consults an injected
///   `HashMap<u16, PathBuf>` keyed on the peer port. Tests obtain
///   the port deterministically via
///   `TcpStream::connect(addr).local_addr()?.port()` before kicking
///   off the WebSocket handshake.
///
/// ## Contract (from spec)
///
/// - Implementations MUST NOT panic on any input.
/// - Implementations MUST NOT propagate errors. A resolver failure
///   (process gone, permission denied, empty cwd, platform
///   unsupported) returns `None`, which causes the transport to
///   fall through to the next workspace-identification priority.
/// - Implementations MUST be cheap enough that the accept loop is
///   not visibly stalled. The macOS implementation runs the
///   blocking libproc enumeration inside `spawn_blocking`; the
///   accept loop additionally wraps the `resolve()` call in a
///   250 ms `tokio::time::timeout` as a safety net (see team-lead
///   OQ2 decision).
pub trait CwdResolver: Send + Sync + std::fmt::Debug + 'static {
    /// Resolve the peer's cwd, or `None` if unavailable.
    ///
    /// Returns a boxed `Send`-bounded future so the trait is
    /// `dyn`-compatible when wrapped behind `Arc<dyn CwdResolver>`.
    /// See the module-level doc comment for the rationale.
    fn resolve<'a>(&'a self, peer: SocketAddr) -> BoxResolveFuture<'a>;
}

/// Construct the platform-appropriate default resolver behind an
/// `Arc<dyn CwdResolver>`.
///
/// - macOS: returns an `Arc<LibprocCwdResolver>`.
/// - Other targets: returns an `Arc<NoopCwdResolver>`. The Noop
///   variant deliberately does not log a one-shot warning at
///   startup (team-lead OQ3 decision in design.md); Linux without
///   peer-cwd produces the same behaviour as the pre-change sidecar.
#[cfg(target_os = "macos")]
pub fn default_cwd_resolver() -> Arc<dyn CwdResolver> {
    Arc::new(LibprocCwdResolver::new())
}

/// Non-macOS variant of [`default_cwd_resolver`].
#[cfg(not(target_os = "macos"))]
pub fn default_cwd_resolver() -> Arc<dyn CwdResolver> {
    Arc::new(NoopCwdResolver::new())
}

// ---------------------------------------------------------------------------
// NoopCwdResolver
// ---------------------------------------------------------------------------

/// `CwdResolver` that always returns `None`.
///
/// Used as the platform default on non-macOS targets, and as a unit
/// test double on every target. Cheap — no allocation, no syscalls.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopCwdResolver;

impl NoopCwdResolver {
    /// Construct a fresh `NoopCwdResolver`.
    pub const fn new() -> Self {
        Self
    }
}

impl CwdResolver for NoopCwdResolver {
    fn resolve<'a>(&'a self, _peer: SocketAddr) -> BoxResolveFuture<'a> {
        Box::pin(async { None })
    }
}

// ---------------------------------------------------------------------------
// MockCwdResolver (test double — public for integration tests)
// ---------------------------------------------------------------------------

/// `CwdResolver` whose `resolve` consults an injected
/// `HashMap<u16, PathBuf>` keyed on the peer port.
///
/// Tests build a `MockCwdResolver` and `insert(port, path)` for each
/// port they expect a client to connect from. The port is obtained
/// deterministically by the test via
/// `TcpStream::connect(addr).local_addr()?.port()` before the
/// WebSocket handshake is driven through `tokio_tungstenite`.
///
/// Public (not `#[cfg(test)]`-gated) because integration tests under
/// `crates/zed-claude-bridge/tests/` need to construct it from a
/// downstream compilation unit.
#[derive(Debug, Default, Clone)]
pub struct MockCwdResolver {
    map: HashMap<u16, PathBuf>,
}

impl MockCwdResolver {
    /// Construct an empty `MockCwdResolver`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Map `peer_port` to `path`. Returns the previous mapping if any.
    pub fn insert(&mut self, peer_port: u16, path: PathBuf) -> Option<PathBuf> {
        self.map.insert(peer_port, path)
    }
}

impl CwdResolver for MockCwdResolver {
    fn resolve<'a>(&'a self, peer: SocketAddr) -> BoxResolveFuture<'a> {
        let result = self.map.get(&peer.port()).cloned();
        Box::pin(async move { result })
    }
}

// ---------------------------------------------------------------------------
// LibprocCwdResolver (macOS)
// ---------------------------------------------------------------------------

/// `CwdResolver` backed by macOS `libproc.dylib` (via the `libproc`
/// crate plus two direct `libc` FFI calls).
///
/// Algorithm:
///
/// 1. Enumerate every PID with `libproc::proc_pid::listpids`.
/// 2. For each PID, walk its fd table with
///    `libproc::proc_pid::listpidinfo::<ListFDs>`.
/// 3. For each fd whose type is `ProcFDType::Socket`, fetch a
///    `SocketFDInfo` via `libproc::file_info::pidfdinfo`. Match if the
///    socket's LOCAL port equals `peer_port` (the kernel's perspective
///    on the Claude-side socket — see design D2 footnote about the
///    direction flip).
/// 4. On match, read the PID's cwd via `libc::proc_pidinfo` with
///    `PROC_PIDVNODEPATHINFO`, decode the C-string `pvi_cdir.vip_path`,
///    and return the resulting `PathBuf`.
///
/// Returns `None` on every failure path (no PID matched, libproc
/// errored, libc returned ≤ 0, empty cwd). Never panics.
///
/// The synchronous walk is wrapped in `tokio::task::spawn_blocking`
/// from `resolve`, so the WebSocket accept loop remains async-clean.
///
/// **Unsafe usage.** This file contains two narrow `unsafe` blocks at
/// FFI / kernel-populated POD-union boundaries, authorised by the
/// `.harness/project.md` exception. Each block carries a `// SAFETY:`
/// comment explaining the invariant. See design.md D2 for the
/// security review.
#[cfg(target_os = "macos")]
#[derive(Debug, Default, Clone, Copy)]
pub struct LibprocCwdResolver;

#[cfg(target_os = "macos")]
impl LibprocCwdResolver {
    /// Construct a fresh `LibprocCwdResolver`.
    pub const fn new() -> Self {
        Self
    }
}

#[cfg(target_os = "macos")]
impl CwdResolver for LibprocCwdResolver {
    fn resolve<'a>(&'a self, peer: SocketAddr) -> BoxResolveFuture<'a> {
        let peer_port = peer.port();
        Box::pin(async move {
            // `proc_listpids` / `proc_pidinfo` are synchronous
            // syscalls. Wrap in `spawn_blocking` so the accept loop
            // (driven from `Transport::run`) remains responsive and
            // can accept further TCP connections while the walk is
            // in flight. The accept-loop side additionally wraps
            // this whole call in `tokio::time::timeout(250 ms)` per
            // OQ2 — the spawn_blocking handle is dropped on timeout
            // and the worker thread completes its enumeration
            // independently (no leak; just discarded work).
            match tokio::task::spawn_blocking(move || macos_impl::resolve_blocking(peer_port)).await
            {
                Ok(opt) => opt,
                Err(join_err) => {
                    tracing::debug!(
                        error = %join_err,
                        "spawn_blocking for peer-cwd resolution failed; treating as None"
                    );
                    None
                }
            }
        })
    }
}

#[cfg(target_os = "macos")]
mod macos_impl {
    //! Synchronous, blocking implementation of the libproc-backed
    //! peer-cwd lookup. Two `unsafe` blocks live in this module per
    //! the `.harness/project.md` exception; see the per-block
    //! `// SAFETY:` comments.

    use std::ffi::CStr;
    use std::mem::MaybeUninit;
    use std::path::PathBuf;

    use libproc::libproc::file_info::{ListFDs, ProcFDType, pidfdinfo};
    use libproc::libproc::net_info::{SocketFDInfo, SocketInfoKind};
    use libproc::libproc::proc_pid::listpidinfo;
    use libproc::processes::{ProcFilter, pids_by_type};

    /// Maximum fd-table size we'll try to read per PID. macOS process
    /// fd tables are commonly < 256; 4096 is a generous upper bound
    /// that still keeps stack churn small.
    const MAX_FDS_PER_PID: usize = 4096;

    pub(super) fn resolve_blocking(peer_port: u16) -> Option<PathBuf> {
        let pids = match pids_by_type(ProcFilter::All) {
            Ok(pids) => pids,
            Err(err) => {
                tracing::debug!(error = %err, "pids_by_type(All) failed; peer-cwd resolution returns None");
                return None;
            }
        };

        for raw_pid in pids {
            // PIDs from libproc are u32; macOS `proc_pidinfo` takes
            // `c_int` (i32). PIDs always fit in i32 on macOS.
            let pid = raw_pid as i32;
            if pid <= 0 {
                continue;
            }
            if pid_owns_socket_with_local_port(pid, peer_port) {
                if let Some(cwd) = read_pid_cwd(pid) {
                    tracing::debug!(
                        pid,
                        peer_port,
                        cwd = %cwd.display(),
                        "peer-cwd resolver matched PID"
                    );
                    return Some(cwd);
                } else {
                    tracing::debug!(
                        pid,
                        peer_port,
                        "peer-cwd resolver matched PID but cwd was empty/unreadable; returning None"
                    );
                    return None;
                }
            }
        }

        tracing::debug!(peer_port, "peer-cwd resolver found no matching PID");
        None
    }

    /// Walk `pid`'s fd table and return true iff any socket fd has
    /// a LOCAL port equal to `peer_port` (host byte order). Errors
    /// from libproc are swallowed (return false / try next PID).
    fn pid_owns_socket_with_local_port(pid: i32, peer_port: u16) -> bool {
        let fds = match listpidinfo::<ListFDs>(pid, MAX_FDS_PER_PID) {
            Ok(fds) => fds,
            // Process may have exited between listpids and now, or
            // we may lack permission for cross-UID targets. Either
            // way, skip this PID.
            Err(_) => return false,
        };

        for fd in fds {
            let fd_type: ProcFDType = fd.proc_fdtype.into();
            if !matches!(fd_type, ProcFDType::Socket) {
                continue;
            }
            let socket_info = match pidfdinfo::<SocketFDInfo>(pid, fd.proc_fd) {
                Ok(info) => info,
                Err(_) => continue,
            };
            if socket_local_port_matches(&socket_info, peer_port) {
                return true;
            }
        }
        false
    }

    /// Return true iff `socket_info` describes an IPv4/IPv6 or TCP
    /// socket whose LOCAL port (network byte order from kernel,
    /// converted to host order here) equals `peer_port`.
    ///
    /// This function contains one of the two project-exception
    /// `unsafe` blocks. See the `// SAFETY:` comment for the
    /// invariant.
    fn socket_local_port_matches(socket_info: &SocketFDInfo, peer_port: u16) -> bool {
        // We only care about TCP loopback connections to the sidecar
        // listener. Anything else (raw IP, Unix domain, kernel
        // event, etc.) cannot be a Claude WebSocket peer.
        let kind: SocketInfoKind = socket_info.psi.soi_kind.into();
        if !matches!(kind, SocketInfoKind::Tcp) {
            return false;
        }

        // UNSAFE SITE 1 of 2 in this file.
        //
        // SAFETY: `socket_info.psi.soi_kind` was read immediately
        // above and matched `SocketInfoKind::Tcp` (kernel constant
        // `SOCKINFO_TCP = 2`). The macOS kernel contract
        // (`<sys/proc_info.h>` → `struct socket_fdinfo`) guarantees
        // that when `soi_kind == SOCKINFO_TCP` the
        // `soi_proto.pri_tcp` union variant is fully initialised
        // with a valid `struct tcp_sockinfo`, whose `tcpsi_ini`
        // member is a `struct in_sockinfo` (POD; reading any of its
        // scalar fields is sound once the union variant is
        // initialised). We read **only** the scalar
        // `insi_lport: c_int` field — no pointer dereference, no
        // union further down (we deliberately do NOT touch
        // `insi_laddr`, which would be another union; the IP isn't
        // needed because all sidecar peers are `127.0.0.1` by
        // construction in `bind_random`). The function is
        // semantically `read-only` even though the read goes through
        // a union variant access.
        let raw_lport: i32 = unsafe { socket_info.psi.soi_proto.pri_tcp.tcpsi_ini.insi_lport };

        // `insi_lport` is stored in network byte order (big-endian)
        // — see the libproc 0.14 doc-test in
        // `~/.cargo/registry/.../libproc-0.14.11/src/libproc/file_info.rs`
        // (line ~131-133) which performs the same byte swap. The
        // kernel field is `c_int` but the upper 16 bits are zero
        // (it's a `u16` widened to `int`). Extract the low 16 bits
        // and ntohs them.
        let be: u32 = (raw_lport as u32) & 0xFFFF;
        let lport_host: u16 = (((be >> 8) | (be << 8)) & 0xFFFF) as u16;
        lport_host == peer_port
    }

    /// Read `pid`'s current working directory via
    /// `libc::proc_pidinfo(PROC_PIDVNODEPATHINFO)`.
    ///
    /// Contains the second project-exception `unsafe` block: the
    /// FFI call itself. See `// SAFETY:` comment for the invariant.
    fn read_pid_cwd(pid: i32) -> Option<PathBuf> {
        // Stack-allocated buffer, zero-initialised. We allocate it
        // uninitialised via `MaybeUninit` so we don't pay for the
        // (large; ~2 KiB) zero-fill of a struct we're about to
        // overwrite — but we only ever read the buffer through the
        // `assume_init` path AFTER the kernel has reported success
        // (return value > 0 and equal to the requested size).
        let mut buf: MaybeUninit<libc::proc_vnodepathinfo> = MaybeUninit::uninit();

        // Cast directly to `*mut c_void` for the FFI call. `size_of`
        // is a const expression on a libc-defined `#[repr(C)]` struct
        // so the cast cannot truncate (struct is way under 2 GiB).
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let buf_size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;

        // UNSAFE SITE 2 of 2 in this file.
        //
        // Single fused `unsafe` block: the FFI call to
        // `libc::proc_pidinfo` AND the paired
        // `MaybeUninit::assume_init`. Splitting these two would
        // create a 3rd `unsafe` block beyond the project budget
        // (`.harness/project.md` exemption: limit 2). They share one
        // safety invariant — the kernel either writes the full
        // struct (return value == requested size) or it doesn't, in
        // which case we abort before reading.
        //
        // SAFETY:
        // - `libc::proc_pidinfo` is the documented macOS syscall
        //   wrapper from `<libproc.h>`, stable since macOS 10.5.
        // - `pid` is an `i32` (PID); we filtered `pid > 0` above so
        //   it represents a real process at the time of the call
        //   (the process may exit between then and now — that is a
        //   correctness concern for the kernel, which returns ≤ 0;
        //   it is not a memory-safety hazard for our caller).
        // - The flavor constant `PROC_PIDVNODEPATHINFO = 9` is the
        //   documented stable value in `<sys/proc_info.h>`.
        // - `arg` is unused for this flavor (per Apple docs); 0 is
        //   the documented convention.
        // - The buffer pointer is a properly-aligned, exclusively-
        //   borrowed mutable pointer to a stack-allocated
        //   `MaybeUninit<proc_vnodepathinfo>`. `proc_vnodepathinfo`
        //   is `#[repr(C)]` in `libc 0.2`, matching the kernel's
        //   ABI byte-for-byte.
        // - `buf_size` is `size_of::<proc_vnodepathinfo>()`, the
        //   exact size the kernel expects for this flavor.
        // - The `assume_init` is guarded by `ret == buf_size`: only
        //   when the kernel reports a complete write do we treat the
        //   buffer as initialised. Any other return value drops the
        //   `MaybeUninit` uninitialised (sound) and we return `None`.
        //
        // See design.md D2 Site A.
        let info = unsafe {
            let ret = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf_size,
            );
            if ret != buf_size {
                // Partial write or error. macOS returns 0 for
                // "no info available" (e.g. zombie process) and a
                // negative-on-error contract per `<libproc.h>`.
                // Either way: no cwd. Drop the buffer
                // uninitialised — sound because `MaybeUninit` has
                // no Drop glue that touches the (uninit) bytes.
                return None;
            }
            buf.assume_init()
        };

        // Path-bytes decoding (no `unsafe` from here on).
        //
        // The libc declaration models `vip_path` as a 2D array
        // `[[c_char; 32]; 32]` because of an older-rustc-MSRV
        // workaround (see libc src/unix/bsd/apple/mod.rs:916-918).
        // The actual on-the-wire layout is a contiguous
        // `[c_char; MAXPATHLEN = 1024]` buffer. We flatten the 2D
        // view into a `[u8; 1024]` byte buffer via a `c_char` (i8) →
        // `u8` element-wise mapping, then parse the C-string up to
        // the first NUL byte.

        // Element-wise `i8` → `u8` reinterpretation, keeping the
        // module `unsafe`-free for this step (only the two
        // FFI/union sites above are exempted).
        let path_2d = info.pvi_cdir.vip_path;
        let mut bytes: [u8; 1024] = [0u8; 1024];
        let mut idx = 0;
        'outer: for chunk in path_2d.iter() {
            for &c in chunk.iter() {
                if idx >= bytes.len() {
                    break 'outer;
                }
                bytes[idx] = c as u8;
                idx += 1;
            }
        }

        // Parse as a C-string up to the first NUL.
        let cstr = CStr::from_bytes_until_nul(&bytes).ok()?;
        let s = cstr.to_str().ok()?;
        if s.is_empty() {
            return None;
        }
        let pb = PathBuf::from(s);
        if pb.as_os_str().is_empty() {
            None
        } else {
            Some(pb)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// `read_pid_cwd` against the current process must return
        /// the same path as `std::env::current_dir()`. This is the
        /// only place we exercise real libproc/libc in unit tests;
        /// CI runs this only on macOS (the module is
        /// `#[cfg(target_os = "macos")]`).
        #[test]
        fn read_pid_cwd_of_self_matches_current_dir() {
            let pid = std::process::id() as i32;
            let cwd = read_pid_cwd(pid).expect("self's cwd must be readable");
            let expected = std::env::current_dir().expect("current_dir is readable");
            // Canonicalise both sides so symlinked /var → /private/var
            // doesn't cause a spurious mismatch.
            let canon_cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd.clone());
            let canon_expected = std::fs::canonicalize(&expected).unwrap_or(expected);
            assert_eq!(canon_cwd, canon_expected);
        }

        /// `read_pid_cwd` against a definitely-nonexistent PID must
        /// return `None`, not panic. macOS reserves the high range
        /// of `i32` PIDs; pid `i32::MAX - 1` is essentially
        /// guaranteed to be unused.
        #[test]
        fn read_pid_cwd_of_nonexistent_pid_returns_none() {
            assert_eq!(read_pid_cwd(i32::MAX - 1), None);
        }

        /// `pid_owns_socket_with_local_port` against the current
        /// process must return true when given the local port of a
        /// `TcpListener` we just bound, and false for an arbitrary
        /// unbound port.
        #[test]
        fn pid_owns_socket_for_self_listener() {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral bind must succeed");
            let port = listener.local_addr().expect("addr").port();
            let pid = std::process::id() as i32;
            assert!(
                pid_owns_socket_with_local_port(pid, port),
                "self process must own the just-bound socket at port {port}"
            );
            // A high, unbound port: no socket should own it.
            // (Note: there's a tiny race-window where another
            // process binds this exact port between the two
            // assertions; we deliberately use port 1 which on
            // macOS requires root to bind and is essentially
            // guaranteed to be unbound for our UID.)
            assert!(
                !pid_owns_socket_with_local_port(pid, 1),
                "self process must not own a socket at privileged port 1"
            );
        }

        /// Full `resolve_blocking` end-to-end against an in-process
        /// TCP listener. Mirrors what `LibprocCwdResolver::resolve`
        /// does, minus the `spawn_blocking` indirection.
        #[test]
        fn resolve_blocking_finds_self_listener() {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral bind must succeed");
            let port = listener.local_addr().expect("addr").port();
            let cwd = resolve_blocking(port).expect("our own listener must be found");
            let expected = std::env::current_dir().expect("current_dir");
            let canon_cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd.clone());
            let canon_expected = std::fs::canonicalize(&expected).unwrap_or(expected);
            assert_eq!(canon_cwd, canon_expected);
        }

        /// `resolve_blocking` for a port that nobody owns returns
        /// `None`, not a panic. Port 1 is privileged on macOS and
        /// essentially guaranteed unbound for our UID.
        #[test]
        fn resolve_blocking_returns_none_for_unowned_port() {
            assert_eq!(resolve_blocking(1), None);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}")
            .parse()
            .expect("test-local literal `127.0.0.1:<u16>` must parse as SocketAddr")
    }

    #[tokio::test]
    async fn noop_returns_none_for_any_port() {
        let resolver = NoopCwdResolver::new();
        assert_eq!(resolver.resolve(addr(1)).await, None);
        assert_eq!(resolver.resolve(addr(42321)).await, None);
        assert_eq!(resolver.resolve(addr(65535)).await, None);
    }

    #[tokio::test]
    async fn mock_returns_mapped_path_for_known_port() {
        let mut resolver = MockCwdResolver::new();
        resolver.insert(42321, PathBuf::from("/tmp/ws-a"));
        assert_eq!(
            resolver.resolve(addr(42321)).await,
            Some(PathBuf::from("/tmp/ws-a"))
        );
    }

    #[tokio::test]
    async fn mock_returns_none_for_unmapped_port() {
        let mut resolver = MockCwdResolver::new();
        resolver.insert(42321, PathBuf::from("/tmp/ws-a"));
        // Distinct port that is NOT in the map.
        assert_eq!(resolver.resolve(addr(54000)).await, None);
    }

    #[tokio::test]
    async fn mock_overwrites_existing_mapping_and_returns_previous() {
        let mut resolver = MockCwdResolver::new();
        assert_eq!(resolver.insert(42321, PathBuf::from("/a")), None);
        assert_eq!(
            resolver.insert(42321, PathBuf::from("/b")),
            Some(PathBuf::from("/a"))
        );
        assert_eq!(
            resolver.resolve(addr(42321)).await,
            Some(PathBuf::from("/b"))
        );
    }

    /// Exercise the `Arc<dyn CwdResolver>` shape that the Transport
    /// builder will use. If the trait is accidentally not
    /// dyn-compatible, this test fails to compile — a useful
    /// fence against future regressions.
    #[tokio::test]
    async fn trait_is_dyn_compatible_via_arc() {
        let resolver: Arc<dyn CwdResolver> = Arc::new(NoopCwdResolver::new());
        assert_eq!(resolver.resolve(addr(1)).await, None);

        let mut mock = MockCwdResolver::new();
        mock.insert(7777, PathBuf::from("/tmp/dyn"));
        let resolver: Arc<dyn CwdResolver> = Arc::new(mock);
        assert_eq!(
            resolver.resolve(addr(7777)).await,
            Some(PathBuf::from("/tmp/dyn"))
        );
    }

    #[tokio::test]
    async fn default_cwd_resolver_returns_something_callable() {
        // On non-macOS this is NoopCwdResolver → always None.
        // On macOS this is LibprocCwdResolver, whose §2 stub also
        // returns None until §3 lands. Either way, the call must
        // not panic and must return cleanly.
        let resolver = default_cwd_resolver();
        // Use an unbound, definitely-unmapped port. We don't assert
        // on the value (it can be None on every platform under the
        // §2 stub; later §3 may return Some on macOS for a real
        // peer port). We only assert that the call completes.
        let _ = resolver.resolve(addr(1)).await;
    }
}
