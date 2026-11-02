//! typed 服务的 **JSON 面**（M6）：`#[cordis::service]` 生成的适配器用的那几件事。
//!
//! 一条线上契约（D2）：**方法调用的负载是 JSON 数组 `[a1, a2, …]`，位置 = 参数位置**。
//! 为什么是数组：`ExternService::call` 只有一个 JSON 值的位置，而方法元数是固定的——
//! 数组是唯一能同时表达"位置"与"元数"的形态。信封的形状**在这里统一校验**
//! （G8 的 SDK 侧那一半：宿主侧对任意 JSON 服务值的形状校验属于领域契约，不在这层）。

use cordis_core::{Config, Error};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// 一次方法调用的参数信封游标。
///
/// 方法体按声明顺序 `take` 参数；参数名只进诊断消息（"第几个参数解不出来"）。
pub struct Envelope {
    service: &'static str,
    method: String,
    args: std::vec::IntoIter<Config>,
}

impl Envelope {
    /// 取下一个参数（按方法声明顺序调用）。
    pub fn take<A: DeserializeOwned>(&mut self, name: &str) -> Result<A, Error> {
        let Some(raw) = self.args.next() else {
            return Err(message(
                self.service,
                &self.method,
                format!("缺参数 `{name}`（信封比方法声明短）"),
            ));
        };
        serde_json::from_value(raw).map_err(|err| {
            message(
                self.service,
                &self.method,
                format!("参数 `{name}` 解不出来: {err}"),
            )
        })
    }
}

/// 校验信封形状并开一个游标：必须是**长度等于方法元数**的 JSON 数组。
pub fn open(
    service: &'static str,
    method: &str,
    args: Config,
    arity: usize,
) -> Result<Envelope, Error> {
    match args {
        Config::Array(values) if values.len() == arity => Ok(Envelope {
            service,
            method: method.to_string(),
            args: values.into_iter(),
        }),
        Config::Array(values) => Err(message(
            service,
            method,
            format!(
                "参数信封长度不符：期望 {arity} 个参数，收到 {} 个",
                values.len()
            ),
        )),
        other => Err(message(
            service,
            method,
            format!("参数信封必须是 JSON 数组（长度 {arity}），收到 {other}"),
        )),
    }
}

/// 方法返回值 → JSON。
pub fn reply<T: Serialize>(service: &str, method: &str, value: T) -> Result<Config, Error> {
    serde_json::to_value(value)
        .map_err(|err| message(service, method, format!("返回值无法序列化: {err}")))
}

/// 方法自己的错误（`E: Display`）→ 内核的错误类型。**错误照原话带出去**，不改写。
pub fn returned_error<E: std::fmt::Display>(service: &str, method: &str, err: E) -> Error {
    format!("service \"{service}\" 的方法 \"{method}\" 返回错误: {err}").into()
}

/// 方法名不存在：**列出可用方法**（可执行的诊断，与 `Ctx::call_iface` 同一口径）。
pub fn unknown_method(service: &str, method: &str, known: &[&str]) -> Error {
    format!("service \"{service}\" 没有方法 \"{method}\"；可用方法: {known:?}").into()
}

fn message(service: &str, method: &str, detail: String) -> Error {
    format!("service \"{service}\" 的方法 \"{method}\"：{detail}").into()
}
