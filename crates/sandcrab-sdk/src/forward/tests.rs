use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use sandcrab_core::testing::FakeBackend;
use sandcrab_core::{SandboxConfig, SandboxError};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

use crate::Sandbox;
use crate::test_support::Env;

async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A target that answers every byte with its upper case.
fn shouting_target() -> DuplexStream {
    let (near, mut far) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        while let Ok(n) = far.read(&mut buf).await {
            if n == 0 || far.write_all(&buf[..n].to_ascii_uppercase()).await.is_err() {
                break;
            }
        }
        let _ = far.shutdown().await;
    });
    near
}

async fn listening(sandbox: &Sandbox) {
    eventually("the forwards are listening", || {
        sandbox.forwards_listening()
    })
    .await;
}

#[tokio::test]
async fn accept_relays_and_reject_closes() {
    let env = Env::new();
    let backend = FakeBackend::new();
    let sc = env.sandcrab(&backend);
    let sb = sc
        .spawn(
            SandboxConfig::from_template("base")
                .forward(5432)
                .forward(6000),
        )
        .await
        .unwrap();
    let guest = backend.mock_of(sb.id()).unwrap().forwards;
    assert!(!sb.forwards_listening());

    // The target sees the guest port and decides per connection.
    let asked = Arc::new(AtomicU32::new(0));
    let seen = Arc::clone(&asked);
    sb.serve_forwards_with(move |port| {
        seen.fetch_add(1, Ordering::SeqCst);
        async move {
            match port {
                5432 => Ok(shouting_target()),
                _ => Err(io::Error::new(io::ErrorKind::ConnectionRefused, "no")),
            }
        }
    })
    .unwrap();
    listening(&sb).await;

    // Accept: bytes both ways, then the half-close travels through.
    let mut conn = guest.connect(5432).unwrap();
    conn.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    conn.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"PING");
    conn.shutdown().await.unwrap();
    let mut rest = Vec::new();
    conn.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());

    // Reject: the program's connection is closed without a byte.
    let mut conn = guest.connect(6000).unwrap();
    let mut rest = Vec::new();
    conn.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
    assert_eq!(asked.load(Ordering::SeqCst), 2);

    eventually("the rejection is counted", || {
        guest.stats_of(6000).refused.rejected == 1
    })
    .await;
    let stats = sb.forward_stats().await.unwrap();
    assert_eq!(stats.len(), 2);
    assert_eq!((stats[0].port, stats[0].accepted), (5432, 1));
    assert_eq!(stats[0].refused.rejected, 0);
    assert_eq!((stats[1].port, stats[1].accepted), (6000, 0));
    assert_eq!(stats[1].refused.rejected, 1);

    // One target per sandbox.
    let again = sb.serve_forwards_with(|_| async { Ok(shouting_target()) });
    assert!(matches!(again, Err(SandboxError::AlreadyExists { .. })));

    // The forwards end with the sandbox.
    sb.core().destroy().await.unwrap();
    assert!(!sb.forwards_listening());
    eventually("the listen connection closed", || !guest.has_listener()).await;
}

#[tokio::test]
async fn nothing_is_forwarded_until_a_target_is_given() {
    let env = Env::new();
    let backend = FakeBackend::new();
    let sc = env.sandcrab(&backend);
    let sb = sc
        .spawn(SandboxConfig::from_template("base").forward(5432))
        .await
        .unwrap();
    let guest = backend.mock_of(sb.id()).unwrap().forwards;
    let mut conn = guest.connect(5432).unwrap();
    let mut rest = Vec::new();
    conn.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
    let stats = sb.forward_stats().await.unwrap();
    assert_eq!((stats[0].accepted, stats[0].refused.no_listener), (0, 1));
    sb.destroy().await.unwrap();

    // A sandbox without forwards has nothing to serve.
    let sb = sc
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap();
    let err = sb
        .serve_forwards_with(|_| async { Ok(shouting_target()) })
        .unwrap_err();
    assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
    assert!(sb.forward_stats().await.unwrap().is_empty());
    sb.destroy().await.unwrap();
}

/// A backend that cannot forward refuses the spawn; it is never started without them.
#[tokio::test]
async fn a_backend_without_forwards_refuses_the_spawn() {
    let env = Env::new();
    let mut backend = FakeBackend::new();
    backend.caps.supports_forwards = false;
    let sc = env.sandcrab(&backend);
    let err = sc
        .spawn(SandboxConfig::from_template("base").forward(5432))
        .await
        .unwrap_err();
    assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");
    assert!(err.to_string().contains("forwards"), "{err}");
    assert!(backend.events().is_empty());
    assert!(sc.list_sandboxes().is_empty());
}
