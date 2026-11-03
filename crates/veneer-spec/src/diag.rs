//! Source-located diagnostics with stable codes.

use serde::Serialize;

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

/// A source location. `start`/`end` are 0-based byte offsets; `line`/`col` are 1-based.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Span {
    pub uri: String,
    pub start: usize,
    pub end: usize,
    pub line: u32,
    pub col: u32,
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    pub span: Option<Span>,
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Error,
            message: message.into(),
            span: None,
            notes: Vec::new(),
        }
    }

    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Warning,
            message: message.into(),
            span: None,
            notes: Vec::new(),
        }
    }

    #[must_use]
    pub fn at(mut self, span: Option<Span>) -> Self {
        self.span = span;
        self
    }

    #[must_use]
    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    fn sort_key(&self) -> (Option<(&str, usize, usize)>, &'static str, &str) {
        (
            self.span.as_ref().map(|s| (s.uri.as_str(), s.start, s.end)),
            self.code,
            self.message.as_str(),
        )
    }
}

/// Deterministic order: spanless first, then `(uri, start, end)`, then code, then message.
pub fn sort_diagnostics(diags: &mut [Diagnostic]) {
    diags.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
}

pub fn has_errors(diags: &[Diagnostic]) -> bool {
    diags.iter().any(|d| d.severity == Severity::Error)
}

/// Stable diagnostic codes. `V1xxx` resolve, `V2xxx` template/phase, `V3xxx` adapters,
/// `V4xxx` steps, `W…` warnings.
pub mod codes {
    pub const SYNTAX: &str = "V1000";
    pub const NOT_FOUND: &str = "V1001";
    pub const POINTER_NOT_FOUND: &str = "V1002";
    pub const CYCLE: &str = "V1003";
    pub const REF_SIBLINGS: &str = "V1004";
    pub const REMOTE_UNSUPPORTED: &str = "V1007";
    pub const DUPLICATE_KEY: &str = "V1008";
    pub const SCHEMA: &str = "V1009";
    pub const VERSION: &str = "V1010";
    pub const OVERLAY: &str = "V1011";
    pub const SPEC_NOT_FOUND: &str = "V1012";
    pub const SECRET_SOURCE: &str = "V1013";
    pub const SERVICE_KIND: &str = "V1014";
    pub const PHASE: &str = "V2001";
    pub const TEMPLATE_SYNTAX: &str = "V2002";
    pub const TEMPLATE_UNKNOWN_VAR: &str = "V2003";
    pub const OVERLAY_HINT: &str = "W1101";
    pub const OVERLAY_UNMATCHED: &str = "W1102";
    pub const MODULES_UNSUPPORTED: &str = "W1201";
}
