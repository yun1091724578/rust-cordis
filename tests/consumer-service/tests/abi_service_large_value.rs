//! 真 DLL 的 **>1 MiB 业务服务值**：宿主写入、插件跨 ABI 读回。
//!
//! O8 的插件侧 1 MiB 扩容上限误伤了业务载荷：宿主对 `provide`/`set` 的写入没有
//! 字节上限，所以 1.5 MiB 的服务值改前读得回、改后读不回，`ctx.get` 还会把它
//! 静默压成 `None`（插件看到的是"服务缺失"）。
//!
//! 这条用例走**真 ABI**：`greeting`（示例插件 A）把配置里 1.5 MiB 的 `text`
//! provide 成 `greeting.text`，`hello`（示例插件 B）在 apply 里用 `ctx.get`
//! 读它并拼进 `hello.reply.message`。消息与原文逐字相同 ⇒ 读回既没被截断、
//! 也没变成 `None`。`ctx.require` 没有示例插件用到，等价覆盖见
//! `crates/host/tests/sdk_large_service_value.rs`。

mod common;

use common::{Admission, Sandbox, GATE};
use cordis_core::FiberState;

/// 1.5 MiB：越过 O8 误加的 1 MiB 插件侧扩容上限。
const TEXT_BYTES: usize = 1_572_864;
const TIMES: u64 = 3;

/// 首尾各带一枚标记、长度恰好 `TEXT_BYTES` 的探针串。
fn probe_text() -> String {
    let head = "BEGIN-";
    let tail = "-END";
    let mut text = String::with_capacity(TEXT_BYTES);
    text.push_str(head);
    text.push_str(&"a".repeat(TEXT_BYTES - head.len() - tail.len()));
    text.push_str(tail);
    assert_eq!(text.len(), TEXT_BYTES);
    text
}

/// 一条短诊断：1.5 MiB 的原文绝不能整段进断言消息。
fn brief(text: &str) -> String {
    let head: String = text.chars().take(24).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{} 字节，头 {head:?}，尾 {tail:?}", text.len())
}

#[tokio::test]
async fn a_service_value_over_one_mib_survives_the_abi_round_trip() {
    let _gate = GATE.lock().await;
    let text = probe_text();
    let mut sandbox = Sandbox::new("large-value");
    let _admission = Admission::dev();
    // 双引号标量：1.5 MiB 的单行 YAML 不受 plain-scalar 规则的干扰。
    sandbox.write_config(&format!(
        "- id: greeting\n  name: greeting\n  config:\n    text: \"{text}\"\n\
         - id: hello\n  name: hello\n  config:\n    times: {TIMES}\n"
    ));
    sandbox
        .load(true)
        .await
        .unwrap_or_else(|err| panic!("大服务值不得让装载失败: {err}"));

    for label in ["greeting", "hello"] {
        let fiber = sandbox
            .state()
            .fiber_of(label)
            .unwrap_or_else(|| panic!("{label} 必须有挂载 fiber"));
        assert_eq!(
            fiber.state(),
            FiberState::Active,
            "{label} 必须已激活（读大值失败会让它停在别的状态）"
        );
    }

    // ① 宿主侧的那一份：完整、长度正确（写入本身没有上限）。
    let greeting = common::extern_value(sandbox.app(), "greeting.text");
    let stored = greeting["text"]
        .as_str()
        .unwrap_or_else(|| panic!("greeting.text 必须是 {{text: …}}: {greeting}"));
    assert_eq!(
        stored.len(),
        TEXT_BYTES,
        "宿主侧写入被截断：{}",
        brief(stored)
    );

    // ② 插件读回的那一份：`hello` 的 `ctx.get("greeting.text")` 必须拿到完整内容。
    //    读成 `None`（值太大被吞）时这里是 `<缺失>`，长度断言会先红。
    let reply = common::extern_value(sandbox.app(), "hello.reply");
    let message = reply["message"]
        .as_str()
        .unwrap_or_else(|| panic!("hello.reply.message 必须是字符串: {}", reply["message"]));
    assert_eq!(
        message.len(),
        TEXT_BYTES + format!(" × {TIMES}").len(),
        "跨 ABI 读回的消息长度不对（可能被读成\"服务缺失\"）：{}",
        brief(message)
    );
    assert!(
        message == format!("{text} × {TIMES}"),
        "跨 ABI 读回的内容不完整：{}",
        brief(message)
    );

    sandbox.close().await;
    assert!(sandbox.leftovers().is_empty(), "插件目录不得残留暂存副本");
    sandbox.assert_removed();
}
