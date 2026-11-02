//! ADR 0004 的能力位与结构化错误载荷：先钉住尺寸判界与编解码边界。

use cordis_abi::{Blob, ErrorPayload, HostApi, Out, PluginDesc, ABI_VERSION, ERROR_FORMAT_V1};
use serde_json::json;
use std::mem::offset_of;

unsafe extern "C" fn dummy_attach(_: *const HostApi) {}

unsafe extern "C" fn dummy_call(_: u64, _: u32, _: Blob, _: Blob, _: *mut Out) -> i32 {
    cordis_abi::STATUS_OK
}

fn desc() -> PluginDesc {
    PluginDesc {
        abi_version: ABI_VERSION,
        size: std::mem::size_of::<PluginDesc>() as u32,
        id: Blob::EMPTY,
        inject: Blob::EMPTY,
        schema: Blob::EMPTY,
        attach: dummy_attach,
        call: dummy_call,
        provision: Blob::EMPTY,
        error_format: ERROR_FORMAT_V1,
        tail_reserved: 0,
    }
}

#[test]
fn plugin_desc_error_format_uses_the_size_gate() {
    let mut desc = desc();
    let offset = offset_of!(PluginDesc, error_format);
    desc.size = (offset + 4) as u32;
    assert_eq!(
        unsafe { PluginDesc::error_format_at(&desc) },
        ERROR_FORMAT_V1
    );
    desc.size = (offset + 3) as u32;
    assert_eq!(
        unsafe { PluginDesc::error_format_at(&desc) },
        0,
        "差一字节必须读作未声明"
    );
}

#[test]
fn host_error_format_uses_the_size_gate() {
    // 全 None / 0 的默认表；能力位是 4 字节，不是 Option<u32> 的 8 字节。
    let offset = offset_of!(HostApi, error_format);
    let mut host = HostApi {
        abi_version: ABI_VERSION,
        error_format: ERROR_FORMAT_V1,
        ..HostApi::default()
    };
    host.size = (offset + std::mem::size_of::<u32>() - 1) as u32;
    assert_eq!(host.error_format(), 0, "差一字节必须读作未声明");
    host.size = (offset + std::mem::size_of::<u32>()) as u32;
    assert_eq!(host.error_format(), ERROR_FORMAT_V1);
}

#[test]
fn host_field_at_checks_size_before_reading() {
    let offset = offset_of!(HostApi, ctx_get);
    let mut host = HostApi {
        size: offset as u32,
        ..HostApi::default()
    };
    assert!(
        unsafe { host.field_at::<Option<cordis_abi::CtxGetFn>>(offset) }.is_none(),
        "size 未覆盖字段时不得读取该字段"
    );
    host.size = (offset + std::mem::size_of::<Option<cordis_abi::CtxGetFn>>()) as u32;
    assert!(
        unsafe { host.field_at::<Option<cordis_abi::CtxGetFn>>(offset) }.is_some(),
        "size 覆盖字段时应返回声明值"
    );
}

#[test]
fn payload_round_trips_all_fields() {
    let payload = ErrorPayload::new("boom")
        .with_code("E_BOOM")
        .with_category("business")
        .with_certainty("reported")
        .with_retry("never")
        .with_detail(json!({ "field": "x" }));
    let decoded = ErrorPayload::from_json(&payload.to_json()).expect("结构化载荷必须可解码");
    assert_eq!(decoded, payload);
}

#[test]
fn plain_text_is_not_structured() {
    assert!(ErrorPayload::from_json("plain error").is_none());
}

#[test]
fn missing_or_non_string_error_is_invalid() {
    assert!(ErrorPayload::from_json(r#"{"code":"E"}"#).is_none());
    assert!(ErrorPayload::from_json(r#"{"error":42}"#).is_none());
}

#[test]
fn invalid_json_is_not_structured() {
    assert!(ErrorPayload::from_json("{not json").is_none());
}

#[test]
fn empty_message_is_preserved() {
    let decoded = ErrorPayload::from_json(&ErrorPayload::new("").to_json()).expect("空消息仍合法");
    assert_eq!(decoded.error, "");
}

#[test]
fn unknown_code_is_preserved() {
    let decoded = ErrorPayload::from_json(
        r#"{"error":"boom","code":"E_FUTURE","category":"future","certainty":"unknown","retry":"unknown"}"#,
    )
    .expect("未知码不得被解释成成功");
    assert_eq!(decoded.code, "E_FUTURE");
    assert_eq!(decoded.error, "boom");
}

#[test]
fn unknown_fields_are_ignored() {
    let decoded = ErrorPayload::from_json(
        r#"{"error":"boom","code":"E","future_field":{"x":1},"detail":{"a":1}}"#,
    )
    .expect("未知字段必须前向兼容");
    assert_eq!(decoded.detail, json!({ "a": 1 }));
}

#[test]
fn defaults_are_explicitly_unknown() {
    let decoded = ErrorPayload::from_json(r#"{"error":"boom"}"#).expect("只有 error 也能解");
    assert_eq!(decoded.code, "unknown");
    assert_eq!(decoded.category, "unknown");
    assert_eq!(decoded.certainty, "unknown");
    assert_eq!(decoded.retry, "unknown");
    assert_eq!(decoded.detail, serde_json::Value::Null);
}
