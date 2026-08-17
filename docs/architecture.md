# Rust Agent Runtime：基于 Cordis 思想的可动态扩展架构设计

> 文档状态：架构草案  
> 目标读者：Runtime、平台、插件与 Agent 工程开发者  
> 核心目标：以 Rust 构建稳定内核，同时允许 AgentLoop、Tool、Memory、Skill 等能力在运行时发现、装载、授权、替换和卸载。

## 1. 摘要

本文提出一套受 Cordis 的 Context、Scope、事件与服务注册思想启发的 Rust Agent Runtime 架构。它不追求在运行时生成新的 Rust trait 实现，而是把动态边界定义为稳定协议，并通过 Native、Process、WASM 三类后端承载具体能力。

架构的核心分工是：

- **Runtime 负责执行与治理**：生命周期、注册表、调度、状态、权限、审批、审计和故障恢复。
- **AgentLoop 负责决策**：根据显式输入状态与上下文，返回下一步动作，不直接绕过 Runtime 操作资源。
- **Context 负责依赖解析**：以父子层级提供服务、配置和局部覆盖。
- **Scope 负责生命周期**：统一管理注册、订阅、后台任务和资源释放。
- **插件协议负责动态性**：通过可序列化输入输出隔离语言、进程和编译边界。

系统应优先保证四件事：行为可审计、状态可恢复、权限不可绕过、插件可卸载。性能优化与热替换能力建立在这些约束之上。

## 2. 目标与非目标

### 2.1 目标

1. Runtime 启动后可发现并加载新的 Tool、Skill、Memory Provider 和 AgentLoop，无需重新编译主程序。
2. Session 能覆盖全局服务，并为一次 Run 或 Turn 提供更短生命周期的临时上下文。
3. 所有高风险操作必须经过统一的权限判定、可选人工审批和审计记录。
4. AgentLoop 可在安全点替换，并通过版本化状态迁移继续运行。
5. Session 可在进程崩溃或重启后由事件日志与 checkpoint 恢复。
6. Native、Process、WASM 后端共享同一能力语义与治理路径。

### 2.2 非目标

- 不承诺任意 Rust 动态库 ABI 的长期稳定性。
- 不允许第三方 Loop 直接持有 Runtime 内部对象或绕过 Tool Pipeline。
- 不把 Context 设计成无类型的全局键值垃圾场。
- MVP 不包含分布式调度、插件市场、跨节点热迁移和复杂 DAG 编排。

## 3. 关键设计原则

### 3.1 动态来自协议，而不是 Rust ABI

Rust trait object 只能在运行时选择已经编译进二进制的实现。真正的第三方动态扩展应使用 Process 或 WASM，通过稳定、版本化、可序列化的协议交互。Native 后端用于内置能力和性能敏感路径，不作为开放插件 ABI。

### 3.2 决策与执行分离

AgentLoop 只能产生决策，例如调用工具、请求模型、等待审批或完成任务。文件、网络、密钥、Session 和 Memory 的真实访问始终由 Runtime 执行。

```text
External / WASM / Native Loop
          │  提交 Decision
          ▼
      Rust Runtime
          │
          ├─ Policy Engine
          ├─ Approval Manager
          ├─ Tool Pipeline
          ├─ Memory Gateway
          └─ Audit Log
```

### 3.3 状态外置、实现尽量无状态

Loop 状态由 Session/Runtime 持有并持久化；Loop 实现接收旧状态，返回决策与新状态。这样才能重放、恢复、迁移和热替换。

### 3.4 所有动态注册都绑定 Scope

工具、事件监听器、中间件、后台任务和资源句柄必须注册到明确 Scope。Scope 关闭时执行逆序释放，保证插件卸载和 Session 关闭不会留下孤儿资源。

### 3.5 权限检查位于不可绕过的执行边界

manifest 中的权限仅是声明，不是授权。真正的判定发生在每次敏感操作之前，并绑定主体、Session、资源、参数、时间和调用链。

## 4. 总体架构

```text
┌──────────────────────────────── Agent Runtime ────────────────────────────────┐
│                                                                              │
│  Runtime Context                                                             │
│  ├─ Service Registry      ├─ Plugin Manager       ├─ Event Bus              │
│  ├─ Loop Registry         ├─ Policy Engine        ├─ Approval Manager       │
│  ├─ Tool Registry         ├─ Checkpoint Store     └─ Audit / Telemetry      │
│  └─ Memory Registry                                                          │
│          │                                                                   │
│          ├────────────── Session Context A ──────────────────────────────┐    │
│          │  Session Log · Projection · Memory View · Session Scope       │    │
│          │       │                                                       │    │
│          │       └─ Run/Turn Context                                     │    │
│          │          Cancellation · Deadline · Trace · Temporary Grants   │    │
│          │                                                               │    │
│          └────────────── Session Context B ──────────────────────────────┘    │
│                                                                              │
│  Capability Backends                                                         │
│  ├─ Native: in-process Rust                                                   │
│  ├─ Process: JSON-RPC over stdio / local socket                              │
│  └─ WASM: component boundary + capability-based host calls                   │
└──────────────────────────────────────────────────────────────────────────────┘
```

一次典型步骤的控制流：

```text
User Input
   │
   ▼
append SessionEvent::UserMessage
   │
   ▼
ContextAssembler ── retrieve Memory ── apply Skill prompt/resources
   │
   ▼
LoopExecutor.step(input)
   │
   ├─ Continue / RequestModel
   ├─ CallTools ── Policy ── Approval ── ToolExecutor
   ├─ Wait
   └─ Complete
   │
   ▼
append events ── update projection ── checkpoint at safe point
```

## 5. Runtime、Context 与 Scope

### 5.1 Runtime

Runtime 是进程级组合根，负责创建顶层 Context、加载配置、初始化注册表、发现插件、恢复 Session，并协调优雅关闭。

```rust
pub struct Runtime {
    context: Context,
    scope: Scope,
    sessions: SessionManager,
    plugins: PluginManager,
}

impl Runtime {
    pub async fn start(config: RuntimeConfig) -> Result<Self>;
    pub async fn open_session(&self, id: SessionId) -> Result<SessionHandle>;
    pub async fn shutdown(self) -> Result<()>;
}
```

Runtime 不应亲自实现具体 Tool 或 Loop，而应组合注册表、Provider 和策略组件。

### 5.2 Context：分层服务解析

Context 是带父级引用的类型化服务容器。解析顺序遵循最近作用域优先：

```text
Run / Turn Context
        │ miss
        ▼
Session Context
        │ miss
        ▼
Runtime Context
```

建议采用强类型键或 Rust 类型本身作为服务标识，对真正需要扩展的数据使用受约束的 typed extension，而不是让业务依赖任意 JSON 键。

```rust
pub trait Service: Send + Sync + 'static {}

pub struct Context {
    parent: Option<Arc<Context>>,
    services: ServiceMap,
}

impl Context {
    pub fn child(&self) -> Context;
    pub fn provide<T: Service>(&mut self, value: Arc<T>);
    pub fn resolve<T: Service>(&self) -> Result<Arc<T>>;
}
```

Context 的局部覆盖适用于模型 Provider、策略、工具视图、Memory 视图和配置，不应覆盖不可变的身份、审计与安全根。

### 5.3 Scope：资源所有权与释放

Scope 形成树状生命周期：

```text
RuntimeScope
├─ PluginScope(github-research)
├─ SessionScope(A)
│  ├─ AgentScope(researcher)
│  └─ RunScope(turn-42)
└─ SessionScope(B)
```

Scope 至少管理：取消令牌、事件订阅、注册表句柄、后台任务、临时目录、外部进程、WASM 实例和清理回调。

```rust
pub struct Scope { /* private */ }

impl Scope {
    pub fn child(&self, name: impl Into<String>) -> Scope;
    pub fn cancellation_token(&self) -> CancellationToken;
    pub fn defer<F>(&self, cleanup: F) where F: FnOnce() + Send + 'static;
    pub fn spawn<F>(&self, future: F) -> TaskHandle;
    pub async fn dispose(self) -> Result<()>;
}
```

释放采用幂等、逆序和限时原则。超时资源进入强制终止与告警路径，但不能阻塞整个 Runtime 无限退出。

## 6. AgentLoop 的动态替换

### 6.1 协议边界

```rust
#[async_trait]
pub trait LoopExecutor: Send + Sync {
    async fn step(&self, input: LoopStepInput) -> Result<LoopStepOutput>;
}

#[derive(Serialize, Deserialize)]
pub struct LoopStepInput {
    pub protocol_version: String,
    pub session_id: SessionId,
    pub step_id: StepId,
    pub state: VersionedState,
    pub messages: Vec<Message>,
    pub available_tools: Vec<ToolDefinition>,
    pub context_budget: ContextBudget,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoopStepOutput {
    Continue { state: VersionedState },
    RequestModel { request: ModelRequest, state: VersionedState },
    CallTools { calls: Vec<ToolCall>, state: VersionedState },
    Wait { reason: WaitReason, state: VersionedState },
    Complete { output: AgentOutput, state: VersionedState },
}
```

Runtime 必须验证输出：协议版本、状态大小、工具是否存在、参数 schema、单步调用数量、预算和状态迁移合法性。Loop 返回的内容一律视为不可信输入。

### 6.2 Registry 与选择

```rust
pub struct LoopDescriptor {
    pub id: LoopId,
    pub version: Version,
    pub backend: BackendKind,
    pub state_schema: SchemaRef,
    pub capabilities: CapabilitySet,
}

pub trait LoopProvider: Send + Sync {
    fn descriptor(&self) -> &LoopDescriptor;
    async fn instantiate(&self, scope: Scope) -> Result<Arc<dyn LoopExecutor>>;
}
```

Session 保存的是 `LoopRef { id, version_constraint }`，而不是 Rust 类型。开始运行或到达安全点时，由 LoopRegistry 解析具体 Provider。

### 6.3 热替换流程

```text
step N 完成
   │
   ▼
阻止新 step，等待在途工具结束或取消
   │
   ▼
写入 checkpoint + LoopSwapRequested
   │
   ▼
验证新 Loop 的协议、权限与 state schema
   │
   ├─ schema 相同：直接加载
   └─ schema 不同：执行显式 migrate(old_state)
   │
   ▼
切换 LoopRef，释放旧 AgentScope，记录 LoopSwapped
   │
   ▼
step N+1
```

只有 safe point 允许切换。若迁移或初始化失败，保持旧 Loop 和旧状态；不得提交半完成切换。

## 7. Tool 动态加载与执行管线

Tool 是可发现的受治理操作。定义与执行分离：定义供模型和 Loop 选择，执行器仅由 Runtime 调用。

```rust
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: JsonSchema,
    pub output_schema: Option<JsonSchema>,
    pub required_permissions: Vec<PermissionRequirement>,
    pub effects: EffectSet,
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn invoke(&self, ctx: ToolCallContext, args: JsonValue)
        -> Result<ToolResult>;
}
```

执行管线固定为：

```text
resolve tool
  → validate input schema
  → normalize resource target
  → evaluate policy
  → request approval when required
  → reserve budget / idempotency key
  → execute with timeout and cancellation
  → validate / bound output
  → redact secrets
  → append audit and Session events
```

动态注册返回 `RegistrationHandle`，其所有权归插件 Scope。卸载插件时先将工具标记为 draining，拒绝新调用，等待或取消在途调用，再注销定义与执行器。

## 8. Event：emit、waterfall、serial、parallel

事件系统既服务于通知，也服务于受控扩展点。四种语义必须分开，避免同一 API 同时承担广播和数据变换。

| 模式 | 输入输出 | 顺序 | 失败语义 | 典型用途 |
|---|---|---|---|---|
| `emit` | `E -> ()` | 不保证业务顺序 | 聚合报告，不改主结果 | 遥测、UI 通知、审计镜像 |
| `waterfall` | `T -> T` | 按优先级串行 | 中断或显式降级 | Prompt、Context、Tool 参数变换 |
| `serial` | `E -> Result<()>` | 按优先级串行 | fail-fast 或策略化继续 | 生命周期钩子、迁移、提交前检查 |
| `parallel` | `E -> Vec<R>` | 并发 | 聚合结果与错误 | 独立检索、并行评估、健康检查 |

接口示例：

```rust
pub trait EventBus {
    async fn emit<E: Event>(&self, event: E) -> EmitReport;
    async fn waterfall<T: TransformEvent>(&self, event: T) -> Result<T>;
    async fn serial<E: Event>(&self, event: E) -> Result<SerialReport>;
    async fn parallel<E: Event, R>(&self, event: E) -> ParallelReport<R>;
}
```

关键约束：

1. 监听器绑定 Scope，Scope 释放即自动退订。
2. 优先级必须稳定；相同优先级使用注册序号保证确定性。
3. `waterfall` 中每个变换都要保留 trace，敏感字段修改需再做 schema 与权限校验。
4. `parallel` 必须有并发上限、超时和确定性的结果排序，不以完成时间决定业务顺序。
5. 用于恢复的事实写入 Session Event Log；普通 Event Bus 通知不能替代持久化日志。

## 9. 权限、审批与审计

### 9.1 权限模型

建议采用 capability + resource + constraints：

```yaml
permission:
  action: filesystem.write
  resource: /workspace/project/**
  constraints:
    max_bytes: 1048576
    session_only: true
```

一次授权决策至少包含：主体（用户、Session、Skill、Loop）、动作、规范化资源、参数摘要、数据分类、当前 Scope、授权来源和过期时间。

### 9.2 审批状态

```rust
pub enum PolicyDecision {
    Allow(Grant),
    Deny(DenyReason),
    RequireApproval(ApprovalRequest),
}

pub enum ApprovalStatus {
    Pending,
    Approved { grant: Grant },
    Denied,
    Expired,
    Cancelled,
}
```

审批不是简单布尔值。`Grant` 应绑定调用指纹或受限范围，并支持一次性、当前 Run、当前 Session 和限时授权。审批期间 Loop 返回 `Wait`，Session 进入可恢复状态；用户批准后由 Runtime 重新验证当前调用与授权是否仍匹配。

### 9.3 不可绕过路径

```text
Loop / Skill / Plugin
        │ intent
        ▼
Runtime Gateway
        ├─ identity
        ├─ schema
        ├─ policy
        ├─ approval
        ├─ execution
        └─ audit
```

Process 插件通过 OS 隔离与最小环境变量限制能力；WASM 插件不启用默认 WASI 权限，只暴露经过授权的 host functions；Native 插件视为受信任内置代码，但其业务操作仍应走相同 Gateway，以保持一致的审计语义。

## 10. Session Context

Session Context 不等于 Runtime Context，也不等于 AgentLoop 状态。推荐三层生命周期：

| 层级 | 生命周期 | 主要内容 |
|---|---|---|
| Runtime Context | 进程 | 注册表、插件、策略、审批、持久化后端 |
| Session Context | 会话 | Event Log、Projection、Memory View、LoopRef、Session Scope |
| Run/Turn Context | 一次运行或轮次 | deadline、cancellation、trace、预算、临时授权 |

```rust
pub struct SessionContext {
    pub id: SessionId,
    pub log: Arc<dyn SessionLog>,
    pub projection: Arc<RwLock<SessionProjection>>,
    pub memory: MemoryView,
    pub loop_ref: Arc<RwLock<LoopRef>>,
    pub context: Context,
    pub scope: Scope,
}
```

对话与执行事实写入 append-only 日志；频繁读取的轻量状态保存为 Projection：

```rust
pub enum SessionEvent {
    UserMessage(UserMessage),
    AssistantMessage(AssistantMessage),
    TurnStarted(TurnMeta),
    ToolCallRequested(ToolCall),
    ApprovalRequested(ApprovalRequest),
    ApprovalResolved(ApprovalResolution),
    ToolCallCompleted(ToolResultMeta),
    LoopSwapped(LoopSwapMeta),
    MemoryWriteCommitted(MemoryWriteMeta),
    TurnCompleted(TurnResult),
    CheckpointCreated(CheckpointMeta),
}
```

事件写入必须携带单调序号、时间、trace、actor 和幂等键。大体积工具输出应存入 Blob Store，事件中只保存摘要、引用和内容哈希。

## 11. Memory

Memory 是独立数据能力，不应由 Loop 自行拼接后直接注入模型。推荐链路：

```text
MemoryProvider.retrieve
        │
        ▼
MemoryItem + provenance + score + policy labels
        │
        ▼
ContextAssembler
        │  去重、排序、裁剪、脱敏、预算控制
        ▼
LLM Working Context
```

```rust
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    async fn retrieve(&self, query: MemoryQuery) -> Result<Vec<MemoryItem>>;
    async fn write(&self, item: MemoryWrite) -> Result<MemoryId>;
    async fn forget(&self, selector: MemorySelector) -> Result<ForgetReport>;
}
```

MemoryRegistry 可提供 session、SQLite、向量数据库、远程服务等 Provider。Session 或 Agent 通过配置选择 Provider，但 Runtime 返回的应是带命名空间和权限约束的 `MemoryView`。

命名空间示例：

```text
session/<session-id>
user/<user-id>/profile
skill/github-research
agent/researcher
```

Skill 可以声明所需的 Memory namespace 或写入策略，但不能默认读取全部长期记忆。每个 `MemoryItem` 应包含来源、创建者、时间、置信度、敏感级别、TTL 和引用关系，便于过滤、解释与删除。

## 12. Skills

Skill 是可发现、可加载、可授权、可卸载的能力包，不是单一服务。它可以组合 Tool、Prompt、Resource、Middleware、Memory policy，以及可选 AgentLoop。

```text
github-research/
├─ skill.yaml
├─ prompts/
├─ resources/
├─ schemas/
├─ bin/            # Process backend，可选
└─ component.wasm  # WASM backend，可选
```

manifest 示例：

```yaml
apiVersion: agent.runtime/v1alpha1
kind: Skill
metadata:
  name: github-research
  version: 1.2.0
spec:
  backend:
    kind: process
    command: ["python", "main.py"]
  provides:
    tools: [github_search, github_read_repo]
    prompts: [research_system]
    loops: [deep_research]
  requires:
    runtime: ">=0.1,<0.2"
    permissions:
      - network.connect: api.github.com:443
      - secret.read: GITHUB_TOKEN
      - memory.read: skill/github-research
```

加载事务：

```text
discover
 → parse + validate manifest
 → verify identity / integrity / compatibility
 → resolve dependencies
 → calculate requested permissions
 → create SkillScope
 → start backend and handshake
 → stage registrations
 → atomically publish capability snapshot
```

任一步失败都回滚已注册内容并释放 SkillScope。卸载时执行反向流程：drain、取消任务、注销能力、停止后端、释放资源。运行中的 Session 应持有能力快照或版本引用，避免注册表变化导致单步执行期间能力集合漂移。

## 13. 插件后端：Native、Process、WASM

| 后端 | 优点 | 主要风险 | 推荐用途 |
|---|---|---|---|
| Native | 性能最好、类型集成强、调试直接 | 与主程序同故障域，开放 ABI 不稳定 | 内置核心能力、可信高频路径 |
| Process | 语言无关、进程隔离、开发简单 | 启动与序列化成本，OS 沙箱差异 | Python/Node 插件、早期生态、重型依赖 |
| WASM | 可移植、能力式沙箱、资源可计量 | 组件工具链与调试更复杂 | 第三方分发、不可信插件、可控热加载 |

### 13.1 统一握手

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "runtime/initialize",
  "params": {
    "protocolVersion": "1.0",
    "runtimeVersion": "0.1.0",
    "grantedCapabilities": ["tool.register", "memory.read:skill/github-research"],
    "limits": {"maxMessageBytes": 1048576, "maxConcurrentCalls": 4}
  }
}
```

插件返回实际支持的协议范围、能力和 schema 摘要。Runtime 取交集后建立连接；不兼容则拒绝激活。

### 13.2 后端共同要求

- 调用必须携带 request、session、trace 和 deadline。
- 支持取消、超时、心跳与受控关闭。
- 消息大小、并发、CPU、内存和输出体积均有上限。
- 插件崩溃不得破坏 Session 事实日志；Runtime 将未完成调用标记为明确的失败或未知结果。
- 对具有外部副作用的调用使用幂等键或补偿策略，避免恢复时重复执行。

## 14. 状态机与 Checkpoint

### 14.1 Run 状态机

```text
             ┌──────────────┐
             │   CREATED    │
             └──────┬───────┘
                    ▼
             ┌──────────────┐
       ┌────▶│   RUNNING    │◀────────────┐
       │     └──┬─────┬─────┘             │
       │        │     │                   │
 resume│        │     └──────┐            │ approval/result
       │        ▼            ▼            │
 ┌─────┴────┐ ┌──────────┐ ┌───────────┐ │
 │ PAUSED   │ │ WAITING  │ │CHECKPOINT │─┘
 └──────────┘ │ APPROVAL │ └───────────┘
              └────┬─────┘
                   │
         ┌─────────┴─────────┐
         ▼                   ▼
   ┌───────────┐       ┌───────────┐
   │ COMPLETED │       │  FAILED   │
   └───────────┘       └───────────┘
```

另外允许从非终态进入 `CANCELLED`。状态转换由 Runtime 单点提交，并与 Session Event 写入保持原子一致或可恢复的一致性。

### 14.2 Checkpoint 内容

```rust
pub struct Checkpoint {
    pub session_id: SessionId,
    pub sequence: u64,
    pub run_state: RunState,
    pub loop_ref: LoopRef,
    pub loop_state: VersionedState,
    pub projection_version: u64,
    pub pending_actions: Vec<PendingAction>,
    pub capability_snapshot: CapabilitySnapshotRef,
    pub created_at: DateTime<Utc>,
    pub checksum: String,
}
```

Checkpoint 应在以下安全点创建：用户输入已提交后、模型响应落盘后、工具结果提交后、进入审批等待前、Loop 切换前后，以及达到步数或时间阈值时。

恢复流程为：加载最后一个校验通过的 checkpoint，重放其后的 SessionEvent，重建 Projection，核对 pending action。对已经发出但结果未知的副作用调用，不得自动重复；应根据幂等键查询结果或进入人工确认状态。

## 15. 一致性、并发与故障边界

1. **单 Session 串行提交**：一个 Session 同时只有一个状态提交者；独立的读、检索和工具执行可并发，但提交按 sequence 排序。
2. **能力快照**：每个 step 固定使用一个 registry snapshot，插件安装或卸载只影响后续 step。
3. **事件先于投影**：事实日志成功后再更新 Projection；Projection 可丢弃并由日志重建。
4. **副作用栅栏**：在执行外部副作用前记录 intent，完成后记录 outcome，二者共享幂等键。
5. **背压**：Event Bus、插件通道和 Tool Pipeline 都必须有有界队列；满载时显式拒绝或降级，不能无限缓存。
6. **隔离**：单个插件、Session 或监听器失败不应拖垮 Runtime；异常转化为结构化事件并按策略熔断。

## 16. MVP 实施顺序

### 阶段 1：可恢复的单体内核

实现 Runtime Context、Context 父子解析、Scope、Session Event Log、Projection、Native ToolRegistry、固定 ReAct Loop 和基础 checkpoint。

**验收**：运行一个含模型调用与工具调用的 Session；进程在工具结果落盘后终止，重启可从 checkpoint + event log 恢复且不重复副作用。

### 阶段 2：统一 Tool 治理

实现 ToolDefinition/Executor、schema 校验、Tool Pipeline、Policy Engine、一次性审批、审计记录、超时与取消。

**验收**：未授权的文件写入被拒绝；需审批调用进入可恢复等待；批准后仅匹配的调用可以执行。

### 阶段 3：事件扩展模型

实现 `emit`、`waterfall`、`serial`、`parallel`，绑定 Scope，加入优先级、trace、错误策略、并发上限与背压。

**验收**：监听器卸载后不再接收事件；waterfall 输出顺序稳定；parallel 超时不会阻塞 Session 结束。

### 阶段 4：Process 插件与动态 Tool

实现插件 manifest、发现、完整性校验、JSON-RPC 握手、进程监督、staged registration、drain/unload。

**验收**：Runtime 不重启即可安装一个 Python Tool；卸载后新调用不可见，在途调用按策略完成或取消。

### 阶段 5：Memory 与 Skill

实现 MemoryProvider/Registry、MemoryView、ContextAssembler、Skill manifest、SkillScope，以及 Tool/Prompt/Resource 的组合注册。

**验收**：两个 Skill 的 Memory namespace 相互隔离；ContextAssembler 在预算内产生可追溯、已去重的模型上下文。

### 阶段 6：动态 AgentLoop

将固定 Loop 提升为 LoopExecutor 协议，实现 ProcessLoopProvider、外置 VersionedState、safe point、状态迁移与回滚。

**验收**：运行中的 Session 在 checkpoint 后从内置 ReAct Loop 切换到外部 Planning Loop；迁移失败时仍可用旧 Loop 继续。

### 阶段 7：WASM 与强化隔离

实现 WASM 组件后端、受限 host functions、资源配额、缓存和确定性测试；统一 Native/Process/WASM 的兼容性测试套件。

**验收**：无文件权限的 WASM 插件无法访问宿主文件系统；超出 CPU/内存限制会被终止且不破坏 Session。

### 阶段 8：生产化

补齐插件签名与信任策略、数据迁移、可观测性、故障注入、审计导出、运维工具和协议兼容矩阵。

**验收**：通过崩溃恢复、插件故障、审批过期、日志损坏、重复请求、背压和版本升级测试。

## 17. 建议的模块边界

```text
crates/
├─ runtime-core       # Runtime、Context、Scope、基础类型
├─ runtime-session    # Event Log、Projection、Checkpoint、恢复
├─ runtime-events     # emit / waterfall / serial / parallel
├─ runtime-policy     # Permission、Approval、Audit
├─ runtime-tools      # Tool Registry 与执行管线
├─ runtime-memory     # Provider、View、ContextAssembler
├─ runtime-skills     # manifest、生命周期、能力组合
├─ runtime-loop       # Loop 协议、状态迁移、safe point
├─ runtime-plugin     # 发现、事务式注册、监督
├─ backend-process    # JSON-RPC transport
├─ backend-wasm       # WASM component host
└─ runtime-api        # 面向应用的稳定 facade
```

跨模块只共享稳定领域类型和接口。具体存储、传输和 Provider 实现位于边缘 crate，避免核心 crate 依赖某个数据库、模型 SDK 或插件语言。

## 18. 关键待决策项

以下决策会影响协议兼容性，进入实现前应形成 ADR：

1. Session Event Log 的事务边界：单机 SQLite、嵌入式 KV，还是抽象存储接口优先。
2. Process 协议采用 JSON-RPC over stdio，还是从第一版同时支持本地 socket。
3. WASM 采用 Component Model/WIT 的最低版本与宿主运行时。
4. 状态 schema 的表达方式：JSON Schema、Protobuf，或带自定义迁移函数的版本化 JSON。
5. 审批授权的粒度与默认有效期，尤其是文件 glob、网络域名和密钥读取。
6. 插件依赖是否允许互相引用；MVP 建议禁止插件直接依赖另一插件的实现，只依赖具名 capability。

## 19. 结论

这套架构的关键不是把所有组件都抽象成 trait，而是把可信内核与动态能力之间的边界设计成稳定协议。Rust Runtime 保留生命周期、状态、权限与执行权；AgentLoop、Tool、Memory 和 Skill 通过 Registry、Provider、Scope 与统一后端协议接入。

优先落地顺序应是：先建立可恢复的 Session 与不可绕过的 Tool 治理，再扩展事件机制和 Process 插件，随后引入 Memory、Skill、动态 Loop，最后增加 WASM 与生产级信任体系。这样每一阶段都能产生可验证的系统能力，也不会过早把复杂度押在插件生态或热替换上。
