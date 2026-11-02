# 冻结 ABI v1 夹具

这是一个**真正的 cdylib**，不是手写的结构体模拟。

- `src/lib.rs` 是 `crates/abi/src/lib.rs` 的快照（去掉了 `sdk` 模块），
  再加上一个最小插件入口；
- 入口返回**真正的 72 字节 v1 结构体**（只到 `call`，没有 `provision` 字段），
  `size = size_of::<PluginDescV1>()`（x64 上等于 `PLUGIN_DESC_PREFIX_SIZE`）；
- `abi_version = 1`；
- `OP_VOLATILE_PATHS` 返回 `{"ret":["data"]}`；`OP_DISPOSE` 写
  `CORDIS_FROZEN_MARKER` 指向的标记文件；事件回调只接受
  `fixture/event` / id 1；
- `features = ["bad-version"]` / `["bad-size"]` 生成两种坏夹具，供宿主拒绝路径测试；
- 零依赖，`cargo build --manifest-path ... --target-dir <tmp> --offline --locked`
  可构建；测试每次在临时 target 里重建，不提交二进制；
- 由 `crates/host/tests/frozen_plugin.rs` 加载：
  hash 校验 → probe → 原始尺寸断言 → 坏夹具拒绝 → load → mount →
  `bail("fixture/event")` → dispose → 先卸库后删文件。

## 构建与格式

- 测试每次用临时 `--target-dir` 重建；不要提交二进制或 `target/`。
- 这是独立 crate：`cargo fmt --all` 不覆盖它，格式检查用
  `cargo fmt --manifest-path tests/fixtures/abi-v1/Cargo.toml -- --check`。
- 测试依赖仓库根的夹具；发布包 `cargo package --no-verify` 不运行它。

## 冻结纪律

**这个副本永远不跟随 HEAD 更新。** 需要改它时，视为一次有意的“新夹具版本”，
同时更新下面的哈希与测试；`frozen_plugin.rs` 会重算 `src/lib.rs` 与
`Cargo.toml` 的 SHA256，和本表不一致就失败。否则“旧插件”会悄悄变成“当前插件”。

## Provenance

| 文件 | SHA256 |
|---|---|
| 来源 `crates/abi/src/lib.rs` | `EB7EA39C9499AABD585471E94105F10EC5E75A768D949F81A07DF595780BE1CB` |
| 夹具 `src/lib.rs` | `7740D09228A1ED3F00FA77CBA98486634241CBAEDF704D771047CD90361E2B8E` |
| 夹具 `Cargo.toml` | `1C91F3296AADF96335401FEF192643C07E8872A96217EBE5A472D92D78842CEC` |

哈希按 **LF 字节**计算；根目录 `.gitattributes` 把 `tests/fixtures/` 固定为 `eol=lf`，
避免 Windows CRLF 检出改变夹具字节。来源哈希是快照时的出处；A3 之后 HEAD 已多次变化，夹具仍是 `EB7EA39C…`
时的冻结副本，不跟随当前 ABI。
夹具 `Cargo.lock` 零依赖，`--locked` 用它保证构建不漂移。

## 局限

它是“冻结快照”，不是历史发布版。A3 的 `error_format` 已使快照成为真正的旧版；
历史发布版仍未取得，因此“历史发布版 × 新宿主”仍是 `docs/support-contract.md`
里的待验证项。
