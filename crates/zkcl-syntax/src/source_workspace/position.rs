//! Syntax positions use the shared snapshot's line and encoding rules.

use super::{SourcePosition, SourcePositionEncoding, SourceSnapshot};

impl SourceSnapshot {
    pub fn line_count(&self) -> usize {
        self.source().line_count()
    }
    pub fn line_text(&self, line: usize) -> Option<&str> {
        self.source().line_text(line)
    }
    pub fn position(&self, offset: usize) -> Option<SourcePosition> {
        self.source().position(offset)
    }
    pub fn position_with_encoding(
        &self,
        offset: usize,
        encoding: SourcePositionEncoding,
    ) -> Option<SourcePosition> {
        self.source().position_with_encoding(offset, encoding)
    }
    pub fn byte_offset(&self, position: SourcePosition) -> Option<usize> {
        self.source().byte_offset(position)
    }
    pub fn byte_offset_with_encoding(
        &self,
        position: SourcePosition,
        encoding: SourcePositionEncoding,
    ) -> Option<usize> {
        self.source().byte_offset_with_encoding(position, encoding)
    }
    pub fn clamped_byte_offset_with_encoding(
        &self,
        position: SourcePosition,
        encoding: SourcePositionEncoding,
    ) -> Option<usize> {
        self.source()
            .clamped_byte_offset_with_encoding(position, encoding)
    }
}
