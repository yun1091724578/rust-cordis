# abi-err-v1 夹具

这是一个**独立的旧 ABI 错误夹具**，不是 `tests/fixtures/abi-v1` 的修改版：

- 布局停在 72B v1 前缀：7 个字段（`abi_version` / `size` / `id` / `inject` /
  `schema` / `attach` / `call`），没有 `provision` / `error_format` / `reserved`；
  源码内有 72B 编译期断言；
- `OP_APPLY` 与 `OP_CALLBACK` 都返回**纯文本** `STATUS_ERR`；
- 它不跟随 HEAD，也不被 `abi-v1` 的冻结哈希门禁覆盖；`abi-v1` 的哈希与字节保持不变；
- 用途是验证新宿主收到旧插件错误时保留可读文本，并把缺失的结构字段按 `unknown`
  处理，而不是误判成“无错误”或“未执行”。

`crates/host/tests/legacy_error.rs` 在临时 target 目录中构建并加载它，并断言
`PluginDesc.size == 72` 且 `provision` 不存在，防止它静默退化成 88B/96B 形状。

## Provenance

| 文件 | SHA256 |
|---|---|
| `src/lib.rs` | `5dc6b27fca154e178896eba017f6550b308cccd8a3315952897d8f9deb3e1f10` |
| `Cargo.toml` | `be4eea93b83abcd282ee90d2690c63b11d92ee5849eb00862edfbf64110ad50e` |

哈希按 **LF 字节**计算；根目录 `.gitattributes` 把 `tests/fixtures/` 固定为 `eol=lf`，
避免 Windows CRLF 检出改变夹具字节。来源：`tests/fixtures/abi-v1` 的 v1 布局快照，
按错误场景裁剪为本夹具；修改源码或清单时必须有意更新本表，不能跟随 workspace HEAD
自动漂移。
