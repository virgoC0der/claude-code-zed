//! Per-client registry for multi-session at-mention routing.
//!
//! The registry replaces the prior single-client policy + broadcast
//! channel pair (`transport/ws.rs:166–188` of the pre-`session-routing`
//! tree). Each authorized WebSocket connection is tracked as a
//! [`ClientHandle`] in a shared [`ClientRegistry`]; outbound
//! notifications are delivered via that client's bounded
//! `mpsc::Sender`.
//!
//! Layer position: this module is part of layer 4 (`transport/`). It
//! depends only on `protocol` (for the wire-level `Notification`
//! type). It does NOT depend on `mcp`, `ipc`, or `app` — the router
//! consumes a [`ClientHandleSnapshot`] which is computed under the
//! lock and then dropped, so routing decisions never await while
//! holding the registry lock.
//!
//! See the OpenSpec deltas at
//! `openspec/changes/session-routing/specs/websocket/spec.md` for the
//! authoritative contract (the **Multi-client registry**,
//! **Workspace identification on connect**, **Per-client activity
//! tracking**, and **Outbound delivery via per-client channel**
//! requirements).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{RwLock, mpsc};
use tokio::time::Instant;
use uuid::Uuid;

use crate::protocol::Notification as JsonRpcNotification;

/// Capacity of each client's outbound mpsc channel. Matches the
/// capacity used by the prior `broadcast::Sender` (`transport/ws.rs`
/// `Transport::new`) so per-client buffering headroom is unchanged.
pub const CLIENT_CHANNEL_CAPACITY: usize = 64;

/// Opaque per-connection identifier.
///
/// Minted on accept; used in DEBUG/WARN logs and as the
/// `--client-id` value the helper carries in the second leg of a
/// picker round-trip. Wire form is the lowercase 36-character
/// hyphenated UUID v4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(pub Uuid);

impl ClientId {
    /// Mint a fresh random `ClientId` (UUID v4).
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Borrow the underlying [`Uuid`].
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for ClientId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hyphenated lowercase; matches the wire form used in IPC
        // frames and picker labels.
        std::fmt::Display::fmt(&self.0.hyphenated(), f)
    }
}

impl From<Uuid> for ClientId {
    fn from(u: Uuid) -> Self {
        Self(u)
    }
}

/// Live per-client state owned by the registry.
///
/// The `tx` channel is the only path by which the IPC layer delivers
/// outbound JSON-RPC notifications to this client; the connection
/// task receives from the matching `Receiver` and writes each frame
/// to its WebSocket peer.
#[derive(Debug)]
pub struct ClientHandle {
    /// Opaque per-connection identifier.
    pub id: ClientId,
    /// Bounded outbound notification channel (capacity
    /// [`CLIENT_CHANNEL_CAPACITY`]). Cloned cheaply into the
    /// router's per-call snapshot.
    pub tx: mpsc::Sender<JsonRpcNotification>,
    /// Canonicalised workspace cwd for this client, if any was
    /// resolved at connect time or set later via the MCP
    /// `initialize` request's `clientInfo.cwd`.
    pub workspace_root: Option<PathBuf>,
    /// Wall-clock-ish timestamp of the last inbound JSON-RPC frame
    /// from this client. Bumped by [`ClientRegistry::bump_activity`]
    /// before dispatch.
    pub last_activity: Instant,
    /// Fixed timestamp of when this client's WebSocket upgrade
    /// completed. Used to build picker labels
    /// (`connected_at_ms_ago`).
    pub connected_at: Instant,
}

/// A read-only snapshot of one [`ClientHandle`] for use by the
/// router.
///
/// Cloning the snapshot is cheap (`tx` is an `Arc` internally, paths
/// share allocations). The router consumes snapshots without holding
/// the registry lock so routing decisions never block other
/// registry operations.
#[derive(Debug, Clone)]
pub struct ClientHandleSnapshot {
    /// Opaque per-connection identifier.
    pub id: ClientId,
    /// Cheap clone of the client's outbound channel sender.
    pub tx: mpsc::Sender<JsonRpcNotification>,
    /// Canonicalised workspace cwd.
    pub workspace_root: Option<PathBuf>,
    /// Last inbound JSON-RPC frame timestamp.
    pub last_activity: Instant,
    /// Fixed connect timestamp.
    pub connected_at: Instant,
}

impl From<&ClientHandle> for ClientHandleSnapshot {
    fn from(h: &ClientHandle) -> Self {
        Self {
            id: h.id,
            tx: h.tx.clone(),
            workspace_root: h.workspace_root.clone(),
            last_activity: h.last_activity,
            connected_at: h.connected_at,
        }
    }
}

/// In-memory registry of every currently-connected, authorized
/// WebSocket client.
///
/// Cloning a [`ClientRegistry`] is cheap (it's `Arc`-shared with the
/// IPC layer). All mutating operations take a write lock for the
/// minimum duration needed; reads via [`ClientRegistry::snapshot`]
/// take a read lock, clone out, and drop.
#[derive(Debug, Clone, Default)]
pub struct ClientRegistry {
    inner: Arc<RwLock<HashMap<ClientId, ClientHandle>>>,
}

impl ClientRegistry {
    /// Build an empty registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Insert a new client handle. If a handle with the same
    /// `ClientId` is already present (vanishingly unlikely with
    /// UUID v4), it is overwritten and the prior `Sender` is
    /// dropped — that prior connection task will see its `Receiver`
    /// close on its next `recv` and exit cleanly.
    pub async fn insert(&self, handle: ClientHandle) {
        let mut guard = self.inner.write().await;
        guard.insert(handle.id, handle);
    }

    /// Remove the client with the given id. Returns `true` iff a
    /// handle was actually removed (i.e. the id was present).
    pub async fn remove(&self, id: ClientId) -> bool {
        let mut guard = self.inner.write().await;
        guard.remove(&id).is_some()
    }

    /// Atomically read out a snapshot of every registered client.
    /// The returned vector is owned and disconnected from the
    /// registry — the caller can hold it across `.await` points
    /// without contention.
    pub async fn snapshot(&self) -> Vec<ClientHandleSnapshot> {
        let guard = self.inner.read().await;
        guard.values().map(ClientHandleSnapshot::from).collect()
    }

    /// Bump the given client's `last_activity` to `Instant::now()`.
    /// No-op if the client has since disconnected. Returns `true`
    /// iff the update was applied.
    pub async fn bump_activity(&self, id: ClientId) -> bool {
        let mut guard = self.inner.write().await;
        if let Some(handle) = guard.get_mut(&id) {
            handle.last_activity = Instant::now();
            true
        } else {
            false
        }
    }

    /// Set / overwrite the given client's `workspace_root`. Used by
    /// the MCP `initialize` dispatch when `clientInfo.cwd` is
    /// present. No-op if the client has since disconnected.
    /// Returns `true` iff the update was applied.
    pub async fn set_workspace(&self, id: ClientId, workspace_root: PathBuf) -> bool {
        let mut guard = self.inner.write().await;
        if let Some(handle) = guard.get_mut(&id) {
            handle.workspace_root = Some(workspace_root);
            true
        } else {
            false
        }
    }

    /// Look up a single client's outbound channel sender. Returns
    /// `None` if the client has disconnected. Used by the router's
    /// rule 1 (`DirectClient`) path for direct delivery without
    /// scanning the full snapshot.
    pub async fn lookup_tx(&self, id: ClientId) -> Option<mpsc::Sender<JsonRpcNotification>> {
        let guard = self.inner.read().await;
        guard.get(&id).map(|h| h.tx.clone())
    }

    /// Number of currently-registered clients. Convenience for
    /// tests and DEBUG logs.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// `true` iff the registry has no live clients.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    use super::*;

    fn handle_at(workspace: Option<&str>) -> (ClientHandle, mpsc::Receiver<JsonRpcNotification>) {
        let (tx, rx) = mpsc::channel::<JsonRpcNotification>(CLIENT_CHANNEL_CAPACITY);
        let now = Instant::now();
        let h = ClientHandle {
            id: ClientId::new(),
            tx,
            workspace_root: workspace.map(PathBuf::from),
            last_activity: now,
            connected_at: now,
        };
        (h, rx)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn insert_two_clients_snapshot_returns_both() {
        let reg = ClientRegistry::new();
        let (h1, _rx1) = handle_at(Some("/a"));
        let (h2, _rx2) = handle_at(Some("/b"));
        let (id1, id2) = (h1.id, h2.id);
        reg.insert(h1).await;
        reg.insert(h2).await;

        let snap = reg.snapshot().await;
        assert_eq!(snap.len(), 2);
        let ids: Vec<ClientId> = snap.iter().map(|s| s.id).collect();
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remove_one_snapshot_returns_the_other() {
        let reg = ClientRegistry::new();
        let (h1, _rx1) = handle_at(Some("/a"));
        let (h2, _rx2) = handle_at(Some("/b"));
        let (id1, id2) = (h1.id, h2.id);
        reg.insert(h1).await;
        reg.insert(h2).await;

        assert!(reg.remove(id1).await);
        let snap = reg.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, id2);

        // Removing a stale id reports false and leaves the registry untouched.
        assert!(!reg.remove(id1).await);
        assert_eq!(reg.snapshot().await.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bump_activity_advances_monotonically() {
        let reg = ClientRegistry::new();
        let (h, _rx) = handle_at(None);
        let id = h.id;
        let original = h.last_activity;
        reg.insert(h).await;

        // Sleep a small amount so the new Instant is strictly greater.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        assert!(reg.bump_activity(id).await);

        let snap = reg.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert!(
            snap[0].last_activity >= original,
            "last_activity must be monotonic"
        );
        assert!(
            snap[0].last_activity > original,
            "after a sleep, last_activity must strictly advance"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bump_activity_on_stale_id_is_a_noop() {
        let reg = ClientRegistry::new();
        let stale = ClientId::new();
        assert!(!reg.bump_activity(stale).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_workspace_overwrites_prior_value() {
        let reg = ClientRegistry::new();
        let (h, _rx) = handle_at(Some("/initial"));
        let id = h.id;
        reg.insert(h).await;

        assert!(reg.set_workspace(id, PathBuf::from("/overwritten")).await);
        let snap = reg.snapshot().await;
        assert_eq!(
            snap[0].workspace_root.as_deref(),
            Some(std::path::Path::new("/overwritten"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_workspace_on_stale_id_is_a_noop() {
        let reg = ClientRegistry::new();
        let stale = ClientId::new();
        assert!(!reg.set_workspace(stale, PathBuf::from("/x")).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lookup_tx_returns_some_for_live_and_none_for_stale() {
        let reg = ClientRegistry::new();
        let (h, _rx) = handle_at(None);
        let id = h.id;
        reg.insert(h).await;

        assert!(reg.lookup_tx(id).await.is_some());

        let stale = ClientId::new();
        assert!(reg.lookup_tx(stale).await.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn len_and_is_empty_track_registry_state() {
        let reg = ClientRegistry::new();
        assert!(reg.is_empty().await);
        assert_eq!(reg.len().await, 0);

        let (h, _rx) = handle_at(None);
        let id = h.id;
        reg.insert(h).await;
        assert!(!reg.is_empty().await);
        assert_eq!(reg.len().await, 1);

        reg.remove(id).await;
        assert!(reg.is_empty().await);
    }

    #[test]
    fn client_id_display_is_lowercase_hyphenated_uuid() {
        let id =
            ClientId::from(Uuid::parse_str("F47AC10B-58CC-4372-A567-0E02B2C3D479").expect("uuid"));
        let s = id.to_string();
        // Always lowercase, always 36 chars with 4 hyphens.
        assert_eq!(s, "f47ac10b-58cc-4372-a567-0e02b2c3d479");
        assert_eq!(s.len(), 36);
        assert_eq!(s.chars().filter(|c| *c == '-').count(), 4);
    }
}
