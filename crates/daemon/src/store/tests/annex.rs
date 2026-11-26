//! The annex's persisted JSON (PR6-CL-23).

use crate::store::{Continuity, CountedClass, CountedClasses, EventAnnex, WriteBasis};

/// Every variant serializes to exactly the JSON of §2.6 and reads back.
#[test]
fn golden_json() {
    let readiness = EventAnnex::Readiness(WriteBasis {
        launch_id: chaldea_core::ids::LaunchId::from_u128(
            0x0190_c6f2_0000_7000_8000_0000_0000_0001,
        ),
        position: 42,
        counted: CountedClasses::ALL,
    });
    let json = r#"{"readiness":{"launch_id":"0190c6f2-0000-7000-8000-000000000001","position":42,"counted":["input","human","reply"]}}"#;
    assert_eq!(serde_json::to_string(&readiness).unwrap(), json);
    assert_eq!(serde_json::from_str::<EventAnnex>(json).unwrap(), readiness);
    let transcript = EventAnnex::Transcript(Continuity("opaque".into()));
    let json = r#"{"transcript":"opaque"}"#;
    assert_eq!(serde_json::to_string(&transcript).unwrap(), json);
    assert_eq!(
        serde_json::from_str::<EventAnnex>(json).unwrap(),
        transcript
    );
    let hook = EventAnnex::Hook { truncated: true };
    let json = r#"{"hook":{"truncated":true}}"#;
    assert_eq!(serde_json::to_string(&hook).unwrap(), json);
    assert_eq!(serde_json::from_str::<EventAnnex>(json).unwrap(), hook);
    let prompt = EventAnnex::Prompt { superseded: true };
    let json = r#"{"prompt":{"superseded":true}}"#;
    assert_eq!(serde_json::to_string(&prompt).unwrap(), json);
    assert_eq!(serde_json::from_str::<EventAnnex>(json).unwrap(), prompt);
}

/// The hook annex is accepted only on a `raw_unknown` whose source kind is `hook`.
#[test]
fn hook_annex_only_on_a_hook_raw_unknown() {
    use chaldea_core::ids::StreamId;
    use chaldea_core::payload::{Content, Encoding, SourceKind};

    use super::support::{TestStore, raw_unknown, registered, source, thread};
    use crate::store::{AnnexError, StoreError, WriteError};

    let s = TestStore::new();
    let t = thread(1);
    s.append(vec![registered(t, false)]).unwrap();
    let raw = || Content::Inline {
        inline: "x".into(),
        encoding: Encoding::Utf8,
    };
    let mut ok = raw_unknown(StreamId::Thread(t), "hook:1", raw());
    ok.source = source(SourceKind::Hook);
    ok.annex = Some(EventAnnex::Hook { truncated: true });
    s.append(vec![ok])
        .expect("a hook raw_unknown takes the annex");
    let unexpected = |r: Result<_, WriteError>, what: &str| {
        assert!(
            matches!(
                r,
                Err(WriteError::RolledBack(StoreError::Annex(
                    AnnexError::Unexpected(_)
                )))
            ),
            "{what}: {r:?}"
        );
    };
    let mut daemon = raw_unknown(StreamId::Thread(t), "hook:2", raw());
    daemon.annex = Some(EventAnnex::Hook { truncated: true });
    unexpected(s.append(vec![daemon]), "a daemon-sourced raw_unknown");
    let mut other = registered(thread(2), false);
    other.source = source(SourceKind::Hook);
    other.annex = Some(EventAnnex::Hook { truncated: false });
    unexpected(s.append(vec![other]), "another kind");
}

/// The prompt annex is accepted only on a `user_message` whose source kind is `hook`.
#[test]
fn prompt_annex_only_on_a_hook_user_message() {
    use chaldea_core::ids::StreamId;
    use chaldea_core::payload::{Content, Encoding, SourceKind, UserMessage};

    use super::support::{TestStore, event, raw_unknown, registered, source, thread};
    use chaldea_core::event::TypedEvent;

    use crate::store::{AnnexError, StoreError, WriteError};

    let s = TestStore::new();
    let t = thread(1);
    s.append(vec![registered(t, false)]).unwrap();
    let message = |key: &str, kind: SourceKind| {
        let mut e = event(
            StreamId::Thread(t),
            key,
            TypedEvent::UserMessage(UserMessage {
                prompt: Content::text("p".to_owned()),
                input_id: None,
            }),
        );
        e.source = source(kind);
        e.annex = Some(EventAnnex::Prompt { superseded: true });
        e
    };
    s.append(vec![message("hook:1", SourceKind::Hook)])
        .expect("a hook user_message takes the annex");
    let unexpected = |r: Result<_, WriteError>, what: &str| {
        assert!(
            matches!(
                r,
                Err(WriteError::RolledBack(StoreError::Annex(
                    AnnexError::Unexpected(_)
                )))
            ),
            "{what}: {r:?}"
        );
    };
    unexpected(
        s.append(vec![message("daemon:1", SourceKind::Daemon)]),
        "a daemon-sourced user_message",
    );
    let mut raw = raw_unknown(
        StreamId::Thread(t),
        "hook:2",
        Content::Inline {
            inline: "x".into(),
            encoding: Encoding::Utf8,
        },
    );
    raw.source = source(SourceKind::Hook);
    raw.annex = Some(EventAnnex::Prompt { superseded: true });
    unexpected(s.append(vec![raw]), "a hook raw_unknown");
}

/// A counted set has one spelling: the fixed order, no repeats, known classes only.
#[test]
fn counted_classes_have_one_spelling() {
    let set = CountedClasses::of(&[CountedClass::Reply, CountedClass::Input]);
    assert_eq!(serde_json::to_string(&set).unwrap(), r#"["input","reply"]"#);
    assert_eq!(
        serde_json::to_string(&CountedClasses::default()).unwrap(),
        "[]"
    );
    for bad in [
        r#"["reply","input"]"#,
        r#"["input","input"]"#,
        r#"["input","other"]"#,
        r#""input""#,
    ] {
        assert!(
            serde_json::from_str::<CountedClasses>(bad).is_err(),
            "{bad}"
        );
    }
    for bad in [
        r#"{"readiness":{"launch_id":"0190c6f2-0000-7000-8000-000000000001","position":1,"counted":[],"extra":1}}"#,
        r#"{"other":"x"}"#,
    ] {
        assert!(serde_json::from_str::<EventAnnex>(bad).is_err(), "{bad}");
    }
}
