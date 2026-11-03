//! veneer-spec: the veneer spec model, value tree, and diagnostics.

mod diag;

pub use diag::{Diagnostic, Severity, Span, codes, has_errors, sort_diagnostics};
