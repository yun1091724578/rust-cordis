# abi-error 夹具

这是一个**跟随当前 ABI 布局**的最小 cdylib 插件，用于验证
`LoadedPlugin::apply` 把 `PluginDesc.error_format` 带进实例后，插件的
`STATUS_ERR` 结构化载荷会被宿主解码成可读错误（而不是把原始 JSON 泄露给用户）。

## 不是冻结快照

它与 `tests/fixtures/abi-v1` 的定位不同：`abi-v1` 是冻结的 v1 形状，
不跟随 HEAD；本夹具主动跟随当前 `PluginDesc` / `HostApi` 布局，ABI 尾追加时必须
同步更新。

## 构建与使用

- 独立 crate，`[workspace]` 为空，零依赖；
- `crates/host/tests/apply_error_format.rs` 在**临时 target 目录**里用
  `cargo build --manifest-path ... --offline --locked` 构建，避免复用旧 DLL；
- 夹具在 apply 时返回一个结构化 `STATUS_ERR`，测试断言宿主错误里只有可读消息、
  没有 `"code"` 信封。

## 文件

- `src/lib.rs`：最小入口、`PluginDesc` 形状、结构化错误返回；
- `Cargo.toml` / `Cargo.lock`：独立构建所需的最小清单。
