//! `grep` walk + search.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use sandcrab_protocol::{GrepMatch, GrepParams, GrepResult, ProtocolError};

use super::{
    GREP_MAX_CONTEXT_LINES, GREP_MAX_LINE_CHARS, GREP_MAX_MATCHES_PER_FILE, rel_path, stat_root,
    walker,
};

/// Validated, compiled `grep` call.
pub(super) struct Options {
    matcher: RegexMatcher,
    filter: Option<FileFilter>,
    context: usize,
    max: usize,
}

struct FileFilter {
    glob: GlobMatcher,
    /// Pattern has no `/`: match the file name instead of the relative path.
    basename: bool,
}

impl Options {
    pub(super) fn new(p: &GrepParams, max: usize) -> Result<Self, ProtocolError> {
        let mut b = RegexMatcherBuilder::new();
        b.case_insensitive(p.case_insensitive)
            .line_terminator(Some(b'\n'));
        let matcher = if p.literal {
            b.build(&escape(&p.pattern))
        } else {
            b.build(&p.pattern)
        }
        .map_err(|e| {
            let hint = if p.literal {
                ""
            } else {
                "; set `literal: true` to search for the plain string"
            };
            ProtocolError::InvalidParams(format!("invalid pattern `{}`: {e}{hint}", p.pattern))
        })?;
        let filter = p
            .glob
            .as_deref()
            .map(|g| {
                let g = g.strip_prefix("./").unwrap_or(g);
                GlobBuilder::new(g)
                    .literal_separator(true)
                    .backslash_escape(true)
                    .build()
                    .map(|glob| FileFilter {
                        glob: glob.compile_matcher(),
                        basename: !g.contains('/'),
                    })
                    .map_err(|e| {
                        ProtocolError::InvalidParams(format!("invalid `glob` filter `{g}`: {e}"))
                    })
            })
            .transpose()?;
        let context = p.context_lines.unwrap_or(0).min(GREP_MAX_CONTEXT_LINES) as usize;
        Ok(Self {
            matcher,
            filter,
            context,
            max,
        })
    }
}

pub(super) fn run(root: &Path, o: &Options) -> Result<GrepResult, ProtocolError> {
    let files = if stat_root(root)?.is_file() {
        let name = root
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        vec![(root.to_path_buf(), name)]
    } else {
        collect_files(root, o.filter.as_ref())
    };

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .before_context(o.context)
        .after_context(o.context)
        .build();

    let mut matches = Vec::new();
    let mut truncated = false;
    for (path, rel) in files {
        let remaining = o.max.saturating_sub(matches.len());
        // With `remaining == 0` the search stops at the first match; it only tells us
        // whether the result is actually truncated.
        let mut sink = Collector::new(rel, o.context, remaining.min(GREP_MAX_MATCHES_PER_FILE));
        if searcher.search_path(&o.matcher, &path, &mut sink).is_err() || sink.binary {
            continue;
        }
        if sink.hit_cap {
            truncated = true;
        }
        matches.extend(sink.matches);
        if truncated && matches.len() >= o.max {
            break;
        }
    }
    Ok(GrepResult { matches, truncated })
}

/// Regular files under `root` passing the filter, sorted by relative path. `grep` has no
/// `respect_gitignore` param: it always honours ignore files (and skips `.git`).
fn collect_files(root: &Path, filter: Option<&FileFilter>) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    for item in walker(root, true).build() {
        let Ok(e) = item else { continue };
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = rel_path(root, e.path());
        if let Some(f) = filter {
            let hit = if f.basename {
                f.glob.is_match(e.file_name())
            } else {
                f.glob.is_match(&rel)
            };
            if !hit {
                continue;
            }
        }
        files.push((e.into_path(), rel));
    }
    files.sort_by(|a, b| a.1.cmp(&b.1));
    files
}

/// Per-file [`Sink`]: turns searcher lines into [`GrepMatch`]es with per-match context.
struct Collector {
    rel: String,
    context: usize,
    cap: usize,
    /// Last `context` emitted lines (match or context), for `context_before`.
    window: VecDeque<(u64, String)>,
    matches: Vec<GrepMatch>,
    /// Index of the first match still collecting `context_after`.
    open: usize,
    hit_cap: bool,
    binary: bool,
}

impl Collector {
    fn new(rel: String, context: usize, cap: usize) -> Self {
        Self {
            rel,
            context,
            cap,
            window: VecDeque::new(),
            matches: Vec::new(),
            open: 0,
            hit_cap: false,
            binary: false,
        }
    }

    /// Feed one emitted line; returns whether the search should continue.
    fn line(&mut self, ln: u64, bytes: &[u8], is_match: bool) -> bool {
        let n = self.context as u64;
        let text = line_text(bytes);
        for m in &mut self.matches[self.open..] {
            if ln > m.line_number && ln <= m.line_number + n {
                m.context_after.push(text.clone());
            }
        }
        if is_match {
            if self.matches.len() >= self.cap {
                self.hit_cap = true;
            } else {
                let context_before = self
                    .window
                    .iter()
                    .filter(|(l, _)| *l < ln && *l + n >= ln)
                    .map(|(_, t)| t.clone())
                    .collect();
                self.matches.push(GrepMatch {
                    path: self.rel.clone(),
                    line_number: ln,
                    line: text.clone(),
                    context_before,
                    context_after: Vec::new(),
                });
            }
        }
        while self
            .matches
            .get(self.open)
            .is_some_and(|m| m.line_number + n <= ln)
        {
            self.open += 1;
        }
        if self.context > 0 {
            self.window.push_back((ln, text));
            if self.window.len() > self.context {
                self.window.pop_front();
            }
        }
        !(self.hit_cap && self.open >= self.matches.len())
    }
}

impl Sink for Collector {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, io::Error> {
        let first = m.line_number().unwrap_or(0);
        let mut go = true;
        for (i, line) in m.lines().enumerate() {
            go = self.line(first + i as u64, line, true);
            if !go {
                break;
            }
        }
        Ok(go)
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, io::Error> {
        Ok(self.line(c.line_number().unwrap_or(0), c.bytes(), false))
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
    }
}

/// Escape regex metacharacters (same set as `regex_syntax::escape`) for `literal` search.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if r"\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Line bytes → display text: terminator stripped, lossy UTF-8, cut to the line cap.
fn line_text(bytes: &[u8]) -> String {
    let b = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let b = b.strip_suffix(b"\r").unwrap_or(b);
    let s = String::from_utf8_lossy(b);
    match s.char_indices().nth(GREP_MAX_LINE_CHARS) {
        None => s.into_owned(),
        Some((i, _)) => {
            let rest = s[i..].chars().count();
            format!("{}… [{rest} more chars]", &s[..i])
        }
    }
}
