//! JSON Schema for `veneer.yaml`, generated from the Rust types.

use crate::Spec;

/// Pretty JSON Schema text with a trailing newline (the committed file's exact bytes).
pub fn json_schema_string() -> String {
    let schema = schemars::schema_for!(Spec);
    let mut text = serde_json::to_string_pretty(&schema).expect("schema serializes");
    text.push('\n');
    text
}
