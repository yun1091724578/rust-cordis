//! 非法 schema 在真实 `cordis_plugin_v1` 入口上必须降级为空描述表，
//! 不能把 panic 带过 `extern "C"` 边界终止宿主。

cordis_abi::cordis_plugin! {
    name: "invalid-schema-entry",
    schema: "not-json",
    apply: |_ctx, _config| Ok(()),
}

#[test]
fn invalid_schema_entry_returns_null_instead_of_aborting() {
    assert!(
        cordis_plugin_v1().is_null(),
        "构造失败必须返回空指针，让宿主报可读装载错误"
    );
}
