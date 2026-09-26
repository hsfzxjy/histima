use crate::source::{SourceFile, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub span: Span,
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub labels: Vec<Label>,
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>, span: Span) -> Self {
        Self {
            severity: Severity::Error,
            message: message.into(),
            labels: vec![Label {
                span,
                message: None,
            }],
            notes: Vec::new(),
        }
    }

    pub fn with_label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            span,
            message: Some(message.into()),
        });
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn render(&self, source: &SourceFile) -> String {
        let severity = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let mut output = format!("{severity}: {}\n", self.message);
        for label in &self.labels {
            let (line, column) = source.location(label.span.start);
            output.push_str(&format!(" --> {}:{line}:{column}\n", source.name()));
            if let Some(line_text) = source.line_text(line) {
                output.push_str(&format!("  |\n{line:>2} | {line_text}\n  | "));
                output.push_str(&" ".repeat(column.saturating_sub(1)));
                let width = if source.location(label.span.end).0 == line {
                    (label.span.end - label.span.start).max(1) as usize
                } else {
                    1
                };
                output.push_str(&"^".repeat(width));
                if let Some(message) = &label.message {
                    output.push(' ');
                    output.push_str(message);
                }
                output.push('\n');
            }
        }
        for note in &self.notes {
            output.push_str(&format!(" note: {note}\n"));
        }
        output
    }
}
