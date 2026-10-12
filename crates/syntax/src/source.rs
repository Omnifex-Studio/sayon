//! Source files, byte spans, and line/column lookup.

use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// A half-open byte range `[start, end)` inside one source file.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(file: FileId, start: usize, end: usize) -> Span {
        Span { file, start: start as u32, end: end as u32 }
    }

    /// The smallest span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        debug_assert_eq!(self.file, other.file);
        Span { file: self.file, start: self.start.min(other.start), end: self.end.max(other.end) }
    }

    pub fn empty_at_end(self) -> Span {
        Span { file: self.file, start: self.end, end: self.end }
    }

    pub fn len(self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}..{}", self.file.0, self.start, self.end)
    }
}

pub struct SourceFile {
    pub id: FileId,
    /// Path used when reporting, normally relative to the project or script root.
    pub name: String,
    /// Full path on disk, when the file came from disk.
    pub path: Option<PathBuf>,
    pub text: String,
    line_starts: Vec<u32>,
}

impl SourceFile {
    fn new(id: FileId, name: String, path: Option<PathBuf>, text: String) -> SourceFile {
        let mut line_starts = vec![0];
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(index as u32 + 1);
            }
        }
        SourceFile { id, name, path, text, line_starts }
    }

    /// Zero-based line index containing the byte offset.
    pub fn line_index(&self, offset: u32) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(line) => line,
            Err(next) => next - 1,
        }
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// The text of a zero-based line without its line terminator.
    pub fn line_text(&self, line: usize) -> &str {
        let start = self.line_starts[line] as usize;
        let end = self
            .line_starts
            .get(line + 1)
            .map(|&next| next as usize)
            .unwrap_or(self.text.len());
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }

    pub fn line_start(&self, line: usize) -> u32 {
        self.line_starts[line]
    }

    /// One-based line and column. Columns count Unicode scalar values.
    pub fn line_col(&self, offset: u32) -> (usize, usize) {
        let line = self.line_index(offset);
        let start = self.line_starts[line] as usize;
        let end = (offset as usize).min(self.text.len());
        let column = self.text.get(start..end).map(|s| s.chars().count()).unwrap_or(0);
        (line + 1, column + 1)
    }

    pub fn slice(&self, span: Span) -> &str {
        &self.text[span.start as usize..span.end as usize]
    }
}

#[derive(Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> SourceMap {
        SourceMap::default()
    }

    pub fn add(&mut self, name: impl Into<String>, path: Option<&Path>, text: String) -> FileId {
        let id = FileId(self.files.len() as u32);
        self.files.push(SourceFile::new(id, name.into(), path.map(Path::to_path_buf), text));
        id
    }

    pub fn get(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    pub fn files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter()
    }

    pub fn find_by_path(&self, path: &Path) -> Option<FileId> {
        self.files.iter().find(|file| file.path.as_deref() == Some(path)).map(|file| file.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_and_column_lookup() {
        let mut map = SourceMap::new();
        let id = map.add("a.sayon", None, "first\nsécond line\r\nthird".to_string());
        let file = map.get(id);
        assert_eq!(file.line_count(), 3);
        assert_eq!(file.line_col(0), (1, 1));
        assert_eq!(file.line_col(6), (2, 1));
        // "sé" is three bytes; the column counts scalar values.
        assert_eq!(file.line_col(9), (2, 3));
        assert_eq!(file.line_text(1), "sécond line");
        assert_eq!(file.line_text(2), "third");
    }
}
