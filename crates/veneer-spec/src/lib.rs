//! veneer-spec: the veneer spec model, value tree, and diagnostics.

mod diag;
mod value;

pub use diag::{Diagnostic, Severity, Span, codes, has_errors, sort_diagnostics};
pub use value::{Kind, Node, escape_pointer_token, split_pointer};
