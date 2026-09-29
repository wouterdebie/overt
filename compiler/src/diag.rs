//! Diagnostics: one line each, `path:line:col: error: message`.
//!
//! Messages carry their own fix when one is known ("...; did you mean X"), so an
//! agent can act on the line without looking anything up.

use crate::source::{Source, Span};

#[derive(Clone, Debug)]
pub struct Diag {
    pub span: Span,
    pub msg: String,
    /// Which file of the program the span is in.
    pub file: usize,
}

impl Diag {
    pub fn new(span: Span, msg: impl Into<String>) -> Diag {
        Diag { span, msg: msg.into(), file: 0 }
    }

    pub fn in_file(mut self, file: usize) -> Diag {
        self.file = file;
        self
    }

    pub fn render(&self, src: &Source) -> String {
        let (line, col) = src.line_col(self.span.lo);
        format!("{}:{}:{}: error: {}", src.path, line, col, self.msg)
    }
}

/// Edit distance, used for "did you mean" suggestions.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The closest candidate to `name`, if it is close enough to be a likely typo.
/// A candidate that starts the name (`len` for `lenght`) counts as close.
pub fn closest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (name.chars().count() / 3).max(1);
    candidates
        .into_iter()
        .filter_map(|c| {
            let d = edit_distance(name, c);
            if d <= limit {
                Some((d, c))
            } else if c.len() >= 3 && name.starts_with(c) {
                Some((limit + 1, c))
            } else {
                None
            }
        })
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}
