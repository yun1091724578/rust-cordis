# ADR 0005：信任模型与插件来源边界

- **状态**：已接受（2026-11-07；2026-11-18 修订：新增 §2.3 受审流水线签发）
- **日期**：2026-11-07（修订 2026-11-18）
- **依据**：`README.md`《定位与边界》；`docs/support-contract.md` 非目标；
  `docs/adr/0001-second-carrier.md`；
  `docs/adr/0009-rsi-deployment-substrate.md`；
  `docs/release.md` 的 0.1.0/0.1.1 发布记录
- **决定摘要**：本仓只承诺承载**第一方、可审查、随宿主发布**的 T1/T2 插件；
  不承诺对不可信代码的隔离。第三方、运行时下载、以及既未经人工审查也未经
  §2.3 受审流水线签发的模型生成可执行插件，在 T3 落地前不得进入运行时。
  来源治理是准入，不是执行隔离。

---

## 1. 第一性原理

本决定不先从“要加什么功能”出发，而从不可改变的执行事实出发。

### 1.1 不可违背的事实

1. **同进程原生插件与宿主共享地址空间、权限和生命周期。**
   插件可以直接读写宿主内存、打开文件、访问网络、读取环境变量和凭据；
   `realm` / `scope` 只提供可见性隔离，不是安全边界。

2. **同步原生调用在 T2 中不可抢占。**
   宿主可以不再等待一次挂起派发，也可以拒绝后续请求，但不能安全地中断
   正在执行的同步 `apply` / 回调。Rust、tokio 或 `catch_unwind` 都不能把
   一个阻塞的 C ABI 调用变成可取消操作。

3. **Rust 的内存安全不等于资源安全。**
   Rust 能防止 use-after-free 等语言级错误，但不能阻止逻辑泄漏、无界集合、
   `Rc` 环、未 join 的任务、插件自身 allocator 或恶意分配。T2 只能观测和
   拒绝新工作，不能给单个插件设硬内存上限。

4. **ABI 兼容不等于插件身份，也不等于行为兼容。**
   `abi_version` 只回答线格式是否匹配；服务 key 的 `u32` 版本只回答接口
   请求版本是否过高。两者都不能回答“这是哪个插件、哪个 semver、哪个
   源码 revision”。

5. **一个 dylib 的 static 是该模块所有实例共享的。**
   按 handle 分片只能保护宿主已知的插件侧状态（如 volatile）；插件作者
   新增的全局状态仍可能让同一 DLL 的两个实例互相污染。

6. **信任是传递的。**
   如果构建流水线、签名密钥或宿主凭据被攻破，来源治理也会失效。T2 的
   “可信”是组织和工程约束，不是密码学隔离。

### 1.2 由此推出的产品结论

- T2 可以提供**可信组件内的服务组合、生命周期、可观测性和工程安全**；
- T2 不能提供**不可信代码隔离、多租户安全、单次同步调用抢占、per-plugin
  硬内存上限**；
- 如果插件来源包含第三方，或未经 §2.3 签发的模型生成代码，**T3 是前置条件**，
  不是后续优化；
- 任何超出上述边界的“安全”“稳定”“无泄漏”“不冲突”表述，都是契约破坏。

---

## 2. 决定

### 2.1 信任域

| 载体 | 信任域 | 本 ADR 的承诺 |
|---|---|---|
| T1：链进宿主的原生插件 | 与宿主同信任域 | 可作为第一方可信组件 |
| T2：同进程动态库插件 | 与宿主同信任域 | 可作为第一方可信组件 |
| T3：WASM / 子进程 | 独立故障域（不等于自动安全） | 本 ADR 不实现；需另立 T3 ADR |

T1/T2 的 plugin、宿主、配置和宿主注入的 log sink 都在同一信任域。
**“可信组件”不是“安全组件”**：它表示来源、审查和发布纪律可控，不表示
运行时能抵抗恶意代码。

### 2.2 允许的插件来源

允许：

- 与宿主同仓库，或同一组织控制的其他仓库；
- 源码可审查，且有强制 review 记录；
- 产物能对应到固定 commit / revision；
- 由受信任 CI 从该 revision 构建；
- 产物有 SHA256，最好有签名或 SLSA provenance；
- 使用受支持的 ABI；
- 随 `xtask seal` 一起封存（`xtask seal` 只覆盖随宿主发布的插件；变体不走
  seal，走归档仓库 + 准入台账，见 §2.3。ABI 要求对变体同样适用）；
- 来源解析出策略时，loader 只加载 allow-list 中的
  `(plugin id, semver, source revision, artifact SHA256, abi version)`：
  strict 恒生效（未覆写 `*_admitted` 的外部来源除外），fallback 在进程级策略
  存在时生效、无策略且未开开发模式时默认拒绝；`dev_unverified` 下
  allow-list 不生效（ADR 0008 §3/§4）。

不允许：

- 第三方二进制；
- 运行时下载的可执行插件；
- 模型生成、且既未经人工审查也未经 §2.3 受审流水线签发的代码；
- 无法对应到固定源码 revision 的产物；
- 仅凭 `schema` / `provision` 自述就视为已授权。

跨仓库不是禁止项，但“同一流水线”本身不是信任证明。最低要求是：
**组织控制 + review + 固定 revision + 受控构建 + 产物 hash/来源证明 +
allow-list**。如果产品要求更严，可以升级为“必须同仓库”。

### 2.3 受审流水线签发（演化产物）

自我改进/演化循环产出的变体是模型生成代码，逐个人工审查不成立。本 ADR 允许
一条替代路径：由**受审流水线**签发，而不是由人对每个变体签字。签发不降低
§2.2 的最低要求，只是把审查对象从“产物”上移到“生产者”。

受审流水线必须**同时**满足：

- **组织控制**：流水线代码与宿主在同一组织控制的仓库，受同一 review 纪律；
- **review 的对象是流水线**：评估器（固定用例与评分）、选择门禁（何者可以
  被签发）、签发逻辑三者都经人工 PR 修改；单个变体不审。只有已审的
  `pipeline_revision` 可以签发，集合由人工 PR 维护（**本仓不校验该集合**，
  由流水线入口与 PR 纪律强制）；签发入口不得有“评估失败仍签发”的 override；
- **固定 revision**：变体源进归档仓库并记 variant revision；准入台账记录
  variant revision 与 pipeline revision。sidecar 的 `PluginManifest` 有
  `source_revision`（`verify` 会比对），但**没有** variant revision 与
  pipeline revision 这两个字段，且未开 `deny_unknown_fields`——把后两者写进
  sidecar 会被静默丢弃，阶段 1 不依赖 sidecar 承载它们。变体的
  `source_revision` 由归档构建写入 manifest，准入台账必须把它与
  `variant_revision` 显式绑定：台账持一行 `source_revision → variant_revision`
  的映射（相等时也写明）；无法映射的变体在准入合并时不得进入 allow-list，
  不允许“事后解释”；
- **受控构建**：产物由受信任 CI 从该 revision 构建；归档仓库**禁止提交二进制
  产物**，否则“从该 revision 构建”退化为“取回已提交的二进制”；
- **产物 hash 与来源记录**：artifact SHA256，加 `attestation` 三元组
  （`pipeline_id` / `pipeline_revision` / `eval_record_sha256`）。评估记录
  本身必须含 variant revision 与 artifact SHA256，准入时分别核对：
  **variant revision 对准入台账记录**，**artifact SHA256 对 manifest 的
  `sha256`**。锚点不能混用——manifest 里没有 variant revision，只有
  `source_revision`。否则同一份好记录可以配任意二进制（重放）；
- **ABI 同受精确门禁**：`abi_version` 不匹配的变体在装载前的校验中被拒绝
  （`validate_manifest` / `verify`，不是部署准入台账）；
  `xtask seal` 只覆盖随宿主发布的插件，变体走归档仓库 + allow-list；
- **allow-list 在来源解析出策略时生效**：条目按
  `(plugin id, semver, source revision, artifact SHA256, abi version)` 匹配。
  strict 恒生效（未覆写 `*_admitted` 的外部来源除外，ADR 0008 §4）；
  fallback 在进程级策略存在时生效（此时它仍是 allow-list 门禁），无策略且
  未开开发模式时**默认拒绝**；`dev_unverified` 下 allow-list 不生效
  （ADR 0008 §3）。
  所以“唯一门禁”只在 strict（且来源确实覆写了 `*_admitted`）下无条件成立；
- **准入台账旁挂 attestation 摘要**：摘要写入 allow-list 旁的准入记录，不写进
  `PluginSourcePolicy` 的 JSON。现行 `AllowedPlugin` 没有该字段，且
  `from_json` 未开 `deny_unknown_fields`——写进 policy 会被**静默丢弃**，
  不构成任何校验；阶段 1 不依赖该字段；
- **职责分离**：准入合并（把条目写进 allow-list）必须由签发流水线**之外**的
  主体执行。同一自动化既签发又合并时，下面的交叉核对退化为自洽检查。

**attestation 的核对发生在部署准入，不在运行时。** `loader` /
`PluginSourcePolicy::verify` 只比对 id / semver / revision / hash / ABI，
不认识 `attestation`；把“缺 attestation 即由 loader 拒绝”写成运行时行为是
虚报。（`verify` 本身仍会在装载前执行 allow-list 比对，那是另一回事。）
**准入台账是部署侧文件**：loader 不读它，它也不随产物发行。缺任一项的材料
**不得进入 allow-list**，这条由部署方的准入台账步骤（人工 PR 或受审 CI）
执行：核对 `eval_record` 的 `variant_revision` 与台账记录相等、
`artifact_sha256` 与 manifest 的 `sha256` 相等、`eval_record_sha256` 与
attestation 相等、`attestation.pipeline_revision` 与台账的 pipeline revision
相等，并核对台账映射行的 `source_revision` 与 manifest 的 `source_revision`
一致。

**写入者与核对者不是同一主体。** 台账与 `eval_record` 由签发流水线写入，
核对与合并由部署准入方执行；这正是上一条“职责分离”的含义。

**交叉相等只能防配错与重放，不能防伪造。** `eval_record` 与准入台账由同一条
流水线写入，两边一致也可能两边都错或都被改；真正的独立证据是签名与构建
provenance。没有签名时，attestation 的可信上限等于 allow-list 合并者的审查。
需要抗伪造时，给 allow-list 与 attestation 加签名或 SLSA provenance；本 ADR
不承诺该能力。

**`dev_unverified` 禁令只能由部署纪律保证。** loader 没有“变体”概念，只读来源
对象自带的准入结论；`PolicyPluginSource::dev_unverified` 是公开构造器，宿主
无法区分它是给普通插件还是给变体用的。把它写成运行时门禁同样是虚报。

仍然**不允许**：

- 流水线自身被演化：`pipeline_revision` 的变更只能经人工 PR；
- 跳过评估门禁的签发（例如评估失败但强制签发）；
- 把 `attestation` 自述当成授权：它是准入材料，不是运行时信任；
- 用 §2.3 包装第三方二进制：外部下载物不是“变体”，不得进入归档仓库。

后果不变：签发方被攻破等价于 §1.1 第 6 条“信任是传递的”失效；宿主仍与插件同进程、
同权限。签发只回答“这是哪个流水线、用哪份评估记录选出来的”，不回答
“这段代码是安全的”。

### 2.4 承诺口径

| 表述 | 是否允许 | 说明 |
|---|---|---|
| 可信组件内的工程安全 | 允许 | panic 截断、生命周期可逆、已枚举路径资源回基线 |
| 安全隔离 / 沙箱 | **禁止** | T2 同进程、同地址空间、同权限 |
| 不会内存泄漏 | **禁止** | 只能证明已枚举路径回到基线；插件仍可逻辑泄漏 |
| 版本不冲突 | **有条件** | 需要插件 semver、来源 revision、接口 range 与 mount 前求解；当前只有 ABI/接口门禁 |
| 多租户安全底座 | **禁止** | 无 per-plugin 内存上限、抢占、权限收窄 |
| 单次同步调用可超时取消 | **禁止** | `DispatchPolicy` 只覆盖等待结算，不覆盖正在执行的同步调用 |
| 微服务底座（远程/独立部署） | **禁止** | 当前是进程内服务组合，不是远程协议或独立故障域 |

对外文档必须使用左列口径。出现右列禁用语时，应视为文档契约破坏。

### 2.5 允许的失败模式

“接受”不等于“已解决”，也不等于“可以写成保证”。它表示产品明确知道
T2 无法在本进程内消除该风险，并把处置责任放到了部署层或后续 T3。

| 事件 | T2 后果 | 处置 | 可否称为保证 |
|---|---|---|---|
| 插件死循环 / 阻塞 IO | 占住调用线程；宿主不能安全抢占 | 部署级 supervisor 重启；拒绝新工作；不承诺单次调用取消 | 否 |
| 插件内存暴涨 | 进程 OOM；cgroup/Job Object 只能限制整个宿主 | 进程级上限、RSS 观测、容量准入 | 否 |
| 插件 panic | SDK 生成的 Rust 插件可在 ABI 边界截断；手写/非 Rust 插件跨 `extern "C"` panic 可能 abort | 故障注入；不承诺状态回滚 | 否 |
| 逻辑泄漏 | 内存随轮次增长 | RSS/dhat、真实 `open_staged` soak、泄漏预算 | 否（只能是已枚举路径基线） |
| 版本/依赖不匹配 | 装载前拒绝或诊断 | semver + ABI 精确门禁 + 接口 range 求解 | 有条件 |
| 恶意插件 | 完全控制进程 | 来源治理；一旦出现必须触发 T3 复评 | 否 |

特别声明：

- `DispatchPolicy` 是**等待结算的看门狗**，不是执行超时；
- `ctx_dispose_self` / `ctx_dispose` 当前通过 spawn 执行 fiber dispose，
  不是同步重入等待；但“持有生命周期读锁跨越插件调用”仍需单独锁纪律审查；
- 插件 panic 的捕获边界取决于插件是否使用 SDK；不要把 SDK 的保护扩大到手写插件；
- 子进程 T3 也不是自动安全：还需要低权限、文件/网络策略和 IPC 收窄。

---

## 3. 工程不变量

以下不是建议，而是任何“底座”声明成立前必须满足的条件。

### I1. 插件来源可追溯

- manifest 至少记录：plugin id、semver、source revision、ABI version、
  artifact SHA256；
- 演化产物另记 `attestation`：pipeline id、pipeline revision、eval record
  SHA256；评估记录须含 variant revision 与 artifact SHA256（§2.3）；
  其他来源可选。`attestation` 落在准入台账侧；若要写进 `PluginManifest`，
  属 0.2 级变更（ADR 0006：可字面量构造的公开 DTO 加字段）；注意
  `api-inventory` 对宿主 DTO 是每类型一行，字段级差异不进门禁，需人工评审
  （ABI 结构的字段由扫描覆盖，那是另一条路径）；
- 演化产物的 attestation 由部署准入方核对（写入者与核对者分离，§2.3），
  `verify` 只比对 id / semver / revision / hash / ABI；
- loader 有 allow-list（strict 恒生效，未覆写 `*_admitted` 的外部来源除外；
  fallback 在进程级策略存在时生效、无策略且未开开发模式时默认拒绝；
  `dev_unverified` 下 allow-list 不生效，ADR 0008 §3/§4）；
- `inspect` / `status` 能显示来源和 revision；
- 没有来源信息的插件默认拒绝（或显式开发模式）。

### I2. 调用边界不被误解

- 同步 `apply` / 回调没有抢占；
- `DispatchPolicy` 只覆盖 `STATUS_PENDING` 之后的等待；
- 文档、CLI 和报告都不得暗示“设置 timeout 就能停止插件”；
- 需要进程级 supervisor 兜底。

### I3. 锁纪律

- 不在持有宿主 Mutex/RwLock 时调用插件；
- `call_inbound` 的生命周期读锁跨调用路径必须审查并测试；
- 不在持锁区间执行可能 panic 的用户/插件代码；
- 锁中毒只在找到可达触发路径后升级为缺陷，不统一吞掉 poison。

### I4. 容量与准入

- instance、dispatch、队列、stream 之外的宿主集合都需要上限；
- 超限给出可读诊断并拒绝新工作；
- 上限按 App / 实例配置，而不是只做进程级全局值；
- 慢插件不能通过无界集合拖垮整个宿主。

### I5. 策略按宿主实例而不是全局

`DispatchPolicy` 不能只有一个进程级 global static；多 App / 多宿主实例需要
独立配置。默认值不能一刀切（例如模型长流可能合法超过 30 秒），应按
插件/方法/调用类型配置，并显式 opt-out。

### I6. 内存可观测，但不冒充硬上限

- RSS / dhat / allocator 统计是进程级观测；
- cgroup / Job Object 限制整个宿主进程，不是单个 T2 插件；
- per-plugin 硬上限、OOM 只杀该插件，需要 T3；
- 不承诺“不会泄漏”，只承诺“已枚举路径回归基线 + 可观测 + 可告警”。

### I7. 插件身份先于版本求解

- ABI 保持精确匹配，不在同一版同时引入互相冲突的 `abi_min/abi_max` 规则；
- 先在尾追加字段记录插件 semver + source revision；
- `provides/requires` range 求解是后续独立能力，不能和 ABI 兼容混为一谈；
- 同一插件多 major 并存需要明确注册表身份和冲突策略，不能默认自动安全。

### I8. 插件侧全局状态由插件作者负责

- 宿主已知的 volatile 按 handle 分片；
- callback registry 按 owner 记录；
- 但插件作者新增的 static 不受宿主保护；
- 文档和模板必须要求按实例句柄分片，测试同一 DLL 多实例。

---

## 4. 分阶段整改

### P0：可信单租户底座可用前

1. **来源与身份**
   - plugin id、semver、source revision、artifact hash 进入 manifest；
   - loader allow-list；
   - `inspect` / `status` 展示来源；
   - 没有来源信息的插件默认拒绝（开发模式显式放宽）。

2. **按 App 的截止时间接线**
   - `DispatchPolicy` 接入每个 App/宿主实例；
   - 支持按插件/方法配置；
   - 明确只覆盖挂起结算等待，不覆盖同步 `call`；
   - 长调用显式 opt-out 并记录。

3. **容量与准入**
   - instances / dispatches / 队列设置上限；
   - 超限拒绝并给出可读错误；
   - 上限按 App/实例配置。

4. **锁纪律与故障注入**
   - 审查并测试 `call_inbound` 生命周期读锁跨调用；
   - 覆盖 hang / panic / 重入 / 卸载竞态；
   - 不在持锁时调用插件；
   - 明确 panic 捕获边界（SDK vs 手写插件）。

5. **对外失败模式**
   - README / 支持契约 / release 说明写入“同步调用不可抢占”；
   - 说明 `DispatchPolicy` 的真实边界；
   - 说明部署级 supervisor 是 T2 的唯一兜底。

> **P0 不包含“有界专用线程池 + 超时放弃等待”。**
> 线程池只能把阻塞移出 tokio worker，不能取消线程；超时放弃会留下仍在
> 执行插件代码的线程，卸载 DLL 时可能 use-after-unload。它最多是 P1 的
> 故障隔离手段，且线程退出前不得卸载。

### P1：长期运行与版本治理

6. **插件 semver 与依赖求解**
   - `provides/requires` range；
   - mount 前解析、歧义/缺失拒绝；
   - `(id, version)` 身份；
   - inspect/status 展示解析结果。

7. **版本矩阵 CI**
   - 插件 vN 在宿主 vN 与 vN+1 上运行；
   - 冻结夹具扩展为两个相邻版本；
   - 不把同版本加载冒充跨版本兼容。

8. **内存与性能**
   - nightly RSS 断言、dhat 采样；
   - soak 的暂存路径改为真实 `open_staged`；
   - 调用缓冲池化，去掉每次 16 KiB 临时分配；
   - 高频路径 benchmark 后再谈“高效”。

9. **锁中毒与故障隔离**
   - 找到可达持锁 panic 路径后再决定 poison 策略；
   - 若保留专用线程池，必须证明超时后不卸载、不释放插件状态；
   - 明确线程泄漏是已知代价，不是“稳定”。

### P2 / T3：不可信代码或硬隔离

10. **T3 决策**
    - WASM（wasmtime + component model）或子进程；
    - 内存线性上限、fuel/epoch、kill、低权限沙箱；
    - HostApi 收窄、IPC 协议、失败映射、迁移路径；
    - 在 T3 验收前不发布“安全隔离/多租户”表述。

---

## 5. 非目标

本 ADR 不改变现有六条非目标，并额外明确：

- 不实现 T3；
- 不提供 per-plugin 内存上限与 CPU 抢占；
- 不提供文件/网络能力收窄；
- 不把 `schema` / `provision` 自述升级为授权；
- 不把 `attestation` 自述升级为运行时信任（§2.3）；
- 不承诺多租户、远程部署或独立故障域；
- 不承诺“不会死锁/不会泄漏/不会版本冲突”的绝对命题；
- 不把线程池超时描述成“取消同步调用”。

---

## 6. 复评触发条件

出现任一情况，必须重开本 ADR，并另立 T3 决策：

- 任一第三方插件，或未按 §2.3 签发的模型生成插件要进入运行时；
- 出现多租户、不可信输入或对外托管需求；
- 需要 per-plugin 内存上限、CPU 配额或可抢占执行；
- 生产环境出现插件 hang 导致宿主不可用；
- 需要独立部署、独立故障域或远程调用协议；
- 构建流水线、签名密钥或发布凭据被攻破。

T3 决策至少包含：载体选型、HostApi 收窄、权限模型、性能预算、
失败映射、迁移路径、验收与回滚。

---

## 7. 验收与证据

- README / 支持契约已有“插件来源”行，状态为**声明**，并注明这是来源治理
  不是运行时安全（当时的保证索引 26 行 / 17 保证；WP1/WP2/WP2b 后为 29 行 / 20 保证，
  `xtask/src/contract.rs` 固定；§2.3 只改该行措辞，不增行）；
- P0 每项有对应测试或故障注入记录，否则不得标记完成；
- `cargo xtask docs` 继续只对“保证”行要求测试证据；
- 文档不得出现“安全隔离 / 沙箱 / 不会泄漏 / 版本不冲突 / 多租户安全底座”
  等禁用语；
- nightly 产出 RSS / soak 报告并保留 artifact；
- 本 ADR 不新增任何“安全隔离”表述。

---

## 8. 后果

**正面**

- 产品边界与文档一致，不再为不存在的隔离能力辩护；
- 现有冻结 ABI、资源基线、1M soak 证据继续有效；
- 来源治理和 allow-list 可以先于 T3 落地；
- 保留 `Plugin` trait / loader 作为未来 T3 的替换 seam。

**负面**

- 不形成第三方插件生态；
- 不支持多租户和不可信代码；
- 单插件 hang/OOM 仍可能影响整个宿主，只能靠部署级监督兜底；
- “可信”依赖构建与审查纪律，流水线被攻破则全部失效；
- 不做 T3 就永远不能把“安全”作为产品卖点。

---

## 9. 待确认问题

1. “第一方”是否允许跨仓库？本 ADR 建议允许，但要求组织控制、review、
   固定 revision、受控构建、hash/provenance 和 allow-list。
2. P0 是否接受“不包含线程池抢占”？本 ADR 建议接受；线程池最多是 P1
   的 tokio 保护，不是取消机制。
3. 是否允许同一插件多 major 并存？本 ADR 建议先记录 `(id, version)`，
   在 P1 明确注册表身份和冲突策略前不承诺。
4. T3 选 WASM 还是子进程？本 ADR 不选；另立 T3 ADR。
