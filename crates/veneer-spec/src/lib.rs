//! veneer-spec: the veneer spec model, value tree, and diagnostics.

mod de;
mod diag;
mod model;
mod schema;
mod value;

pub use de::{DeError, from_node};
pub use diag::{Diagnostic, Severity, Span, codes, has_errors, sort_diagnostics};
pub use model::*;
pub use schema::json_schema_string;
pub use value::{Kind, Node, escape_pointer_token, split_pointer};
