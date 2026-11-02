//! `cordis_plugin!` 的 `schema:` 声明必须落到描述表。

use cordis_abi::PluginDesc;

cordis_abi::cordis_plugin! {
    name: "macro-schema",
    schema: r#"{"type":"object","properties":{"name":{"type":"string"}}}"#,
    apply: |_ctx, _config| Ok(()),
}

#[test]
fn macro_schema_arm_writes_the_declaration() {
    let desc = cordis_plugin_v1();
    let blob = unsafe { (*desc).schema };
    let bytes = unsafe { std::slice::from_raw_parts(blob.ptr, blob.len) };
    let text = std::str::from_utf8(bytes).unwrap();
    assert_eq!(
        text,
        r#"{"type":"object","properties":{"name":{"type":"string"}}}"#
    );
    assert!(unsafe {
        PluginDesc::field_at::<cordis_abi::Blob>(desc, std::mem::offset_of!(PluginDesc, schema))
    }
    .is_some());
}
