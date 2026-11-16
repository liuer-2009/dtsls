use ropey::Rope;
use tower_lsp::lsp_types::Position;

/// Convert an LSP position (UTF-16 line/character) to a byte offset in the rope.
pub fn position_to_byte(rope: &Rope, pos: Position) -> usize {
    let line_idx = (pos.line as usize).min(rope.len_lines().saturating_sub(1));
    let line = rope.line(line_idx);
    let character = (pos.character as usize).min(line.len_utf16_cu());
    let char_in_line = line.utf16_cu_to_char(character);
    rope.line_to_byte(line_idx) + line.char_to_byte(char_in_line)
}

/// Convert a byte offset in the rope to an LSP position (UTF-16 line/character).
pub fn byte_to_position(rope: &Rope, byte: usize) -> Position {
    let byte = byte.min(rope.len_bytes());
    let line_idx = rope.byte_to_line(byte);
    let char_idx = rope.byte_to_char(byte);
    let character = rope.line(line_idx).char_to_utf16_cu(char_idx - rope.line_to_char(line_idx));
    Position {
        line: line_idx as u32,
        character: character as u32,
    }
}

/// Convert a byte offset to a tree-sitter point (row = line, column = byte offset within line).
pub fn byte_to_point(rope: &Rope, byte: usize) -> tree_sitter::Point {
    let byte = byte.min(rope.len_bytes());
    let line_idx = rope.byte_to_line(byte);
    tree_sitter::Point {
        row: line_idx,
        column: byte - rope.line_to_byte(line_idx),
    }
}
