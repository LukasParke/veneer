//! veneer-spec: the veneer spec model, value tree, and diagnostics.

mod de;
mod diag;
mod value;

pub use de::{DeError, from_node};
pub use diag::{Diagnostic, Severity, Span, codes, has_errors, sort_diagnostics};
pub use value::{Kind, Node, escape_pointer_token, split_pointer};
