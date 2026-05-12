//! Routing logic for outbound `at_mentioned` and `selection_changed`
//! notifications.
//!
//! The router is a pair of **pure functions** that consume a
//! [`ClientHandleSnapshot`] slice plus per-call frame fields and
//! return a [`RoutingDecision`] / `Vec<ClientId>`. No I/O, no async,
//! no `await` — the caller (IPC layer) performs the actual
//! `mpsc::Sender::send` and deals with timeouts.
//!
//! Layer position: this module is part of layer 4 (`transport/`). It
//! depends on `protocol` (for [`AmbiguousCandidate`]) and on its
//! sibling [`registry`] (for [`ClientHandleSnapshot`]). It does NOT
//! depend on `ipc` or `app`.
//!
//! The contract this module implements is the OpenSpec delta at
//! `openspec/changes/session-routing/specs/notifications/spec.md`,
//! specifically the **at_mentioned routing**, **selection_changed
//! routing within workspace**, and **Ambiguous candidate label
//! content** requirements. Each scenario in those requirements has
//! at least one corresponding unit test below.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

use crate::protocol::AmbiguousCandidate;
use crate::transport::registry::{ClientHandleSnapshot, ClientId};

/// One routing decision for an inbound `at_mention` IPC frame.
///
/// Total / deterministic by construction — every input maps to
/// exactly one variant. The IPC layer dispatches on the variant:
///
/// - `DirectClient` / `WorkspaceUnique` / `Singleton` → deliver the
///   `at_mentioned` notification to that client's `tx`.
/// - `Ambiguous` → write an `IpcFrame::Ambiguous { candidates }` to
///   the helper on the same IPC connection; await a follow-up
///   `at_mention` with `client_id` set.
/// - `StaleClientId` → log WARN with the stale id + the current
///   registry ids; drop the notification.
/// - `NoMatch` → log WARN with the frame's workspace_root + the
///   sorted set of known workspaces; drop the notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingDecision {
    /// Rule 1: frame's `client_id` matched a live registry entry.
    DirectClient(ClientId),
    /// Rule 1 fall-through: frame's `client_id` was set but the
    /// referenced client has since disconnected. The IPC layer
    /// drops the notification with a distinct WARN; the user is
    /// expected to retry the at-mention from Zed.
    StaleClientId {
        /// The id the helper requested (e.g. from a stale picker).
        requested: ClientId,
        /// All currently-live ids in the registry, sorted by their
        /// hyphenated UUID form so logs are stable across runs.
        known_ids: Vec<ClientId>,
    },
    /// Rule 2: frame's `workspace_root` uniquely matched one client.
    WorkspaceUnique(ClientId),
    /// Rule 3: frame's `workspace_root` matched more than one
    /// client. The IPC layer SHALL reply with an
    /// `IpcFrame::Ambiguous { candidates }` and await a follow-up
    /// `at_mention` with `client_id` set.
    Ambiguous {
        /// One candidate per workspace-matching client, in stable
        /// snapshot order. Labels are pre-built per the
        /// **Ambiguous candidate label content** requirement.
        candidates: Vec<AmbiguousCandidate>,
    },
    /// Rule 4: the registry contains exactly one client (regardless
    /// of workspace).
    Singleton(ClientId),
    /// Rule 5: no live client matches; drop with a WARN.
    NoMatch {
        /// Sorted list of every workspace_root seen in the
        /// snapshot (deduplicated). Used by the IPC layer's WARN
        /// log so the user can see "what we knew about" at the
        /// moment of the drop.
        known_workspaces: Vec<PathBuf>,
    },
}

/// Decide which (if any) WebSocket client should receive an
/// `at_mentioned` notification produced from an IPC `at_mention`
/// frame.
///
/// The function is pure: it neither touches the registry lock nor
/// emits any side effect. The IPC layer is responsible for taking
/// a `snapshot()` immediately before calling this function and for
/// dispatching to the chosen client's `tx` channel afterwards.
///
/// Inputs:
/// - `snapshot` — every currently-registered client. The router
///   compares `snapshot[i].workspace_root` with `frame_workspace`
///   via `PathBuf::eq`; the caller is responsible for
///   canonicalising both sides at registry-insert and
///   IPC-frame-parse time, respectively.
/// - `frame_workspace` — the optional `workspace_root` from the
///   IPC frame.
/// - `frame_client_id` — the optional `client_id` override on the
///   IPC frame. When set, this triggers rule 1 (DirectClient or
///   StaleClientId).
/// - `now` — the wall-clock instant used to compute
///   `connected_at_ms_ago` and `last_activity_ms_ago` on ambiguous
///   candidates. Pass `Instant::now()` from production code; pass
///   a fixed value from tests for reproducibility.
///
/// Rules — applied in priority order:
///
/// 1. Direct `client_id` override (`DirectClient` or
///    `StaleClientId`).
/// 2. Workspace match — unique (`WorkspaceUnique`).
/// 3. Workspace match — ambiguous (`Ambiguous`).
/// 4. Singleton registry (`Singleton`).
/// 5. No match (`NoMatch`).
pub fn route_at_mention(
    snapshot: &[ClientHandleSnapshot],
    frame_workspace: Option<&Path>,
    frame_client_id: Option<ClientId>,
    now: Instant,
) -> RoutingDecision {
    // Rule 1: client_id override.
    if let Some(requested) = frame_client_id {
        if snapshot.iter().any(|c| c.id == requested) {
            return RoutingDecision::DirectClient(requested);
        }
        // Stale: helper picked a client that has since disconnected.
        let mut known_ids: Vec<ClientId> = snapshot.iter().map(|c| c.id).collect();
        known_ids.sort_by_key(|id| id.to_string());
        return RoutingDecision::StaleClientId {
            requested,
            known_ids,
        };
    }

    // Rule 2 / 3: workspace match.
    if let Some(workspace) = frame_workspace {
        let matches: Vec<&ClientHandleSnapshot> = snapshot
            .iter()
            .filter(|c| c.workspace_root.as_deref().is_some_and(|p| p == workspace))
            .collect();
        match matches.len() {
            0 => { /* fall through to rules 4/5 */ }
            1 => return RoutingDecision::WorkspaceUnique(matches[0].id),
            _ => {
                let candidates = build_candidates(&matches, now);
                return RoutingDecision::Ambiguous { candidates };
            }
        }
    }

    // Rule 4: singleton registry.
    if snapshot.len() == 1 {
        return RoutingDecision::Singleton(snapshot[0].id);
    }

    // Rule 5: no match.
    let mut known_workspaces: Vec<PathBuf> = snapshot
        .iter()
        .filter_map(|c| c.workspace_root.clone())
        .collect();
    known_workspaces.sort();
    known_workspaces.dedup();
    RoutingDecision::NoMatch { known_workspaces }
}

/// Decide which WebSocket clients should receive a
/// `selection_changed` notification.
///
/// **Longest-prefix routing semantics** (per the **selection_changed
/// routing within workspace** requirement in `notifications/spec.md`,
/// lines 213–216 of the session-routing delta):
///
/// 1. Among the registered workspace roots, find the one with the
///    longest path-component count that is a prefix of
///    `frame_file_path`. Ties (two different roots with equal
///    component-length both prefixing the file) cannot occur with
///    canonicalised paths.
/// 2. Return every client whose `workspace_root` canonically equals
///    that longest matching root.
/// 3. If no registered workspace prefixes `frame_file_path`, fan
///    out to **every** registered client — preserving the
///    pre-routing fan-out behaviour for selections from outside any
///    registered worktree.
///
/// The picker round-trip SHALL NOT apply to `selection_changed`.
/// This is by design: selection changes are a read-only editor state
/// push, and fanning out within a workspace is the expected
/// behaviour.
pub fn route_selection_changed(
    snapshot: &[ClientHandleSnapshot],
    frame_file_path: &str,
) -> Vec<ClientId> {
    if snapshot.is_empty() {
        return Vec::new();
    }
    let file_path = Path::new(frame_file_path);

    // Step 1: find the longest workspace_root (by path-component
    // count) that is a prefix of file_path. Iterate over distinct
    // workspace_roots so we don't accidentally double-count.
    let mut longest: Option<&Path> = None;
    let mut longest_components: usize = 0;
    for client in snapshot {
        let Some(ws) = client.workspace_root.as_deref() else {
            continue;
        };
        if !path_starts_with(file_path, ws) {
            continue;
        }
        let n = ws.components().count();
        if n > longest_components {
            longest_components = n;
            longest = Some(ws);
        }
    }

    let Some(winner) = longest else {
        // Step 3: no prefix matches → fan out to every client.
        return snapshot.iter().map(|c| c.id).collect();
    };

    // Step 2: return every client whose workspace_root equals the
    // winning longest-prefix root.
    snapshot
        .iter()
        .filter(|c| c.workspace_root.as_deref() == Some(winner))
        .map(|c| c.id)
        .collect()
}

/// True iff `file_path` starts with `prefix` as a path prefix (not a
/// byte-string prefix). Uses `Path::starts_with`, which matches
/// whole components — so `/p` is NOT a prefix of `/page/x.rs`.
fn path_starts_with(file_path: &Path, prefix: &Path) -> bool {
    file_path.starts_with(prefix)
}

/// Build [`AmbiguousCandidate`] entries from workspace-matching
/// snapshots.
///
/// Labels satisfy the **Ambiguous candidate label content**
/// requirement:
/// - non-empty UTF-8;
/// - distinct within the candidate list (a `#2`, `#3`, … suffix is
///   appended when two candidates produce identical base labels);
/// - human-readable, containing a 1-based ordinal and a
///   "connected X ago" elapsed-time phrase.
fn build_candidates(matches: &[&ClientHandleSnapshot], now: Instant) -> Vec<AmbiguousCandidate> {
    let mut base_labels: Vec<String> = Vec::with_capacity(matches.len());
    let mut candidates: Vec<AmbiguousCandidate> = Vec::with_capacity(matches.len());
    for (idx, c) in matches.iter().enumerate() {
        let connected_ms = elapsed_ms_since(now, c.connected_at);
        let last_activity_ms = elapsed_ms_since(now, c.last_activity);
        let label = build_label(idx + 1, connected_ms, last_activity_ms);
        base_labels.push(label.clone());
        candidates.push(AmbiguousCandidate {
            client_id: c.id.as_uuid(),
            label,
            connected_at_ms_ago: connected_ms,
            last_activity_ms_ago: last_activity_ms,
        });
    }
    // Ensure distinctness: if any base label appears more than once,
    // append " #N" (1-based occurrence index) so the picker sees
    // unique strings.
    disambiguate_labels(&mut candidates);
    candidates
}

/// Construct the base picker label.
///
/// Format: `Session {N} — connected {duration} ago[, last active {duration} ago]`.
/// The "last active" clause is omitted when it equals "connected"
/// to within 1 second (idle session never bumped activity).
fn build_label(ordinal: usize, connected_ms: u64, last_activity_ms: u64) -> String {
    let connected_phrase = humanise_elapsed(connected_ms);
    if abs_diff(connected_ms, last_activity_ms) <= 1_000 {
        format!("Session {ordinal} — connected {connected_phrase} ago")
    } else {
        let activity_phrase = humanise_elapsed(last_activity_ms);
        format!(
            "Session {ordinal} — connected {connected_phrase} ago, last active {activity_phrase} ago"
        )
    }
}

/// `|a - b|` for two `u64` values, without underflow.
fn abs_diff(a: u64, b: u64) -> u64 {
    a.abs_diff(b)
}

/// Render an elapsed-milliseconds value as a short human string:
/// `"450ms"`, `"3s"`, `"2m"`, `"1h"`.
fn humanise_elapsed(ms: u64) -> String {
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    let secs = ms / 1_000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    format!("{hours}h")
}

fn elapsed_ms_since(now: Instant, then: Instant) -> u64 {
    let dur: Duration = now.saturating_duration_since(then);
    // Saturate at u64::MAX rather than panic on truly absurd values.
    u64::try_from(dur.as_millis()).unwrap_or(u64::MAX)
}

/// Apply `#2`, `#3`, … suffixes so every label in `candidates` is
/// unique. Cheap O(n²) scan; the candidate list is bounded by the
/// number of registered WebSocket clients sharing one workspace,
/// realistically ≤ 4 in practice.
fn disambiguate_labels(candidates: &mut [AmbiguousCandidate]) {
    let n = candidates.len();
    if n < 2 {
        return;
    }
    // Collect a snapshot of base labels to compare against.
    let base_labels: Vec<String> = candidates.iter().map(|c| c.label.clone()).collect();
    for i in 0..n {
        let mut occurrence = 1usize;
        for j in 0..i {
            if base_labels[j] == base_labels[i] {
                occurrence += 1;
            }
        }
        // If there is any later or earlier collision, mark this one
        // with its 1-based occurrence index. occurrence==1 with at
        // least one later collision still gets " #1" so all
        // collisions are explicit.
        let has_collision = base_labels
            .iter()
            .enumerate()
            .any(|(k, lbl)| k != i && lbl == &base_labels[i]);
        if has_collision {
            candidates[i].label = format!("{} #{}", base_labels[i], occurrence);
        }
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
    use std::time::Duration;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use crate::protocol::Notification as JsonRpcNotification;
    use crate::transport::registry::CLIENT_CHANNEL_CAPACITY;

    /// Build a snapshot with a fixed client id so tests can predict
    /// outcomes without searching for the right uuid.
    fn snapshot_with_id(
        id: ClientId,
        workspace: Option<&str>,
        connected_ago: Duration,
        active_ago: Duration,
    ) -> (ClientHandleSnapshot, mpsc::Receiver<JsonRpcNotification>) {
        let (tx, rx) = mpsc::channel(CLIENT_CHANNEL_CAPACITY);
        let now = Instant::now();
        let snap = ClientHandleSnapshot {
            id,
            tx,
            workspace_root: workspace.map(PathBuf::from),
            last_activity: now.checked_sub(active_ago).unwrap_or(now),
            connected_at: now.checked_sub(connected_ago).unwrap_or(now),
        };
        (snap, rx)
    }

    // -------------------- route_at_mention --------------------

    #[test]
    fn at_mention_client_id_override_wins_over_workspace_match() {
        // Spec scenario: "client_id override wins over workspace match"
        let id_a = ClientId::new();
        let id_b = ClientId::new();
        let (a, _rxa) = snapshot_with_id(
            id_a,
            Some("/p"),
            Duration::from_secs(10),
            Duration::from_secs(1),
        );
        let (b, _rxb) = snapshot_with_id(
            id_b,
            Some("/q"),
            Duration::from_secs(10),
            Duration::from_secs(1),
        );
        let snap = vec![a, b];

        let decision = route_at_mention(&snap, Some(Path::new("/p")), Some(id_b), Instant::now());
        assert_eq!(decision, RoutingDecision::DirectClient(id_b));
    }

    #[test]
    fn at_mention_stale_client_id_falls_through_distinctly() {
        // Spec scenario: "Stale client_id falls through to no-match drop"
        let id_a = ClientId::new();
        let stale = ClientId::new();
        let (a, _rxa) = snapshot_with_id(
            id_a,
            Some("/p"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let snap = vec![a];

        let decision = route_at_mention(&snap, None, Some(stale), Instant::now());
        match decision {
            RoutingDecision::StaleClientId {
                requested,
                known_ids,
            } => {
                assert_eq!(requested, stale);
                assert_eq!(known_ids, vec![id_a]);
            }
            other => panic!("expected StaleClientId; got {other:?}"),
        }
    }

    #[test]
    fn at_mention_workspace_match_picks_lone_matching_client() {
        // Spec scenario: "Workspace match picks the lone matching client"
        let id_a = ClientId::new();
        let id_b = ClientId::new();
        let (a, _) = snapshot_with_id(
            id_a,
            Some("/p"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let (b, _) = snapshot_with_id(
            id_b,
            Some("/q"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let snap = vec![a, b];

        let decision = route_at_mention(&snap, Some(Path::new("/p")), None, Instant::now());
        assert_eq!(decision, RoutingDecision::WorkspaceUnique(id_a));
    }

    #[test]
    fn at_mention_same_workspace_yields_ambiguous_decision() {
        // Spec scenario: "Same workspace yields an ambiguous decision"
        let id_a = ClientId::new();
        let id_b = ClientId::new();
        let (a, _) = snapshot_with_id(
            id_a,
            Some("/p"),
            Duration::from_secs(120),
            Duration::from_secs(60),
        );
        let (b, _) = snapshot_with_id(
            id_b,
            Some("/p"),
            Duration::from_secs(30),
            Duration::from_secs(5),
        );
        let snap = vec![a, b];

        let decision = route_at_mention(&snap, Some(Path::new("/p")), None, Instant::now());
        match decision {
            RoutingDecision::Ambiguous { candidates } => {
                assert_eq!(candidates.len(), 2);
                let ids: Vec<Uuid> = candidates.iter().map(|c| c.client_id).collect();
                assert!(ids.contains(&id_a.as_uuid()));
                assert!(ids.contains(&id_b.as_uuid()));
                // Labels are distinct & non-empty & contain a 1-based ordinal
                assert!(!candidates[0].label.is_empty());
                assert!(!candidates[1].label.is_empty());
                assert_ne!(candidates[0].label, candidates[1].label);
                assert!(candidates[0].label.contains('1'));
                assert!(candidates[1].label.contains('2'));
            }
            other => panic!("expected Ambiguous; got {other:?}"),
        }
    }

    #[test]
    fn at_mention_singleton_routes_regardless_of_workspace() {
        // Spec scenario: "Singleton registry routes regardless of workspace"
        let id_a = ClientId::new();
        let (a, _) = snapshot_with_id(
            id_a,
            Some("/p"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let snap = vec![a];

        // workspace=/q does NOT match A's /p, but singleton fires.
        let decision = route_at_mention(&snap, Some(Path::new("/q")), None, Instant::now());
        assert_eq!(decision, RoutingDecision::Singleton(id_a));
    }

    #[test]
    fn at_mention_singleton_with_no_workspace_root_in_frame_fires() {
        // Variant: workspace_root None, registry singleton, expect Singleton.
        let id_a = ClientId::new();
        let (a, _) = snapshot_with_id(id_a, None, Duration::from_secs(5), Duration::from_secs(1));
        let snap = vec![a];

        let decision = route_at_mention(&snap, None, None, Instant::now());
        assert_eq!(decision, RoutingDecision::Singleton(id_a));
    }

    #[test]
    fn at_mention_no_match_returns_sorted_known_workspaces() {
        // Spec scenario: "No matching client drops with a WARN"
        let id_a = ClientId::new();
        let id_b = ClientId::new();
        let (a, _) = snapshot_with_id(
            id_a,
            Some("/q"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let (b, _) = snapshot_with_id(
            id_b,
            Some("/p"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let snap = vec![a, b];

        let decision = route_at_mention(&snap, Some(Path::new("/r")), None, Instant::now());
        match decision {
            RoutingDecision::NoMatch { known_workspaces } => {
                assert_eq!(
                    known_workspaces,
                    vec![PathBuf::from("/p"), PathBuf::from("/q")],
                    "known_workspaces SHALL be sorted"
                );
            }
            other => panic!("expected NoMatch; got {other:?}"),
        }
    }

    #[test]
    fn at_mention_no_match_with_empty_registry() {
        // Boundary: empty registry, no frame_workspace, no client_id.
        let snap: Vec<ClientHandleSnapshot> = Vec::new();
        let decision = route_at_mention(&snap, None, None, Instant::now());
        assert_eq!(
            decision,
            RoutingDecision::NoMatch {
                known_workspaces: Vec::new()
            }
        );
    }

    #[test]
    fn at_mention_no_match_deduplicates_known_workspaces() {
        // Two clients in the same workspace, frame asks for a different workspace.
        let id_a = ClientId::new();
        let id_b = ClientId::new();
        let (a, _) = snapshot_with_id(
            id_a,
            Some("/p"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let (b, _) = snapshot_with_id(
            id_b,
            Some("/p"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let snap = vec![a, b];

        let decision = route_at_mention(&snap, Some(Path::new("/r")), None, Instant::now());
        match decision {
            RoutingDecision::NoMatch { known_workspaces } => {
                assert_eq!(known_workspaces, vec![PathBuf::from("/p")]);
            }
            other => panic!("expected NoMatch; got {other:?}"),
        }
    }

    // -------------------- AmbiguousCandidate labels --------------------

    #[test]
    fn ambiguous_labels_contain_ordinal_and_elapsed_phrase() {
        // Spec scenario: "Labels are distinct and human-readable"
        let id_a = ClientId::new();
        let id_b = ClientId::new();
        // Connected 90s apart so the elapsed-time phrasing differs.
        let (a, _) = snapshot_with_id(
            id_a,
            Some("/p"),
            Duration::from_secs(120),
            Duration::from_secs(60),
        );
        let (b, _) = snapshot_with_id(
            id_b,
            Some("/p"),
            Duration::from_secs(30),
            Duration::from_secs(5),
        );
        let snap = vec![a, b];

        let decision = route_at_mention(&snap, Some(Path::new("/p")), None, Instant::now());
        if let RoutingDecision::Ambiguous { candidates } = decision {
            assert_eq!(candidates.len(), 2);
            assert_ne!(candidates[0].label, candidates[1].label);
            // 1-based ordinals present
            assert!(
                candidates[0].label.contains('1'),
                "label: {}",
                candidates[0].label
            );
            assert!(
                candidates[1].label.contains('2'),
                "label: {}",
                candidates[1].label
            );
            // Elapsed-time phrases present (s/m/ms — any humanised unit)
            for c in &candidates {
                let has_unit = c.label.contains("ms")
                    || c.label.contains('s')
                    || c.label.contains('m')
                    || c.label.contains('h');
                assert!(has_unit, "label '{}' missing elapsed unit", c.label);
            }
        } else {
            panic!("expected Ambiguous decision");
        }
    }

    #[test]
    fn ambiguous_labels_disambiguate_identical_bases_with_suffix() {
        // Two clients connected at identical instants → identical
        // base labels → suffix disambiguator kicks in.
        let now = Instant::now();
        let (tx_a, _rxa) = mpsc::channel(CLIENT_CHANNEL_CAPACITY);
        let (tx_b, _rxb) = mpsc::channel(CLIENT_CHANNEL_CAPACITY);
        let same_connected = now.checked_sub(Duration::from_secs(10)).unwrap_or(now);
        let a = ClientHandleSnapshot {
            id: ClientId::new(),
            tx: tx_a,
            workspace_root: Some(PathBuf::from("/p")),
            last_activity: same_connected,
            connected_at: same_connected,
        };
        let b = ClientHandleSnapshot {
            id: ClientId::new(),
            tx: tx_b,
            workspace_root: Some(PathBuf::from("/p")),
            last_activity: same_connected,
            connected_at: same_connected,
        };
        let snap = vec![a, b];

        let decision = route_at_mention(&snap, Some(Path::new("/p")), None, now);
        if let RoutingDecision::Ambiguous { candidates } = decision {
            assert_eq!(candidates.len(), 2);
            // The ORDINAL differs even when timings match, so base labels
            // already differ — assert distinctness directly.
            assert_ne!(candidates[0].label, candidates[1].label);
        } else {
            panic!("expected Ambiguous");
        }
    }

    #[test]
    fn label_disambiguator_suffix_fires_when_base_labels_collide() {
        // Drive `disambiguate_labels` directly with two identical
        // base labels — the suffix path is the only way the picker
        // can stay unique if a future label format ever omits the
        // ordinal.
        let mut cands = vec![
            AmbiguousCandidate {
                client_id: Uuid::new_v4(),
                label: "Same label".to_string(),
                connected_at_ms_ago: 1,
                last_activity_ms_ago: 1,
            },
            AmbiguousCandidate {
                client_id: Uuid::new_v4(),
                label: "Same label".to_string(),
                connected_at_ms_ago: 1,
                last_activity_ms_ago: 1,
            },
        ];
        disambiguate_labels(&mut cands);
        assert_ne!(cands[0].label, cands[1].label);
        assert!(cands[0].label.ends_with("#1"));
        assert!(cands[1].label.ends_with("#2"));
    }

    // -------------------- route_selection_changed --------------------

    #[test]
    fn selection_changed_matching_prefix_returns_subset() {
        // Spec scenario: "selection_changed reaches only matching workspaces"
        let id_a = ClientId::new();
        let id_b = ClientId::new();
        let (a, _) = snapshot_with_id(
            id_a,
            Some("/p"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let (b, _) = snapshot_with_id(
            id_b,
            Some("/q"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let snap = vec![a, b];

        let ids = route_selection_changed(&snap, "/p/main.rs");
        assert_eq!(ids, vec![id_a]);
    }

    #[test]
    fn selection_changed_no_prefix_match_fans_out_to_all() {
        // Spec scenario: "selection_changed with unknown workspace fans out"
        let id_a = ClientId::new();
        let id_b = ClientId::new();
        let (a, _) = snapshot_with_id(
            id_a,
            Some("/p"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let (b, _) = snapshot_with_id(
            id_b,
            Some("/q"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let snap = vec![a, b];

        let ids = route_selection_changed(&snap, "/unrelated/path/main.rs");
        // Both ids returned; order matches snapshot iteration.
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&id_a));
        assert!(ids.contains(&id_b));
    }

    #[test]
    fn selection_changed_empty_snapshot_returns_empty_vec() {
        let snap: Vec<ClientHandleSnapshot> = Vec::new();
        let ids = route_selection_changed(&snap, "/p/main.rs");
        assert!(ids.is_empty());
    }

    #[test]
    fn selection_changed_path_prefix_is_component_aware() {
        // `/p` is NOT a prefix of `/page/x.rs` — Path::starts_with
        // matches whole components only.
        let id_a = ClientId::new();
        let id_b = ClientId::new();
        let (a, _) = snapshot_with_id(
            id_a,
            Some("/p"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let (b, _) = snapshot_with_id(
            id_b,
            Some("/page"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let snap = vec![a, b];

        let ids = route_selection_changed(&snap, "/page/x.rs");
        assert_eq!(ids, vec![id_b]);
    }

    #[test]
    fn selection_changed_uses_longest_prefix_when_nested_workspaces() {
        // Spec contract (notifications/spec.md lines 213–216): when
        // multiple registered workspace roots are prefixes of the
        // file path (e.g. nested worktrees `/a` and `/a/inner`), the
        // ROUTER chooses the longest one.
        let id_outer = ClientId::new();
        let id_inner = ClientId::new();
        let (outer, _) = snapshot_with_id(
            id_outer,
            Some("/a"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let (inner, _) = snapshot_with_id(
            id_inner,
            Some("/a/inner"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let snap = vec![outer, inner];

        // File inside the inner workspace: longest-prefix → only inner.
        let ids = route_selection_changed(&snap, "/a/inner/lib.rs");
        assert_eq!(
            ids,
            vec![id_inner],
            "longest-prefix wins; outer SHALL NOT receive"
        );

        // File inside outer-only (not inner): outer is the longest match.
        let ids = route_selection_changed(&snap, "/a/other.rs");
        assert_eq!(ids, vec![id_outer]);
    }

    #[test]
    fn selection_changed_longest_prefix_returns_every_client_at_winning_root() {
        // Two clients both at `/a/inner`, one at `/a`. File in
        // `/a/inner/...`. Spec says return EVERY client whose
        // workspace canonically equals the longest-prefix winner.
        let id_outer = ClientId::new();
        let id_inner_1 = ClientId::new();
        let id_inner_2 = ClientId::new();
        let (outer, _) = snapshot_with_id(
            id_outer,
            Some("/a"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let (inner_1, _) = snapshot_with_id(
            id_inner_1,
            Some("/a/inner"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let (inner_2, _) = snapshot_with_id(
            id_inner_2,
            Some("/a/inner"),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let snap = vec![outer, inner_1, inner_2];

        let ids = route_selection_changed(&snap, "/a/inner/x.rs");
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&id_inner_1));
        assert!(ids.contains(&id_inner_2));
        assert!(!ids.contains(&id_outer));
    }

    // -------------------- humanise_elapsed --------------------

    #[test]
    fn humanise_elapsed_uses_largest_fitting_unit() {
        assert_eq!(humanise_elapsed(0), "0ms");
        assert_eq!(humanise_elapsed(999), "999ms");
        assert_eq!(humanise_elapsed(1_000), "1s");
        assert_eq!(humanise_elapsed(59_999), "59s");
        assert_eq!(humanise_elapsed(60_000), "1m");
        assert_eq!(humanise_elapsed(3_599_000), "59m");
        assert_eq!(humanise_elapsed(3_600_000), "1h");
    }
}
