//! 阶段 3：SDK 显式提供配置 schema，并校验它是合法 JSON 对象。

use cordis_abi::sdk::build_plugin_with_schema;
use cordis_abi::PluginDesc;

#[test]
#[should_panic(expected = "合法 JSON")]
fn invalid_schema_is_rejected_by_the_builder() {
    let _ = build_plugin_with_schema(
        "invalid-schema-json",
        &[],
        &[],
        &[],
        Some("not-json"),
        |_, _| Ok(()),
    );
}

#[test]
#[should_panic(expected = "JSON 对象")]
fn non_object_schema_is_rejected_by_the_builder() {
    let _ = build_plugin_with_schema("invalid-schema-array", &[], &[], &[], Some("[]"), |_, _| {
        Ok(())
    });
}

#[test]
#[should_panic(expected = "合法 JSON")]
fn empty_schema_is_rejected_by_the_builder() {
    let _ = build_plugin_with_schema("empty-schema", &[], &[], &[], Some(""), |_, _| Ok(()));
}

#[test]
#[should_panic(expected = "上限")]
fn oversized_schema_is_rejected_by_the_builder() {
    let oversized = format!(
        r#"{{"x":"{}"}}"#,
        "x".repeat(cordis_abi::MAX_CONFIG_SCHEMA_BYTES)
    );
    let _ = build_plugin_with_schema(
        "oversized-schema",
        &[],
        &[],
        &[],
        Some(&oversized),
        |_, _| Ok(()),
    );
}

#[test]
fn undeclared_schema_is_the_canonical_empty_blob() {
    let desc = build_plugin_with_schema("no-schema", &[], &[], &[], None, |_, _| Ok(()));
    let blob = unsafe { (*desc).schema };
    assert_eq!(blob.len, 0);
    assert!(blob.ptr.is_null(), "未声明必须是 Blob::EMPTY");
}

#[test]
fn empty_object_schema_is_accepted_verbatim() {
    let schema = "{}";
    let desc = build_plugin_with_schema(
        "empty-object-schema",
        &[],
        &[],
        &[],
        Some(schema),
        |_, _| Ok(()),
    );
    let blob = unsafe { (*desc).schema };
    let bytes = unsafe { std::slice::from_raw_parts(blob.ptr, blob.len) };
    assert_eq!(std::str::from_utf8(bytes).unwrap(), schema);
}

#[test]
fn schema_is_written_to_the_frozen_plugin_desc_prefix() {
    let schema =
        r#"{"type":"object","required":["token"],"properties":{"token":{"type":"string"}}}"#;
    let desc = build_plugin_with_schema("schema-test", &[], &[], &[], Some(schema), |_, _| Ok(()));
    let blob = unsafe { (*desc).schema };
    assert!(blob.len > 0, "schema 必须写进 PluginDesc.schema");
    let bytes = unsafe { std::slice::from_raw_parts(blob.ptr, blob.len) };
    assert_eq!(std::str::from_utf8(bytes).unwrap(), schema);
    assert_eq!(
        unsafe {
            PluginDesc::field_at::<cordis_abi::Blob>(desc, std::mem::offset_of!(PluginDesc, schema))
        }
        .map(|blob| blob.len),
        Some(schema.len()),
        "schema 在冻结前缀内，按 size 可直接读"
    );
}
