use veneer_spec::{Diagnostic, Severity, Span, codes, has_errors, sort_diagnostics};

fn span(uri: &str, start: usize) -> Option<Span> {
    Some(Span {
        uri: uri.into(),
        start,
        end: start + 1,
        line: 1,
        col: start as u32 + 1,
    })
}

#[test]
fn builders_set_fields() {
    let d = Diagnostic::warning(codes::OVERLAY_UNMATCHED, "nothing matched")
        .at(span("file:///a.yaml", 3))
        .note("check the target");
    assert_eq!(d.code, "W1102");
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(d.message, "nothing matched");
    assert_eq!(d.span.as_ref().unwrap().start, 3);
    assert_eq!(d.notes, vec!["check the target".to_string()]);
}

#[test]
fn sorting_is_by_uri_then_range_then_code_with_spanless_first() {
    let mut ds = vec![
        Diagnostic::error(codes::SCHEMA, "b").at(span("file:///b.yaml", 0)),
        Diagnostic::error(codes::CYCLE, "a2").at(span("file:///a.yaml", 9)),
        Diagnostic::error(codes::SPEC_NOT_FOUND, "none"),
        Diagnostic::error(codes::SYNTAX, "a1").at(span("file:///a.yaml", 2)),
    ];
    sort_diagnostics(&mut ds);
    let order: Vec<&str> = ds.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(order, vec!["none", "a1", "a2", "b"]);
}

#[test]
fn has_errors_ignores_warnings() {
    assert!(!has_errors(&[Diagnostic::warning(
        codes::OVERLAY_HINT,
        "w"
    )]));
    assert!(has_errors(&[Diagnostic::error(codes::SCHEMA, "e")]));
}
