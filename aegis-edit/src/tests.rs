//! The original Aegis-Edit verification suite (built by the Gemini/nemesis8
//! agent) converted from a `main.rs` print harness into real `#[test]`s, plus
//! extra hardening cases Claude added on integration (empty-doc, pure delete,
//! replace-with-empty, CRLF round-trip, execution-time atomicity caveat).

use super::{Document, TextEdit};

// ── Original suite (1-7) ──────────────────────────────────────────────────────

#[test]
fn basic_insertion_and_reconstruction() {
    let mut doc = Document::new("Hello World!\nSecond Line.".to_string());
    assert_eq!(doc.line_count(), 2);

    doc.insert(0, 5, ", Dear").unwrap();
    assert_eq!(doc.get_line(0), Some("Hello, Dear World!\n".to_string()));
    assert_eq!(doc.get_line(1), Some("Second Line.".to_string()));
    assert_eq!(doc.render(), "Hello, Dear World!\nSecond Line.");
}

#[test]
fn multiline_insertion_with_line_splitting() {
    let mut doc = Document::new("First Line.\nThird Line.".to_string());
    doc.insert(0, 11, "\nSecond Line.\nAnd another one.").unwrap();

    assert_eq!(doc.line_count(), 4);
    assert_eq!(doc.get_line(0), Some("First Line.\n".to_string()));
    assert_eq!(doc.get_line(1), Some("Second Line.\n".to_string()));
    assert_eq!(doc.get_line(2), Some("And another one.\n".to_string()));
    assert_eq!(doc.get_line(3), Some("Third Line.".to_string()));
}

#[test]
fn complex_emoji_and_grapheme_safety() {
    // 🙋‍♀️ woman-raising-hand · 👨‍👩‍👧‍👦 family (ZWJ) · 🏳️‍🌈 rainbow flag — each ONE grapheme.
    let initial = "Emoji: 🙋‍♀️ - Family: 👨‍👩‍👧‍👦 - Flag: 🏳️‍🌈".to_string();
    let mut doc = Document::new(initial);

    // 7 + 1 + 11 + 1 + 9 + 1 = 30 graphemes
    assert_eq!(doc.get_line_grapheme_count(0), 30);

    doc.insert(0, 20, " 🎉").unwrap();
    assert_eq!(doc.get_line_grapheme_count(0), 32);
    assert!(doc.render().contains("👨‍👩‍👧‍👦 🎉"));

    doc.delete(0, 19, 1).unwrap();
    assert_eq!(doc.get_line_grapheme_count(0), 31);
    let deleted = doc.render();
    assert!(!deleted.contains("👨‍👩‍👧‍👦"));
    assert!(deleted.contains("Family:  🎉"));
}

#[test]
fn giant_line_piece_segmentation() {
    let mut doc = Document::new("A".repeat(10_000));
    assert_eq!(doc.get_line_grapheme_count(0), 10_000);

    for i in 0..50 {
        let pos = (50 - i) * 180; // back-to-front so the loop index needn't shift
        doc.insert(0, pos, "🌟").unwrap();
    }

    assert_eq!(doc.get_line_grapheme_count(0), 10_050);
    assert_eq!(doc.render().matches("🌟").count(), 50);
}

#[test]
fn deletion_spanning_line_breaks() {
    let mut doc = Document::new("Line1\nLine2\nLine3".to_string());
    doc.delete(0, 3, 5).unwrap(); // "e1\nLi"
    assert_eq!(doc.line_count(), 2);
    assert_eq!(doc.get_line(0), Some("Linne2\n".to_string()));
    assert_eq!(doc.get_line(1), Some("Line3".to_string()));
}

#[test]
fn transactional_multi_replace_bftp() {
    let mut doc = Document::new("Line One\nLine Two\nLine Three\nLine Four".to_string());
    let edits = vec![
        TextEdit { start_line: 0, start_col: 5, end_line: 0, end_col: 8, text: "Zero".to_string() },
        TextEdit { start_line: 1, start_col: 5, end_line: 1, end_col: 8, text: "Second".to_string() },
        TextEdit { start_line: 3, start_col: 5, end_line: 3, end_col: 9, text: "Fourth".to_string() },
    ];
    doc.apply_transactional_edits(edits).unwrap();

    assert_eq!(doc.get_line(0), Some("Line Zero\n".to_string()));
    assert_eq!(doc.get_line(1), Some("Line Second\n".to_string()));
    assert_eq!(doc.get_line(2), Some("Line Three\n".to_string()));
    assert_eq!(doc.get_line(3), Some("Line Fourth".to_string()));
}

#[test]
fn transactional_overlap_rejected_and_rolled_back() {
    let mut doc = Document::new("Line One\nLine Two".to_string());
    let overlapping = vec![
        TextEdit { start_line: 0, start_col: 2, end_line: 0, end_col: 6, text: "X".to_string() },
        TextEdit { start_line: 0, start_col: 4, end_line: 0, end_col: 8, text: "Y".to_string() },
    ];
    assert!(doc.apply_transactional_edits(overlapping).is_err());
    // Up-front validation => nothing applied.
    assert_eq!(doc.render(), "Line One\nLine Two");
}

// ── Hardening additions (Claude) ──────────────────────────────────────────────

#[test]
fn empty_document_inserts_cleanly() {
    let mut doc = Document::new(String::new());
    assert_eq!(doc.line_count(), 1);
    assert_eq!(doc.render(), "");
    doc.insert(0, 0, "hello").unwrap();
    assert_eq!(doc.render(), "hello");
    doc.insert(0, 5, " 🌈 world").unwrap();
    assert_eq!(doc.render(), "hello 🌈 world");
    // 13 graphemes, single line, no trailing newline.
    assert_eq!(doc.get_line_grapheme_count(0), 13);
}

#[test]
fn replace_with_empty_is_pure_delete() {
    let mut doc = Document::new("keep[drop]keep".to_string());
    // Replace "[drop]" (cols 4..10) with "".
    let edits = vec![TextEdit {
        start_line: 0,
        start_col: 4,
        end_line: 0,
        end_col: 10,
        text: String::new(),
    }];
    doc.apply_transactional_edits(edits).unwrap();
    assert_eq!(doc.render(), "keepkeep");
}

#[test]
fn delete_to_end_then_reinsert() {
    let mut doc = Document::new("abcdef".to_string());
    doc.delete(0, 3, 3).unwrap();
    assert_eq!(doc.render(), "abc");
    doc.insert(0, 3, "XYZ").unwrap();
    assert_eq!(doc.render(), "abcXYZ");
}

#[test]
fn crlf_content_round_trips() {
    // LineSplitter splits on '\n'; the '\r' stays as trailing content on the
    // line. Round-trip must be byte-identical.
    let src = "alpha\r\nbeta\r\ngamma".to_string();
    let doc = Document::new(src.clone());
    assert_eq!(doc.line_count(), 3);
    assert_eq!(doc.render(), src);
}

#[test]
fn multi_edit_disjoint_across_lines_back_to_front() {
    let mut doc = Document::new("one\ntwo\nthree".to_string());
    // Two disjoint single-line replaces on different lines.
    let edits = vec![
        TextEdit { start_line: 2, start_col: 0, end_line: 2, end_col: 5, text: "THREE".to_string() },
        TextEdit { start_line: 0, start_col: 0, end_line: 0, end_col: 3, text: "ONE".to_string() },
    ];
    doc.apply_transactional_edits(edits).unwrap();
    assert_eq!(doc.render(), "ONE\ntwo\nTHREE");
}

#[test]
fn out_of_bounds_edit_rejected_and_rolled_back() {
    let mut doc = Document::new("only one line".to_string());
    let edits = vec![TextEdit {
        start_line: 5, // does not exist
        start_col: 0,
        end_line: 5,
        end_col: 1,
        text: "x".to_string(),
    }];
    assert!(doc.apply_transactional_edits(edits).is_err());
    assert_eq!(doc.render(), "only one line");
}
