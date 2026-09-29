//! Source files and byte-offset spans.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub lo: u32,
    pub hi: u32,
}

impl Span {
    pub fn new(lo: usize, hi: usize) -> Span {
        Span { lo: lo as u32, hi: hi as u32 }
    }

    /// The smallest span covering both.
    pub fn to(self, other: Span) -> Span {
        Span { lo: self.lo.min(other.lo), hi: self.hi.max(other.hi) }
    }
}

pub struct Source {
    /// Path as shown in diagnostics.
    pub path: String,
    pub text: String,
    line_starts: Vec<u32>,
}

impl Source {
    pub fn new(path: impl Into<String>, text: impl Into<String>) -> Source {
        let text = text.into();
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        Source { path: path.into(), text, line_starts }
    }

    /// 1-based line of a byte offset.
    pub fn line(&self, pos: u32) -> u32 {
        self.line_starts.partition_point(|&s| s <= pos) as u32
    }

    /// 1-based line and column (in characters) of a byte offset.
    pub fn line_col(&self, pos: u32) -> (u32, u32) {
        let line = self.line(pos);
        let start = self.line_starts[line as usize - 1] as usize;
        let end = (pos as usize).min(self.text.len());
        let col = self.text[start..end].chars().count() as u32 + 1;
        (line, col)
    }

    pub fn slice(&self, span: Span) -> &str {
        &self.text[span.lo as usize..span.hi as usize]
    }
}
