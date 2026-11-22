//! The annex's persisted JSON (PR6-CL-23).

use crate::store::{Continuity, CountedClass, CountedClasses, EventAnnex, WriteBasis};

/// Both variants serialize to exactly the JSON of §2.6 and read back.
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
