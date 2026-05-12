//! Compile-time fence ensuring the `transport::cwd_resolver` public
//! surface is re-exported under `zed_claude_bridge::transport::*` so
//! downstream call sites (the upcoming `Transport::builder(...)`
//! API and the integration tests added in task #13 / #14) can import
//! the trait, the default factory, and both test/Noop impls without
//! reaching through the inner `cwd_resolver` module path.
//!
//! Per `openspec/changes/peer-cwd-discovery/tasks.md` §2.2 acceptance:
//! "`use zed_claude_bridge::transport::CwdResolver;` compiles from
//! an integration test under `tests/`."

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

// The re-exports under test. If §7 regresses, this file fails to
// compile — that is the intended fence.
use zed_claude_bridge::transport::{
    BoxResolveFuture, CwdResolver, MockCwdResolver, NoopCwdResolver, default_cwd_resolver,
};

#[tokio::test]
async fn reexported_trait_is_usable_via_arc_dyn() {
    let resolver: Arc<dyn CwdResolver> = Arc::new(NoopCwdResolver::new());
    let peer: SocketAddr = "127.0.0.1:42321"
        .parse()
        .expect("hardcoded literal must parse");
    let fut: BoxResolveFuture<'_> = resolver.resolve(peer);
    assert_eq!(fut.await, None);
}

#[tokio::test]
async fn reexported_mock_is_usable_from_integration_test() {
    let mut mock = MockCwdResolver::new();
    mock.insert(42321, PathBuf::from("/tmp/ws-a"));
    let peer: SocketAddr = "127.0.0.1:42321"
        .parse()
        .expect("hardcoded literal must parse");
    assert_eq!(mock.resolve(peer).await, Some(PathBuf::from("/tmp/ws-a")));
}

#[tokio::test]
async fn reexported_default_factory_is_callable() {
    let resolver = default_cwd_resolver();
    let peer: SocketAddr = "127.0.0.1:55555"
        .parse()
        .expect("hardcoded literal must parse");
    // We don't assert on the value: on non-macOS this is Noop (None);
    // on macOS this is the §2-stub `LibprocCwdResolver` (also None
    // until §3 / task #8 lands). The point is that the factory
    // resolves and the returned `Arc<dyn CwdResolver>` is callable.
    let _ = resolver.resolve(peer).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn libproc_resolver_reexport_is_macos_gated() {
    // This test only compiles on macOS — it confirms the
    // `pub use cwd_resolver::LibprocCwdResolver` re-export under
    // `#[cfg(target_os = "macos")]` is reachable from downstream
    // crates / integration tests on that target.
    use zed_claude_bridge::transport::LibprocCwdResolver;
    let resolver: Arc<dyn CwdResolver> = Arc::new(LibprocCwdResolver::new());
    let peer: SocketAddr = "127.0.0.1:55555"
        .parse()
        .expect("hardcoded literal must parse");
    let _ = resolver.resolve(peer).await;
}
