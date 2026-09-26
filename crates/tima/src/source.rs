use std::fmt;

/// A byte range in the single source file currently being compiled.
///
/// Byte offsets keep lexer/parser data compact. `SourceFile` owns all line
/// mapping so later compilation stages can retain exact spans without copying
/// line/column pairs through every representation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self {
            start: start as u32,
            end: end as u32,
        }
    }

    pub fn join(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[derive(Clone)]
pub struct SourceFile {
    name: String,
    text: String,
    line_starts: Vec<u32>,
}

impl SourceFile {
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        let mut line_starts = vec![0];
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push((index + 1) as u32);
            }
        }
        Self {
            name: name.into(),
            text,
            line_starts,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn slice(&self, span: Span) -> &str {
        &self.text[span.start as usize..span.end as usize]
    }

    /// Returns one-based `(line, column)` coordinates.
    pub fn location(&self, offset: u32) -> (usize, usize) {
        let line_index = self
            .line_starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        let column = offset.saturating_sub(self.line_starts[line_index]) as usize + 1;
        (line_index + 1, column)
    }

    pub fn line_text(&self, one_based_line: usize) -> Option<&str> {
        let start = *self.line_starts.get(one_based_line.checked_sub(1)?)? as usize;
        let end = self
            .line_starts
            .get(one_based_line)
            .map(|value| *value as usize)
            .unwrap_or(self.text.len());
        Some(self.text[start..end].trim_end_matches(['\r', '\n']))
    }
}

impl fmt::Debug for SourceFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceFile")
            .field("name", &self.name)
            .field("bytes", &self.text.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_byte_offsets_to_lines_and_columns() {
        let source = SourceFile::new("test", "one\ntwo\n");
        assert_eq!(source.location(0), (1, 1));
        assert_eq!(source.location(4), (2, 1));
        assert_eq!(source.location(6), (2, 3));
    }
}
