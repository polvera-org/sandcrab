//! Terminal output: streamed text, dim progress notes, compact tool-call lines.

use std::io::{IsTerminal, Write};

use crate::llm::{EventSink, StreamEvent};
use crate::tools::{ToolOutcome, preview_output, summarize_call, summarize_result};

/// Lines of tool output shown under a call line.
pub const PREVIEW_LINES: usize = 8;

#[derive(Debug, Clone)]
pub struct Ui {
    color: bool,
    quiet: bool,
    at_line_start: bool,
    in_thinking: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self::new(std::io::stdout().is_terminal(), false)
    }
}

impl Ui {
    #[must_use]
    pub fn new(color: bool, quiet: bool) -> Self {
        Self {
            color,
            quiet,
            at_line_start: true,
            in_thinking: false,
        }
    }

    /// Silent UI (tests).
    #[must_use]
    pub fn quiet() -> Self {
        Self::new(false, true)
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }

    #[must_use]
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }

    #[must_use]
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }

    fn write(&mut self, s: &str) {
        if self.quiet || s.is_empty() {
            return;
        }
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
        self.at_line_start = s.ends_with('\n');
    }

    /// Make sure the next output starts on a fresh line.
    pub fn newline(&mut self) {
        if !self.at_line_start {
            self.write("\n");
        }
    }

    pub fn line(&mut self, s: &str) {
        self.newline();
        self.write(&format!("{s}\n"));
    }

    pub fn info(&mut self, s: &str) {
        let d = self.dim(s);
        self.line(&d);
    }

    /// `▸ bash  npm run build  (1.2s, exit 0)` + a few lines of output.
    pub fn tool(&mut self, o: &ToolOutcome) {
        let args = summarize_call(&o.name, &o.input);
        let status = summarize_result(&o.name, &o.output, o.is_error);
        let secs = o.elapsed.as_secs_f64();
        let meta = format!("({secs:.1}s, {status})");
        let meta = if o.is_error {
            self.paint("31", &meta)
        } else {
            self.dim(&meta)
        };
        let head = format!(
            "{} {}  {}  {}",
            self.paint("36", "▸"),
            self.bold(&o.name),
            args,
            meta
        );
        self.line(&head);
        if let Some(p) = preview_output(&o.name, &o.output, o.is_error, PREVIEW_LINES) {
            let indented: String = p.lines().map(|l| format!("  {l}\n")).collect();
            let d = self.dim(indented.trim_end_matches('\n'));
            self.line(&d);
        }
    }
}

impl EventSink for Ui {
    fn on_event(&mut self, ev: StreamEvent<'_>) {
        match ev {
            StreamEvent::BlockStart { kind } => {
                if kind == "thinking" {
                    self.in_thinking = true;
                } else if kind == "text" {
                    self.newline();
                }
            }
            StreamEvent::BlockStop => {
                if self.in_thinking {
                    self.in_thinking = false;
                }
                self.newline();
            }
            StreamEvent::TextDelta(t) => self.write(t),
            StreamEvent::ThinkingDelta(t) => {
                if self.at_line_start && !t.is_empty() {
                    let mark = self.dim("· ");
                    self.write(&mark);
                }
                let s = self.paint("2;3", t);
                self.write(&s);
            }
            StreamEvent::Retry {
                attempt,
                wait,
                reason,
            } => {
                let msg = format!("[retry {attempt} in {:.1}s: {reason}]", wait.as_secs_f64());
                let d = self.paint("33", &msg);
                self.line(&d);
            }
        }
    }
}
