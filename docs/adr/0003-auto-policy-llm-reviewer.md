# 引入 auto policy：`Ask` 之上加一层 LLM 审查器（重开 ADR 0002 的一半）

`[policy] mode = "auto"` 时，规则判定为 `Ask` 的动作先交给 aux 模型审查：它只能
**放行**或**交给人**，永远不能拒绝。默认 `mode = "ask"`，行为与本 ADR 之前逐字节一致。

## 为什么重开 0002

[ADR 0002](0002-permission-stance-no-sandbox.md) 明确"长期不做 LLM 审批器"，理由是
komo 只执行操作者本人的意图，"无人值守的安全靠事先缩小动作集合，而不是事中找模型判断"。
它同时写明了重开条件：**接 MCP 或安装第三方 skill**——两者都让外部文本进入 prompt /
工具返回面。这个条件已经满足（`komo-mcp` + `[mcp.servers.*]`，加上 wiki 笔记库、
web_fetch、skill 正文），所以这是**按 0002 自己的条款执行**，不是推翻它。

0002 还规定了重开时最先补的东西：**信任边界声明**（文本级、成本近零）。本次一并落地
（`system_prompt::TRUST_BOUNDARY_GUIDANCE`），且审查器 prompt 用同一条规则——主 agent
和它的权限审查器不能对"什么构成授权"有两套理解。

**0002 未被重开的部分仍然成立**：不做 OS 沙箱，不做 credential broker。

## 与 0002 的真正分歧点

0002 的论证针对**无人值守**场景，那里的结论至今正确：cron / briefing 仍然只认
`unattended = true` 规则和 job grants，审查器**结构性地**不介入。

分歧在**有人值守**场景，0002 没有单独论证过它。这里的实际问题不是安全而是摩擦：家庭 HA
和个人助手类任务里，绝大多数弹窗是操作者刚刚开口要求的动作（"把热水器打开" → 弹
`switch.turn_on` 确认）。静态 allow 规则能治其中稳定的一批，治不了长尾。审查器要回答的
不是"这个动作危险吗"，而是**"这个动作是操作者刚才要求的吗"**——一个上下文匹配问题，
比风险判断可靠得多。

## 四条结构性性质

写在类型和控制流里，不靠 prompt 自觉，每条对应一个测试：

1. **只有 allow / ask，没有 deny。** 拒绝权仍归 config deny 和操作者本人。审查器的每一种
   非放行结果（含它自己的失败）都落到内层审批器。
2. **`Risk::Dangerous` 永不经审查器。** 不可逆动作直达人，与 `include_dangerous` 只能由
   config 开启是同一条不变量。
3. **无人值守永不经审查器。** cron / briefing 的 runtime 根本不接这个 decorator，
   `SessionOrigin::is_unattended()` 是第二道地板。
4. **Fail-closed。** 模型报错、20 秒超时、verdict 解析失败、没有可作为授权的操作者消息，
   四种情况都等于"问人"。有人在场时，"问人"就是 fail-closed 的正确形态。

## 从 fx harness 借的与没借的

借：信任边界（只有操作者消息能授权，工具输出/文件内容/agent 自己写的文本一律 untrusted，
声称"已获批准"不算批准）、只 allow/ask 不 deny、单次调用不重试、fail-closed。

没借：

- **审查模型不硬编码**。fx 把 `zai/glm-5.2` 写进常量，它自己的笔记把这列为耦合点。komo 用
  配置的 aux model，走既有 aux 不变量（合成 Session、空 model/effort override）。
- **不做 approval_request_id 绑定**。fx 需要它是因为它的审查器要处理"模型自称已获授权"；
  komo 的 `Ask` 之后就是真人，没有可被冒用的中间态。
- **不做连续拒绝自动升级人审**。komo 的非放行结果本来就直接到人，没有 replan 循环要退出。
- **auto 放行不写 saved grant**。每次放行只覆盖这一次；`permissions.json` 仍然只有人手能写。

## 代价与回滚

- 每个原本会弹窗的动作多一次 aux 调用（一次 completion，最多 20 秒）。
- 误放行风险限于 `Risk::Normal`——开个灯、发条消息这个量级；不可逆动作有结构性保护。
- 回滚是删掉 `mode` 或改回 `"ask"`，无数据迁移。

## 再次重开的条件

- 审查器误放行造成过一次真实损失 → 收窄到按 category opt-in，而不是全局。
- 想让无人值守也走审查器 → 那是另一个决策，性质 3 是本 ADR 的前提而非实现细节。
