//! Snapshots, the mode word, and the grid encoding (`docs/design/pty-runtime.md` §2.6),
//! PR2-CL-12, CL-13, CL-23.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chaldea_core::payload::{Content, VtCursor, VtModes};
use chaldea_pty_runtime::fake::FakeScript;
use chaldea_pty_runtime::{
    Cell, CellWidth, Cursor, GridError, Limits, Line, PtySize, Snapshot, TermModes, decode_grid,
};
use common::*;

fn rig_sized(cols: u16, rows: u16) -> Rig {
    rig_with(FakeScript::default(), Limits::default(), cols, rows)
}

fn snap(r: &Rig) -> Snapshot {
    r.session.snapshot(soon()).unwrap()
}

fn grid(s: &Snapshot) -> String {
    match s.to_payload().grid {
        Content::Inline { inline, .. } => inline,
        other @ Content::Blob { .. } => panic!("{other:?}"),
    }
}

fn cell(text: &str, width: CellWidth) -> Cell {
    Cell {
        text: text.to_owned(),
        width,
    }
}

fn single(text: &str) -> Cell {
    cell(text, CellWidth::Single)
}

fn lead(text: &str) -> Cell {
    cell(text, CellWidth::WideLead)
}

fn tail() -> Cell {
    cell("", CellWidth::WideTail)
}

/// Every line has `cols` cells; every lead is followed by its tail and every tail
/// follows a lead; only tails are empty.
fn assert_well_formed(s: &Snapshot) {
    assert_eq!(s.lines.len(), usize::from(s.rows));
    for (r, line) in s.lines.iter().enumerate() {
        assert_eq!(line.cells.len(), usize::from(s.cols), "row {r}");
        for (c, x) in line.cells.iter().enumerate() {
            match x.width {
                CellWidth::WideLead => {
                    assert_eq!(
                        line.cells.get(c + 1).map(|n| n.width),
                        Some(CellWidth::WideTail),
                        "row {r} col {c}"
                    );
                }
                CellWidth::WideTail => {
                    assert!(
                        c > 0 && line.cells[c - 1].width == CellWidth::WideLead,
                        "row {r} col {c}"
                    );
                    assert!(x.text.is_empty());
                }
                CellWidth::Single => assert!(!x.text.is_empty(), "row {r} col {c}"),
            }
        }
    }
    let decoded = decode_grid(&grid(s), s.cols, s.rows).unwrap();
    assert_eq!(decoded, s.lines, "the grid round-trips");
}

/// PR2-CL-12: a wide character occupies a lead and a tail cell; columns after it are
/// exact.
#[test]
fn wide_chars_occupy_two_cells() {
    let r = rig_sized(10, 3);
    emit_parsed(&r, "a日b本".as_bytes());
    let s = snap(&r);
    assert_well_formed(&s);
    assert_eq!(
        s.lines[0].cells[..7],
        [
            single("a"),
            lead("日"),
            tail(),
            single("b"),
            lead("本"),
            tail(),
            single(" ")
        ]
    );
    assert_eq!(
        s.cursor,
        Cursor {
            row: 0,
            col: 6,
            visible: true
        }
    );
    assert_eq!(s.lines[0].text(), "a日b本");
    assert_eq!(grid(&s), r#"[["a","日","","b","本",""],[],[]]"#);
}

/// PR2-CL-12: a wide character that does not fit at the line end wraps, leaving a blank
/// cell.
#[test]
fn wrapped_wide_char_leaves_a_blank() {
    let r = rig_sized(5, 2);
    emit_parsed(&r, "abcd日".as_bytes());
    let s = snap(&r);
    assert_well_formed(&s);
    assert_eq!(s.lines[0].cells[4], single(" "));
    assert_eq!(s.lines[1].cells[..2], [lead("日"), tail()]);
    assert_eq!(grid(&s), r#"[["a","b","c","d"],["日",""]]"#);
}

/// PR2-CL-12: zero-width characters join their base cell.
#[test]
fn combining_mark_joins_its_base() {
    let r = rig_sized(6, 1);
    emit_parsed(&r, "e\u{301}x\u{2764}\u{fe0f}y".as_bytes());
    let s = snap(&r);
    assert_well_formed(&s);
    assert_eq!(
        s.lines[0].cells[..4],
        [
            single("e\u{301}"),
            single("x"),
            single("\u{2764}\u{fe0f}"),
            single("y")
        ]
    );
}

/// PR2-CL-12: erasing one half of a wide character leaves a blank `Single` for the other
/// half (literal wire values).
#[test]
fn erased_half_of_a_wide_char_is_blank() {
    let r = rig_sized(6, 2);
    // Row 1: ECH on the lead of 日; row 2: ECH on its tail.
    emit_parsed(
        &r,
        "日本x\r\n日本x\x1b[1;1H\x1b[1X\x1b[2;2H\x1b[1X".as_bytes(),
    );
    let s = snap(&r);
    assert_well_formed(&s);
    assert_eq!(grid(&s), r#"[[" "," ","本","","x"],[" "," ","本","","x"]]"#);
}

/// PR2-CL-12: shrinking the alternate screen across a wide character leaves no orphan.
#[test]
fn alt_screen_shrink_leaves_no_orphan() {
    let r = rig_sized(6, 2);
    emit_parsed(&r, "\x1b[?1049habcd日".as_bytes());
    assert_eq!(snap(&r).lines[0].cells[4..], [lead("日"), tail()]);
    r.session.resize(PtySize::new(5, 2).unwrap()).unwrap();
    let s = snap(&r);
    assert!(s.modes.alt_screen);
    assert_eq!(s.cols, 5);
    assert_well_formed(&s);
    assert_eq!(s.lines[0].cells[4], single(" "));
}

/// PR2-CL-13: the mode word follows DECSET 2004, 1049, 25, DECCKM, DECKPAM, and kitty.
#[test]
fn modes_follow_decset() {
    let r = rig();
    let m = r.session.modes().modes;
    assert_eq!(
        m,
        TermModes {
            show_cursor: true,
            ..TermModes::default()
        }
    );
    emit_parsed(&r, b"\x1b[?2004h\x1b[?1049h\x1b[?25l\x1b[?1h\x1b=\x1b[>5u");
    assert_eq!(
        r.session.modes().modes,
        TermModes {
            bracketed_paste: true,
            alt_screen: true,
            show_cursor: false,
            app_cursor: true,
            app_keypad: true,
            kitty_flags: 5,
        }
    );
    let s = snap(&r);
    assert_eq!(s.modes, r.session.modes().modes);
    assert!(!s.cursor.visible);
    emit_parsed(&r, b"\x1b[?2004l\x1b[?1049l\x1b[?25h\x1b[?1l\x1b>\x1b[<u");
    assert_eq!(r.session.modes().modes, m);
    assert_eq!(r.session.modes().bracketed_paste_epoch, 2);
}

/// PR2-CL-13: the epoch advances on every 2004 transition, also several in one read.
#[test]
fn epoch_counts_transitions_within_a_read() {
    let r = rig();
    emit_parsed(&r, b"\x1b[?2004h\x1b[?2004l\x1b[?2004h");
    let v = r.session.modes();
    assert!(v.modes.bracketed_paste);
    assert_eq!(v.bracketed_paste_epoch, 3);
    emit_parsed(&r, b"\x1b[?2004h");
    assert_eq!(
        r.session.modes().bracketed_paste_epoch,
        3,
        "no change, no epoch"
    );
}

/// PR2-CL-13: a synchronized update does not hide 2004 transitions inside it.
#[test]
fn sync_update_does_not_hide_2004() {
    let r = rig();
    emit_parsed(&r, b"\x1b[?2004h");
    assert_eq!(r.session.modes().bracketed_paste_epoch, 1);
    emit_parsed(&r, b"\x1b[?2026h\x1b[?2004l\x1b[?2004h\x1b[?2026l");
    let v = r.session.modes();
    assert!(v.modes.bracketed_paste);
    assert_eq!(v.bracketed_paste_epoch, 3);
}

/// PR2-CL-13: a query inside an unterminated synchronized update is still answered.
#[test]
fn unterminated_sync_update_still_answers() {
    let r = rig();
    r.child.emit(b"\x1b[?2026h\x1b[5n");
    assert!(wait_until(|| r.child.writes() == vec![b"\x1b[0n".to_vec()]));
}

/// PR2-CL-13: every loaded word is one the drain published: the 2004 bit always agrees
/// with its epoch's parity, and epochs never go back.
#[test]
fn mode_word_is_never_torn() {
    let r = rig();
    let session = Arc::new(r.session);
    let stop = Arc::new(AtomicBool::new(false));
    let (s2, stop2) = (session.clone(), stop.clone());
    let reader = std::thread::spawn(move || {
        let mut last = 0;
        let mut n = 0u64;
        while !stop2.load(Ordering::SeqCst) {
            let v = s2.modes();
            assert_eq!(
                v.modes.bracketed_paste,
                v.bracketed_paste_epoch % 2 == 1,
                "{v:?}"
            );
            assert!(v.modes.show_cursor && !v.modes.app_cursor, "{v:?}");
            assert!(v.bracketed_paste_epoch >= last, "{v:?} after {last}");
            last = v.bracketed_paste_epoch;
            n += 1;
        }
        (n, last)
    });
    let rounds = 500u64;
    for _ in 0..rounds {
        r.child
            .emit(b"\x1b[?2004h\x1b[?1049h\x1b[?2004l\x1b[?1049l");
    }
    wait_parsed(&session, 32 * rounds);
    stop.store(true, Ordering::SeqCst);
    let (loads, last) = reader.join().unwrap();
    assert!(loads > 0);
    assert_eq!(last, 2 * rounds);
}

/// PR2-CL-23: the payload carries the snapshot's size, cursor, modes, and grid.
#[test]
fn payload_matches_the_snapshot() {
    let r = rig_sized(8, 3);
    emit_parsed(&r, "\x1b[?2004h> 日本\r\n\x1b[?1049h  x".as_bytes());
    let s = snap(&r);
    let p = s.to_payload();
    assert_eq!((p.cols, p.rows), (8, 3));
    assert_eq!(
        p.cursor,
        VtCursor {
            row: s.cursor.row,
            col: s.cursor.col,
            visible: true
        }
    );
    assert_eq!(
        p.modes,
        VtModes {
            bracketed_paste: true,
            alternate_screen: true
        }
    );
    assert_eq!(decode_grid(&grid(&s), 8, 3).unwrap(), s.lines);
    assert_well_formed(&s);
}

fn line(cells: &[Cell], cols: usize) -> Line {
    let mut cells = cells.to_vec();
    cells.resize(cols, single(" "));
    Line { cells }
}

/// PR2-CL-23: the §2.6 examples, as literal strings, both ways.
#[test]
fn grid_examples() {
    let mk = |lines: Vec<Line>| Snapshot {
        cols: 5,
        rows: 2,
        lines,
        cursor: Cursor {
            row: 0,
            col: 0,
            visible: true,
        },
        modes: TermModes::default(),
        output_offset: 0,
    };
    let a = vec![
        line(&[single("a"), lead("日"), tail(), single("b")], 5),
        line(&[], 5),
    ];
    assert_eq!(grid(&mk(a.clone())), r#"[["a","日","","b"],[]]"#);
    assert_eq!(decode_grid(r#"[["a","日","","b"],[]]"#, 5, 2).unwrap(), a);
    let x = vec![
        line(&[single(" "), single(" "), single("x")], 5),
        line(&[], 5),
    ];
    assert_eq!(grid(&mk(x.clone())), r#"[[" "," ","x"],[]]"#);
    assert_eq!(decode_grid(r#"[[" "," ","x"],[]]"#, 5, 2).unwrap(), x);

    // The same screens through the VT.
    let r = rig_sized(5, 2);
    emit_parsed(&r, "a日b".as_bytes());
    assert_eq!(grid(&snap(&r)), r#"[["a","日","","b"],[]]"#);
    let r = rig_sized(5, 2);
    emit_parsed(&r, b"\x1b[1;3Hx");
    assert_eq!(grid(&snap(&r)), r#"[[" "," ","x"],[]]"#);
}

/// PR2-CL-23: every malformed shape of §2.6 is rejected.
#[test]
fn malformed_grids_are_rejected() {
    type Check = fn(&GridError) -> bool;
    let cases: [(&str, Check); 11] = [
        ("not json", |e| matches!(e, GridError::NotJson(_))),
        (r#"{"rows":[]}"#, |e| *e == GridError::NotAnArray),
        ("[[]]", |e| {
            matches!(
                e,
                GridError::RowCount {
                    expected: 2,
                    got: 1
                }
            )
        }),
        ("[[],[],[]]", |e| {
            matches!(
                e,
                GridError::RowCount {
                    expected: 2,
                    got: 3
                }
            )
        }),
        (r#"["a",[]]"#, |e| *e == GridError::RowNotAnArray { row: 0 }),
        (r#"[["a","b","c","d"],[]]"#, |e| {
            matches!(e, GridError::RowTooLong { row: 0, cells: 4 })
        }),
        (r#"[[1],[]]"#, |e| {
            *e == GridError::NotAString { row: 0, col: 0 }
        }),
        (r#"[[""],[]]"#, |e| {
            *e == GridError::EmptyWithoutLead { row: 0, col: 0 }
        }),
        (r#"[["日","",""],[]]"#, |e| {
            *e == GridError::EmptyWithoutLead { row: 0, col: 2 }
        }),
        (r#"[[],["a","日"]]"#, |e| {
            *e == GridError::LeadWithoutTail { row: 1, col: 1 }
        }),
        (r#"[["日","x"],[]]"#, |e| {
            *e == GridError::LeadWithoutTail { row: 0, col: 0 }
        }),
    ];
    for (g, ok) in cases {
        let e = decode_grid(g, 3, 2).unwrap_err();
        assert!(ok(&e), "{g}: {e:?}");
    }
    // A wide pair ending exactly at the last column is well formed.
    assert!(decode_grid(r#"[["a","日",""],[]]"#, 3, 2).is_ok());
}

/// PR2-CL-23: the `core` golden grids (`vt_snapshot_taken` and `Component::VtSnapshot`)
/// are readable in the §2.6 encoding.
#[test]
fn core_goldens_are_valid_grids() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/golden/events");
    for (file, at) in [
        ("vt_snapshot_taken.json", "/payload"),
        ("surface_update.json", "/payload/components/3"),
    ] {
        let text = std::fs::read_to_string(dir.join(file)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let p = v.pointer(at).unwrap();
        let cols = u16::try_from(p["cols"].as_u64().unwrap()).unwrap();
        let rows = u16::try_from(p["rows"].as_u64().unwrap()).unwrap();
        let lines = decode_grid(p["grid"]["inline"].as_str().unwrap(), cols, rows).unwrap();
        assert_eq!(lines.len(), usize::from(rows), "{file}");
        assert!(!lines[0].text().is_empty(), "{file}");
    }
}
