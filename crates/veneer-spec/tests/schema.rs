#[test]
fn committed_schema_is_up_to_date() {
    let generated = veneer_spec::json_schema_string();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../schema/veneer.schema.json");
    if std::env::var_os("VENEER_BLESS").is_some() {
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, &generated).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        committed == generated,
        "schema/veneer.schema.json is stale: run `VENEER_BLESS=1 cargo test -p veneer-spec --test schema`"
    );
}

#[test]
fn schema_forbids_unknown_top_level_keys() {
    let schema: serde_json::Value = serde_json::from_str(&veneer_spec::json_schema_string()).unwrap();
    assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    assert!(schema["properties"]["services"].is_object());
}
