//! ADR 0004：跨 ABI 的结构化错误载荷。
//!
//! 线格式是一个 JSON 对象，`error` 是人类可读消息，其余字段是兄弟：
//!
//! ```json
//! {"error":"boom","code":"E_BOOM","category":"business",
//!  "certainty":"reported","retry":"never","detail":{...}}
//! ```
//!
//! 兼容规则：旧插件/旧宿主只发纯文本，`from_json` 返回 `None`，调用方保留原文；
//! 缺字段按 `unknown` 处理，未知字段忽略；未知码原样保留，绝不解释成成功。

use serde_json::{json, Value};

/// 一条结构化错误。字段值本身是开放字符串：本层只负责传输，不替调用方裁决。
///
/// `#[non_exhaustive]` 保留未来追加字段的空间；外部调用方用 [`ErrorPayload::new`]
/// 和 `with_*` 构造，不直接写结构体字面量。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorPayload {
    /// 人类可读消息。必须保留，即使结构化字段全部缺失。
    pub error: String,
    /// 稳定错误码；未知值原样保留。
    pub code: String,
    /// 错误类别（解析/协议/业务/超时/取消/生命周期/未知）。
    pub category: String,
    /// 执行认知（`not_executed` / `reported` / `unknown`）。
    pub certainty: String,
    /// 重试事实（`never` / `safe` / `unknown`）；不是重试许可。
    pub retry: String,
    /// 可选细节；可以是任意 JSON。
    pub detail: Value,
}

impl ErrorPayload {
    /// 只带消息的错误；其余字段显式为 `unknown`。
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code: "unknown".to_string(),
            category: "unknown".to_string(),
            certainty: "unknown".to_string(),
            retry: "unknown".to_string(),
            detail: Value::Null,
        }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = code.into();
        self
    }

    pub fn with_category(mut self, category: impl Into<String>) -> Self {
        self.category = category.into();
        self
    }

    pub fn with_certainty(mut self, certainty: impl Into<String>) -> Self {
        self.certainty = certainty.into();
        self
    }

    pub fn with_retry(mut self, retry: impl Into<String>) -> Self {
        self.retry = retry.into();
        self
    }

    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = detail;
        self
    }

    /// 编码为线格式；永远包含 `error`。
    pub fn to_json(&self) -> String {
        json!({
            "error": self.error,
            "code": self.code,
            "category": self.category,
            "certainty": self.certainty,
            "retry": self.retry,
            "detail": self.detail,
        })
        .to_string()
    }

    /// 解析线格式。不是 JSON 对象、或缺 `error` 字符串 ⇒ `None`（调用方保留原文）。
    pub fn from_json(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let object = value.as_object()?;
        let error = object.get("error")?.as_str()?.to_string();
        let field = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string()
        };
        Some(Self {
            error,
            code: field("code"),
            category: field("category"),
            certainty: field("certainty"),
            retry: field("retry"),
            detail: object.get("detail").cloned().unwrap_or(Value::Null),
        })
    }
}
