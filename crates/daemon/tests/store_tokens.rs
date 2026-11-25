//! Tokens (PR6-CL-19).

mod support;

use chaldea_core::ids::TokenId;
use chaldea_daemon::store::{NewToken, StoreError, TokenKind, TokenSelector, WriteError};
use support::{TestStore, launch, thread};

fn token(n: u128, kind: TokenKind, hash: u8) -> NewToken {
    NewToken {
        token_id: TokenId::from_u128(0x6000_0000_0000_0000_0000_0000_0000_0000 | n),
        kind,
        sha256: [hash; 32],
        thread_id: (kind == TokenKind::Hook || kind == TokenKind::Agent).then(|| thread(1)),
        launch_id: (kind == TokenKind::Hook || kind == TokenKind::Agent).then(|| launch(1)),
    }
}

/// Tokens are found by hash with their revocation, and a revoked token stays revoked.
#[test]
fn insert_revoke_lookup() {
    let s = TestStore::new();
    let (owner, hook, agent) = (
        token(1, TokenKind::Owner, 1),
        token(2, TokenKind::Hook, 2),
        token(3, TokenKind::Agent, 3),
    );
    let all = vec![owner.clone(), hook.clone(), agent.clone()];
    s.run(move |tx| {
        for t in all {
            tx.token_insert(t)?;
        }
        Ok(())
    })
    .unwrap();
    let row = s.read(|r| r.token_by_hash(&[2; 32])).unwrap();
    assert_eq!(
        (row.token_id, row.kind, row.launch_id),
        (hook.token_id, TokenKind::Hook, Some(launch(1)))
    );
    assert_eq!(row.created_at.0, support::T0);
    assert_eq!(row.revoked_at, None);
    assert_eq!(s.read(|r| r.token_by_hash(&[9; 32])), None);
    s.clock.set("2026-11-23T00:00:00.000Z");
    let n = s
        .run(|tx| Ok(tx.token_revoke(TokenSelector::Launch(launch(1)))?))
        .unwrap();
    assert_eq!(n, 2);
    for h in [2, 3] {
        let row = s.read(move |r| r.token_by_hash(&[h; 32])).unwrap();
        assert_eq!(row.revoked_at.unwrap().0, "2026-11-23T00:00:00.000Z");
    }
    assert_eq!(
        s.read(|r| r.token_by_hash(&[1; 32])).unwrap().revoked_at,
        None
    );
    // Revoking again changes nothing: the first revocation stays.
    s.clock.set("2026-11-24T00:00:00.000Z");
    let n = s
        .run(move |tx| Ok(tx.token_revoke(TokenSelector::Token(hook.token_id))?))
        .unwrap();
    assert_eq!(n, 0);
    let row = s.read(|r| r.token_by_hash(&[2; 32])).unwrap();
    assert_eq!(row.revoked_at.unwrap().0, "2026-11-23T00:00:00.000Z");
    let n = s
        .run(|tx| Ok(tx.token_revoke(TokenSelector::Thread(thread(1)))?))
        .unwrap();
    assert_eq!(n, 0);
    let n = s
        .run(move |tx| Ok(tx.token_revoke(TokenSelector::Token(owner.token_id))?))
        .unwrap();
    assert_eq!(n, 1);
}

/// A second token with a stored hash is refused.
#[test]
fn duplicate_hash_is_refused() {
    let s = TestStore::new();
    s.run(|tx| Ok(tx.token_insert(token(1, TokenKind::Owner, 1))?))
        .unwrap();
    let r = s.run(|tx| Ok(tx.token_insert(token(2, TokenKind::Rescue, 1))?));
    assert!(
        matches!(r, Err(WriteError::RolledBack(StoreError::Constraint(_)))),
        "{r:?}"
    );
    assert_eq!(
        s.read(|r| r.token_by_hash(&[1; 32])).unwrap().kind,
        TokenKind::Owner
    );
}

/// `LaunchScoped` revokes every token bound to a launch, whichever launch, and leaves the
/// tokens without one.
#[test]
fn launch_scoped_revokes_every_launch_token() {
    let s = TestStore::new();
    let mut other = token(4, TokenKind::Hook, 4);
    other.launch_id = Some(launch(2));
    let rows = vec![
        token(1, TokenKind::Owner, 1),
        token(2, TokenKind::Hook, 2),
        token(3, TokenKind::Agent, 3),
        other,
    ];
    s.run(move |tx| {
        for t in rows {
            tx.token_insert(t)?;
        }
        Ok(())
    })
    .unwrap();
    let n = s
        .run(|tx| Ok(tx.token_revoke(TokenSelector::LaunchScoped)?))
        .unwrap();
    assert_eq!(n, 3);
    for (h, revoked) in [(1, false), (2, true), (3, true), (4, true)] {
        let row = s.read(move |r| r.token_by_hash(&[h; 32])).unwrap();
        assert_eq!(row.revoked_at.is_some(), revoked, "token {h}");
    }
    let n = s
        .run(|tx| Ok(tx.token_revoke(TokenSelector::LaunchScoped)?))
        .unwrap();
    assert_eq!(n, 0);
}
