//! PR8-CL-1: the state directory is created private with a complete marker, adopted only when
//! empty or marked, and refused, unchanged, when linked, foreign, open, or unmarked.

use std::path::Path;

use chaldea_pty_runtime::host::{self, EntryKind, Privacy};

use super::super::names;
use super::super::state_dir::{MARKER_CONTENT, prepare, publish_marker};

const INSTANCE: &str = "00000000-0000-4000-8000-000000000002";

fn listing(d: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(d)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let bytes = std::fs::read(e.path()).unwrap_or_default();
            (e.file_name().to_string_lossy().into_owned(), bytes)
        })
        .collect();
    v.sort();
    v
}

#[test]
fn creates_private() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("state");
    let canonical = prepare(&d, INSTANCE).unwrap();
    assert_eq!(canonical, std::fs::canonicalize(&d).unwrap());
    assert_eq!(
        host::privacy(&d, EntryKind::Directory).unwrap(),
        Privacy::Private
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
    #[cfg(windows)]
    {
        let (owner, allows) = chaldea_pty_runtime::fake::descriptor_sids(&d).unwrap();
        let me = chaldea_pty_runtime::fake::user_sid().unwrap();
        assert_eq!(owner, me);
        let mut a = allows;
        a.sort();
        let mut want = vec![me, "S-1-5-18".to_owned()];
        want.sort();
        assert_eq!(a, want);
    }
    assert_eq!(
        listing(&d),
        vec![(names::MARKER.to_owned(), MARKER_CONTENT.to_vec())]
    );
}

#[test]
fn refuses_a_linked_final_component() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("real");
    std::fs::create_dir(&target).unwrap();
    let link = tmp.path().join("state");
    chaldea_pty_runtime::fake::make_link(&link, &target).unwrap();
    let e = prepare(&link, INSTANCE).unwrap_err();
    assert!(e.0.contains("link"), "{}", e.0);
    assert!(listing(&target).is_empty());
}

#[test]
fn adopts_empty_and_marked() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("state");
    host::create_private_dir(&d).unwrap();
    prepare(&d, INSTANCE).unwrap();
    assert!(d.join(names::MARKER).exists());
    // Marked, with other files: adopted again, unchanged.
    std::fs::write(d.join("port"), "7373\n").unwrap();
    let before = listing(&d);
    prepare(&d, INSTANCE).unwrap();
    assert_eq!(listing(&d), before);
}

/// A crash between the marker temporary and its link never makes the directory unadoptable.
#[test]
fn adopts_with_only_a_marker_temp() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("state");
    host::create_private_dir(&d).unwrap();
    std::fs::write(d.join(".CHALDEA_STATE.tmp-dead"), "{").unwrap();
    prepare(&d, INSTANCE).unwrap();
    assert_eq!(
        std::fs::read(d.join(names::MARKER)).unwrap(),
        MARKER_CONTENT
    );
}

/// Publishing over an existing marker with other content (a concurrent writer that won): the
/// link fails, the file is unchanged, no temporary is left, and the marker is validated and
/// refused.
#[test]
fn marker_link_does_not_replace() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("state");
    host::create_private_dir(&d).unwrap();
    std::fs::write(d.join(names::MARKER), "not ours").unwrap();
    let e = publish_marker(&d, INSTANCE).unwrap_err();
    assert!(e.contains("carries a marker that is not Chaldea's"), "{e}");
    assert_eq!(std::fs::read(d.join(names::MARKER)).unwrap(), b"not ours");
    assert_eq!(listing(&d).len(), 1, "{:?}", listing(&d));
    // Through `prepare`, the non-empty directory is validated and refused the same way.
    let e = prepare(&d, INSTANCE).unwrap_err();
    assert!(e.0.contains("not a Chaldea state directory"), "{}", e.0);
}

#[test]
fn refuses_unmarked_nonempty() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("state");
    host::create_private_dir(&d).unwrap();
    std::fs::write(d.join("port"), "7373\n").unwrap();
    let e = prepare(&d, INSTANCE).unwrap_err();
    assert!(e.0.contains("not a Chaldea state directory"), "{}", e.0);
    assert_eq!(listing(&d), vec![("port".to_owned(), b"7373\n".to_vec())]);
}

/// A directory owned by another user reports `NotOwned` before any mode check.
#[cfg(target_os = "macos")]
#[test]
fn refuses_not_owned_first() {
    assert_eq!(
        host::privacy(Path::new("/usr"), EntryKind::Directory).unwrap(),
        Privacy::NotOwned
    );
    let e = prepare(Path::new("/usr"), INSTANCE).unwrap_err();
    assert!(e.0.contains("not owned"), "{}", e.0);
}

#[cfg(unix)]
#[test]
fn refuses_group_readable() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("state");
    std::fs::create_dir(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o750)).unwrap();
    let e = prepare(&d, INSTANCE).unwrap_err();
    assert!(e.0.contains("chmod 700"), "{}", e.0);
    assert!(listing(&d).is_empty());
    assert_eq!(
        std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
        0o750,
        "an existing directory is never chmodded"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn refuses_acl_allow_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("state");
    host::create_private_dir(&d).unwrap();
    chaldea_pty_runtime::fake::grant_everyone_read_acl(&d).unwrap();
    assert_eq!(
        host::privacy(&d, EntryKind::Directory).unwrap(),
        Privacy::AclGrantsOthers
    );
    let e = prepare(&d, INSTANCE).unwrap_err();
    assert!(e.0.contains("access control list"), "{}", e.0);
    assert!(listing(&d).is_empty());
}

/// A directory under `FOLDERID_LocalAppData` with an allow ACE for `BUILTIN\Users`.
#[cfg(windows)]
#[test]
fn refuses_users_ace_under_profile() {
    let base = chaldea_pty_runtime::host::default_state_base().unwrap();
    let tmp = tempfile::tempdir_in(base).unwrap();
    let d = tmp.path().join("state");
    host::create_private_dir(&d).unwrap();
    chaldea_pty_runtime::fake::grant_users_read(&d).unwrap();
    assert_eq!(
        host::privacy(&d, EntryKind::Directory).unwrap(),
        Privacy::AclGrantsOthers
    );
    let e = prepare(&d, INSTANCE).unwrap_err();
    assert!(e.0.contains("access control list"), "{}", e.0);
    assert!(listing(&d).is_empty());
}

/// The existing Public folder (`FOLDERID_Public`), not created by the test, is refused.
#[cfg(windows)]
#[test]
fn refuses_existing_public_dir() {
    let public = chaldea_pty_runtime::fake::public_folder().unwrap();
    assert!(public.is_dir());
    let verdict = host::privacy(&public, EntryKind::Directory).unwrap();
    assert_ne!(verdict, Privacy::Private);
    let e = prepare(&public, INSTANCE).unwrap_err();
    assert!(e.0.contains("not private"), "{}", e.0);
    assert!(!public.join(names::MARKER).exists());
}

// ---- Review round 1 ----

/// A trailing separator does not make a linked final component look like its target: still
/// refused, the target untouched.
#[test]
fn refuses_a_linked_final_component_with_a_trailing_separator() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("real");
    host::create_private_dir(&target).unwrap();
    let link = tmp.path().join("state");
    chaldea_pty_runtime::fake::make_link(&link, &target).unwrap();
    let mut seps = vec!["/"];
    if cfg!(windows) {
        seps.push("\\");
    }
    for sep in seps {
        let mut spelling = link.clone().into_os_string();
        spelling.push(sep);
        let e = prepare(Path::new(&spelling), INSTANCE).unwrap_err();
        assert!(e.0.contains("link"), "{sep}: {}", e.0);
        assert!(listing(&target).is_empty(), "{sep}");
    }
}

/// The unfinished-marker exception covers only a regular file: a directory of that name
/// makes the directory non-empty.
#[test]
fn marker_temp_exception_is_a_regular_file() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("state");
    host::create_private_dir(&d).unwrap();
    std::fs::create_dir(d.join(".CHALDEA_STATE.tmp-dead")).unwrap();
    let e = prepare(&d, INSTANCE).unwrap_err();
    assert!(e.0.contains("not a Chaldea state directory"), "{}", e.0);
    assert!(!d.join(names::MARKER).exists());
}

/// A FIFO at `CHALDEA_STATE` is refused at once instead of blocking the marker check.
#[cfg(target_os = "macos")]
#[test]
fn fifo_marker_does_not_block() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("state");
    host::create_private_dir(&d).unwrap();
    let fifo = d.join(names::MARKER);
    super::files::mkfifo(&fifo);
    let p = d.clone();
    let refused = super::files::without_blocking(&fifo, move || prepare(&p, INSTANCE).is_err());
    assert_eq!(refused, Some(true));
}

/// Windows: a state directory path longer than `MAX_PATH` (260) is created and inspected
/// alike, private.
#[cfg(windows)]
#[test]
fn creates_a_state_dir_beyond_max_path() {
    let tmp = tempfile::tempdir_in(host::default_state_base().unwrap()).unwrap();
    let mut parent = tmp.path().to_path_buf();
    for c in ['a', 'b', 'c'] {
        parent.push(c.to_string().repeat(90));
    }
    std::fs::create_dir_all(&parent).unwrap();
    let d = parent.join("state");
    assert!(d.as_os_str().len() > 260, "{}", d.as_os_str().len());
    let canonical = prepare(&d, INSTANCE).unwrap();
    assert!(canonical.ends_with("state"));
    assert_eq!(
        host::privacy(&d, EntryKind::Directory).unwrap(),
        Privacy::Private
    );
    assert_eq!(
        std::fs::read(d.join(names::MARKER)).unwrap(),
        MARKER_CONTENT
    );
}

// ---- Review round 2 ----

/// Only trailing separators go: a root keeps its own, every other component stays as given.
#[test]
fn strips_only_trailing_separators() {
    use super::super::state_dir::without_trailing_separators as trim;
    #[cfg(unix)]
    let cases = [
        ("/work/state/", "/work/state"),
        ("/work/state//", "/work/state"),
        ("/work/x/../state/", "/work/x/../state"),
        ("/", "/"),
    ];
    #[cfg(windows)]
    let cases = [
        (r"C:\work\state\", r"C:\work\state"),
        (r"C:\work\state/", r"C:\work\state"),
        (
            r"\\?\C:\work\missing\..\state\",
            r"\\?\C:\work\missing\..\state",
        ),
        (r"\\?\C:\", r"\\?\C:\"),
        (r"C:\", r"C:\"),
    ];
    for (given, want) in cases {
        assert_eq!(trim(Path::new(given)).as_os_str(), want, "{given}");
    }
}

/// A verbatim path with a `..` component is not resolved into another directory (Windows
/// does not interpret `..` in that syntax).
#[cfg(windows)]
#[test]
fn refuses_verbatim_path_with_parent_component() {
    let tmp = tempfile::tempdir_in(host::default_state_base().unwrap()).unwrap();
    let parent = std::fs::canonicalize(tmp.path()).unwrap();
    let mut raw = parent.as_os_str().to_os_string();
    raw.push(r"\missing\..\state");
    let path = Path::new(&raw);

    assert!(std::fs::symlink_metadata(path).is_err());
    let result = prepare(path, INSTANCE);
    assert!(result.is_err(), "{result:?}");
    assert!(!parent.join("state").exists());
}
