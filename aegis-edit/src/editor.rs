use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferSource {
    Original,
    Add,
}

#[derive(Debug, Clone)]
pub struct Piece {
    pub source: BufferSource,
    pub start: usize,          // Byte start offset in the source buffer
    pub len: usize,            // Byte length in the source buffer
    pub grapheme_count: usize, // Cached count of extended grapheme clusters
}

#[derive(Debug, Clone)]
pub struct Line {
    pub pieces: Vec<Piece>,
}

pub struct Document {
    original_buffer: Arc<String>, // Loaded original content (immutable). Arc (not Rc) so Document is Send + Sync.
    add_buffer: String,           // Append-only buffer for newly typed text
    lines: Vec<Line>,             // Vector of lines (LOPT)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub start_line: usize,
    pub start_col: usize, // In extended graphemes
    pub end_line: usize,
    pub end_col: usize,   // In extended graphemes
    pub text: String,
}

impl Document {
    /// Creates a new Document from standard string content
    pub fn new(content: String) -> Self {
        let original_buffer = Arc::new(content);
        let mut lines = Vec::new();
        let mut start = 0;

        let mut splitter = LineSplitter::new(&original_buffer);
        while let Some((line_str, ended_with_newline)) = splitter.next() {
            let len = line_str.len() + if ended_with_newline { 1 } else { 0 };
            let graphemes = line_str.graphemes(true).count() + if ended_with_newline { 1 } else { 0 };

            lines.push(Line {
                pieces: vec![Piece {
                    source: BufferSource::Original,
                    start,
                    len,
                    grapheme_count: graphemes,
                }],
            });
            start += len;
        }

        // If the document is completely empty, add one empty line
        if lines.is_empty() {
            lines.push(Line { pieces: Vec::new() });
        }

        Self {
            original_buffer,
            add_buffer: String::new(),
            lines,
        }
    }

    /// Read a specific line from LOPT without re-allocating the entire file
    pub fn get_line(&self, line_idx: usize) -> Option<String> {
        let line = self.lines.get(line_idx)?;
        let mut content = String::new();

        for piece in &line.pieces {
            let buffer = match piece.source {
                BufferSource::Original => &*self.original_buffer,
                BufferSource::Add => &self.add_buffer,
            };
            content.push_str(&buffer[piece.start..piece.start + piece.len]);
        }
        Some(content)
    }

    /// Get total lines in the document
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Reconstructs the entire document into a single String
    pub fn render(&self) -> String {
        let mut content = String::new();
        for i in 0..self.lines.len() {
            if let Some(line_str) = self.get_line(i) {
                content.push_str(&line_str);
            }
        }
        content
    }

    /// Inserts text at a specific coordinate (line_idx, grapheme_col)
    pub fn insert(&mut self, line_idx: usize, grapheme_col: usize, text: &str) -> Result<(), &'static str> {
        if line_idx >= self.lines.len() {
            return Err("Line index out of bounds");
        }

        if text.is_empty() {
            return Ok(());
        }

        // 1. Append the new text to the add buffer
        let add_start = self.add_buffer.len();
        self.add_buffer.push_str(text);
        let add_len = text.len();
        let add_graphemes = text.graphemes(true).count();

        // 2. Handle insertion in an empty line
        if self.lines[line_idx].pieces.is_empty() {
            self.lines[line_idx].pieces.push(Piece {
                source: BufferSource::Add,
                start: add_start,
                len: add_len,
                grapheme_count: add_graphemes,
            });

            if text.contains('\n') {
                self.split_line_on_newlines(line_idx);
            }
            return Ok(());
        }

        // 3. Locate the piece to split (immutably borrow first to avoid borrowing conflicts)
        let (idx, split_offset_in_piece) = {
            let line = &self.lines[line_idx];
            let mut current_grapheme = 0;
            let mut piece_idx = None;
            let mut split_offset_in_piece = 0;

            for (i, piece) in line.pieces.iter().enumerate() {
                if current_grapheme <= grapheme_col && grapheme_col <= current_grapheme + piece.grapheme_count {
                    piece_idx = Some(i);
                    split_offset_in_piece = grapheme_col - current_grapheme;
                    break;
                }
                current_grapheme += piece.grapheme_count;
            }

            let idx = match piece_idx {
                Some(i) => i,
                None => {
                    if grapheme_col == current_grapheme {
                        line.pieces.len().saturating_sub(1)
                    } else {
                        return Err("Grapheme column out of bounds");
                    }
                }
            };
            (idx, split_offset_in_piece)
        };

        // Clone target piece to avoid concurrent borrow of self.lines
        let target_piece = self.lines[line_idx].pieces[idx].clone();
        let (p1, p2) = self.split_piece(&target_piece, split_offset_in_piece);

        let new_piece = Piece {
            source: BufferSource::Add,
            start: add_start,
            len: add_len,
            grapheme_count: add_graphemes,
        };

        // 4. Perform the pieces modification with a localized mutable borrow
        let line = &mut self.lines[line_idx];
        line.pieces.remove(idx);

        let mut inserted_count = 0;
        if let Some(left) = p1 {
            line.pieces.insert(idx, left);
            inserted_count += 1;
        }
        line.pieces.insert(idx + inserted_count, new_piece);
        inserted_count += 1;
        if let Some(right) = p2 {
            line.pieces.insert(idx + inserted_count, right);
        }

        // 5. Post-process: If the inserted text contains newlines, split the line in LOPT!
        if text.contains('\n') {
            self.split_line_on_newlines(line_idx);
        }

        Ok(())
    }

    /// Split a piece into two pieces based on a grapheme offset within that piece
    fn split_piece(&self, piece: &Piece, split_grapheme: usize) -> (Option<Piece>, Option<Piece>) {
        if split_grapheme == 0 {
            return (None, Some(piece.clone()));
        }
        if split_grapheme == piece.grapheme_count {
            return (Some(piece.clone()), None);
        }

        let buffer = match piece.source {
            BufferSource::Original => &*self.original_buffer,
            BufferSource::Add => &self.add_buffer,
        };
        let piece_str = &buffer[piece.start..piece.start + piece.len];

        // Find byte offset of the split grapheme
        let mut byte_split = 0;
        for (i, (byte_idx, _)) in piece_str.grapheme_indices(true).enumerate() {
            if i == split_grapheme {
                byte_split = byte_idx;
                break;
            }
        }

        let left = Piece {
            source: piece.source,
            start: piece.start,
            len: byte_split,
            grapheme_count: split_grapheme,
        };

        let right = Piece {
            source: piece.source,
            start: piece.start + byte_split,
            len: piece.len - byte_split,
            grapheme_count: piece.grapheme_count - split_grapheme,
        };

        (Some(left), Some(right))
    }

    /// Scan a line and split it into multiple lines if newlines were inserted
    fn split_line_on_newlines(&mut self, line_idx: usize) {
        let line = &self.lines[line_idx];
        let mut new_lines: Vec<Line> = Vec::new();
        let mut current_pieces: Vec<Piece> = Vec::new();

        for piece in &line.pieces {
            let buffer = match piece.source {
                BufferSource::Original => &*self.original_buffer,
                BufferSource::Add => &self.add_buffer,
            };
            let piece_str = &buffer[piece.start..piece.start + piece.len];

            if !piece_str.contains('\n') {
                current_pieces.push(piece.clone());
            } else {
                // Split the piece at each newline
                let mut remaining_str = piece_str;
                let mut remaining_start = piece.start;

                while let Some(pos) = remaining_str.find('\n') {
                    // Split is right after the '\n' byte (which is 1 byte long)
                    let segment_len = pos + 1;
                    let segment_graphemes = remaining_str[..segment_len].graphemes(true).count();

                    current_pieces.push(Piece {
                        source: piece.source,
                        start: remaining_start,
                        len: segment_len,
                        grapheme_count: segment_graphemes,
                    });

                    // Complete the current line and push to results
                    new_lines.push(Line { pieces: current_pieces });
                    current_pieces = Vec::new();

                    // Advance
                    remaining_str = &remaining_str[segment_len..];
                    remaining_start += segment_len;
                }

                // If there's content left after the last newline
                if !remaining_str.is_empty() {
                    let segment_graphemes = remaining_str.graphemes(true).count();
                    current_pieces.push(Piece {
                        source: piece.source,
                        start: remaining_start,
                        len: remaining_str.len(),
                        grapheme_count: segment_graphemes,
                    });
                }
            }
        }

        // Push the final remaining pieces as the last line
        if !current_pieces.is_empty() || new_lines.is_empty() {
            new_lines.push(Line { pieces: current_pieces });
        }

        // Splice the new lines into self.lines in place of the single line
        self.lines.splice(line_idx..=line_idx, new_lines);
    }

    /// Splits the pieces of a line at a given grapheme column, returning (left_pieces, right_pieces)
    fn split_line_pieces(&self, line_idx: usize, col: usize) -> (Vec<Piece>, Vec<Piece>) {
        let line = &self.lines[line_idx];
        let mut left_pieces = Vec::new();
        let mut right_pieces = Vec::new();
        let mut current_grapheme = 0;

        for piece in &line.pieces {
            let p_start = current_grapheme;
            let p_end = current_grapheme + piece.grapheme_count;

            if p_end <= col {
                left_pieces.push(piece.clone());
            } else if p_start >= col {
                right_pieces.push(piece.clone());
            } else {
                let split_offset = col - p_start;
                let (p_left, p_right) = self.split_piece(piece, split_offset);
                if let Some(left) = p_left {
                    left_pieces.push(left);
                }
                if let Some(right) = p_right {
                    right_pieces.push(right);
                }
            }
            current_grapheme = p_end;
        }

        (left_pieces, right_pieces)
    }

    /// Deletes `count` graphemes starting at `(line_idx, grapheme_col)`
    pub fn delete(&mut self, line_idx: usize, grapheme_col: usize, count: usize) -> Result<(), &'static str> {
        if line_idx >= self.lines.len() {
            return Err("Line index out of bounds");
        }
        if count == 0 {
            return Ok(());
        }

        // 1. Walk from start coordinate to locate the end coordinate of the deletion
        let mut end_line = line_idx;
        let mut end_col = grapheme_col;
        let mut remaining = count;

        while remaining > 0 && end_line < self.lines.len() {
            let line_graphemes = self.get_line_grapheme_count(end_line);
            let available = line_graphemes - end_col;

            if remaining <= available {
                end_col += remaining;
                remaining = 0;
            } else {
                remaining -= available;
                // Move to next line (which consumes the newline/line break)
                end_line += 1;
                end_col = 0;
            }
        }

        // Clamp end_line to the last valid line index if we reached the end of the document
        if end_line >= self.lines.len() {
            end_line = self.lines.len() - 1;
            end_col = self.get_line_grapheme_count(end_line);
        }

        // 2. Perform the deletion based on start and end coordinates
        if line_idx == end_line {
            // Delete within a single line
            let (left, _) = self.split_line_pieces(line_idx, grapheme_col);
            let (_, right) = self.split_line_pieces(line_idx, end_col);

            let mut merged_pieces = left;
            merged_pieces.extend(right);
            self.lines[line_idx].pieces = merged_pieces;
        } else {
            // Delete across multiple lines
            let (left, _) = self.split_line_pieces(line_idx, grapheme_col);
            let (_, right) = self.split_line_pieces(end_line, end_col);

            let mut merged_pieces = left;
            merged_pieces.extend(right);
            self.lines[line_idx].pieces = merged_pieces;

            // Remove the lines in between and the end line
            self.lines.drain((line_idx + 1)..=end_line);
        }

        // If the document is completely empty, add an empty line
        if self.lines.is_empty() {
            self.lines.push(Line { pieces: Vec::new() });
        }

        Ok(())
    }

    /// Calculate total graphemes in a line
    pub fn get_line_grapheme_count(&self, line_idx: usize) -> usize {
        self.lines.get(line_idx)
            .map(|l| l.pieces.iter().map(|p| p.grapheme_count).sum())
            .unwrap_or(0)
    }

    /// Applies multiple disjoint replacements safely using Back-to-Front Transactional Patching.
    ///
    /// Atomicity note: all bounds + overlap validation happens UP FRONT (steps 1-2)
    /// before any mutation, so a malformed/overlapping batch leaves the document
    /// untouched (the rollback property the tests check). Validated disjoint edits
    /// are then applied back-to-front so earlier offsets never shift.
    pub fn apply_transactional_edits(&mut self, mut edits: Vec<TextEdit>) -> Result<(), &'static str> {
        if edits.is_empty() {
            return Ok(());
        }

        // 1. Validate edits against bounds
        for edit in &edits {
            if edit.start_line >= self.lines.len() || edit.end_line >= self.lines.len() {
                return Err("Edit contains line index out of bounds");
            }
            if edit.start_line > edit.end_line || (edit.start_line == edit.end_line && edit.start_col > edit.end_col) {
                return Err("Edit coordinates are malformed (start is after end)");
            }
        }

        // 2. Verify there are no overlapping edits
        // Sort edits temporarily by start coordinate to verify overlap
        let mut sorted_by_start = edits.clone();
        sorted_by_start.sort_by(|a, b| {
            match a.start_line.cmp(&b.start_line) {
                std::cmp::Ordering::Equal => a.start_col.cmp(&b.start_col),
                other => other,
            }
        });

        for i in 0..sorted_by_start.len() - 1 {
            let curr = &sorted_by_start[i];
            let next = &sorted_by_start[i + 1];
            if curr.end_line > next.start_line || (curr.end_line == next.start_line && curr.end_col > next.start_col) {
                return Err("Edits overlap; transactional multi-replace requires disjoint regions");
            }
        }

        // 3. Sort edits in DESCENDING order (back-to-front) for execution
        edits.sort_by(|a, b| {
            match b.start_line.cmp(&a.start_line) {
                std::cmp::Ordering::Equal => b.start_col.cmp(&a.start_col),
                other => other,
            }
        });

        // 4. Apply edits sequentially
        for edit in edits {
            // A replacement is a deletion followed by an insertion

            // Calculate length of the target region to delete
            let mut delete_graphemes = 0;
            if edit.start_line == edit.end_line {
                delete_graphemes = edit.end_col - edit.start_col;
            } else {
                // First line partial graphemes remaining to delete (including its newline)
                let first_line_len = self.get_line_grapheme_count(edit.start_line);
                delete_graphemes += first_line_len - edit.start_col;

                // Full lines in between (each line counts as its graphemes plus its implicit newline)
                for l in (edit.start_line + 1)..edit.end_line {
                    delete_graphemes += self.get_line_grapheme_count(l);
                }

                // Last line partial graphemes to delete
                delete_graphemes += edit.end_col;
            }

            // Perform Deletion
            self.delete(edit.start_line, edit.start_col, delete_graphemes)?;

            // Perform Insertion
            self.insert(edit.start_line, edit.start_col, &edit.text)?;
        }

        Ok(())
    }
}

// A simple utility to split lines, handling carriage returns and end-of-file conditions
pub struct LineSplitter<'a> {
    text: &'a str,
    cursor: usize,
}

impl<'a> LineSplitter<'a> {
    pub fn new(text: &'a str) -> Self {
        Self { text, cursor: 0 }
    }
}

impl<'a> Iterator for LineSplitter<'a> {
    type Item = (&'a str, bool); // (Line string slice, whether it ended with newline)

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.text.len() {
            return None;
        }
        let remaining = &self.text[self.cursor..];
        if let Some(pos) = remaining.find('\n') {
            let line = &remaining[..pos];
            self.cursor += pos + 1;
            Some((line, true))
        } else {
            self.cursor += remaining.len();
            Some((remaining, false))
        }
    }
}
