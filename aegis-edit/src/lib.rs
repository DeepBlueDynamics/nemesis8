//! Aegis-Edit — line-oriented piece-table (LOPT) text buffer for Hyperia.
//!
//! A `Document` holds an immutable original buffer (`Arc<String>`) + an
//! append-only add buffer, indexed as a vector of lines, each a list of
//! `Piece`s that slice into one of the two buffers. All coordinates are in
//! **extended grapheme clusters** (ZWJ emoji, flags, accents are one column),
//! so edits never split a codepoint — the principled fix for the byte-slice
//! panics elsewhere in the codebase.
//!
//! Two edit paths:
//!   * [`Document::insert`] / [`Document::delete`] — single grapheme-addressed op.
//!   * [`Document::apply_transactional_edits`] — Back-to-Front Transactional
//!     Patching (BFTP): validate a batch of disjoint [`TextEdit`]s up front,
//!     then apply them back-to-front so earlier offsets never shift. Used as
//!     the basis for multi-agent / locked-block sticky editing.

mod editor;

pub use editor::{BufferSource, Document, Line, LineSplitter, Piece, TextEdit};

// Compile-time guarantee that Document can cross threads / live in shared
// sidecar state. This is why the original buffer is Arc, not Rc — if someone
// reverts that, this line fails to compile.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Document>();
};

#[cfg(test)]
mod tests;
