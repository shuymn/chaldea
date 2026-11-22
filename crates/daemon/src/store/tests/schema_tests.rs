//! The shipped fingerprints match what the migrations create.

use crate::store::open::{expected_dump, fingerprint_of};
use crate::store::schema::{FINGERPRINTS, MIGRATIONS};

#[test]
fn shipped_fingerprints_match_the_migrations() {
    assert_eq!(FINGERPRINTS.len(), MIGRATIONS.len());
    for (i, shipped) in FINGERPRINTS.iter().enumerate() {
        let version = u32::try_from(i + 1).unwrap();
        let actual = fingerprint_of(&expected_dump(version).unwrap());
        assert_eq!(
            &actual, shipped,
            "version {version}: the DDL changed without a migration; the fingerprint is {actual}"
        );
    }
}
