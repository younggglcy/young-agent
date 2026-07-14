# Command Approval Policy 不是 Shell Sandbox

## 背景

Issue #8 需要让低风险 read/validation command 自动运行，同时让 mutating、destructive、
dependency-installing、background 和 cross-workspace command 进入审批。`run_command`
接收的是 shell source；如果只按第一个单词做 allowlist，`cargo test && touch marker`、
redirection、command substitution 或工具自带的 helper hook 都可能绕过分类。

## 经验

Command Approval Policy 是“执行前如何决定”，不是“执行时如何隔离”。第一阶段先对最多
64 KiB 的 command 做有界、保守的 shell lexical scan：只有每个 simple command 都属于明确的
low-risk 形态，并且没有 redirection、dynamic expansion、background execution 或危险的
tool-specific option，组合命令才可以自动运行。无法确认的命令进入审批；malformed、过大、
过度复杂、privilege-elevating 和明确以 filesystem root 为目标的命令直接拒绝。

分类结果必须是 `Allow / RequiresApproval / Reject` 三态。Tool Runtime 只分类一次，并把完整
`ToolCall` 和 disposition 放进 `PreparedToolCall`。Agent Runtime 用同一个 plan 生成
`ApprovalRequested`，记录 `ApprovalResolved`，再用匹配 `ToolCallId` 的 authorization 消费它；
这样 CLI 展示的 command、Event Log 记录的 decision 与最终执行不会漂移。拒绝审批时只产生
canonical `approval_denied` result，不调用 executor。

即使 policy 检查了 `..`、absolute path 和现存 symlink，shell 仍运行在本机进程权限下。
handle-bound cwd 解决的是 cwd handoff race，不会把已批准的 shell 变成 filesystem sandbox。
需要更强保证时，应新增独立的 sandbox / remote workspace execution boundary，而不是继续把
更多启发式规则堆进 classifier。

## 下次怎么做

- 新增自动允许的 program 前，先检查它是否存在写文件、执行 helper、加载 config 或启动
  background process 的 option。
- unknown syntax 保持 fail closed；不要为了提高 allow rate 放宽 dynamic expansion 或
  redirection。
- approval reason 要能直接给 Surface 展示，说明风险类别，而不是只写“需要审批”。
- 用完整 Agent Run + FakeModelClient + Canonical Event Log 测试 allow、approve、deny；不要只测
  classifier 的内部 helper。
- 如果需求变成“即使批准也不能越过 workspace”，停止扩展 policy，转向真正的执行隔离边界。
