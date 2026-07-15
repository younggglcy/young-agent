# Capability Manifest 只声明，不执行

## 背景

Issue #6 为第一阶段加入内置 TOML Capability Manifest、Tool Runtime 和
Coding Capability 的初始工具定义。三者都与“工具”有关，但承担的职责不同；
如果让 manifest 直接创建进程、加载外部代码或处理审批，Agent Kernel 的边界会
很快退化成一个难以替换的插件系统。

## 经验

Capability Manifest 是声明层：它描述 capability 身份、tool name、description、
input schema、safety class 和预留的 MCP mapping。`young-tool-runtime` 负责解析和
验证这份通用合同；`young-capability-coding` 只负责内嵌自己的 TOML，并将声明的
工具绑定到 Tool Runtime。

Tool Runtime 是执行分发层：它验证并保存 `ToolDefinition -> ToolHandler` 的注册
关系，完成精确查找、重复注册保护、unknown-tool 失败和 `ToolResult`
correlation。Agent Runtime 只依赖 external `ToolDispatcher` seam，具体 coding tool
只实现 internal `ToolHandler` seam，不能绕过 registry 与 manifest policy 直接接入。
即使一个 handler 当前只搭配静态 policy，它也必须显式实现 call-dependent 分类，并返回
`ToolCallPolicy::{Allow, RequiresApproval, Reject}`；这样未来把 definition 改为
`CallDependent` 时不会因默认值而 fail-open，也能在 executor 运行前拒绝明显危险的调用。handler 输出也会在
Tool Runtime 边界归一化，不能伪造 Agent Runtime 保留的审批拒绝结果。
安全声明会映射到现有 `ToolApprovalPolicy`；其中 `call_dependent` 保留给具体
handler 按调用分类。Tool Runtime 只分类一次并生成绑定完整 `ToolCall` 的
`PreparedToolCall`；审批提示和审批决定仍由 Agent Runtime 驱动，执行时消费同一
plan，并同时校验 originating dispatcher identity 与按 `ToolCallId` 关联的授权，
避免把低权限 runtime 的 plan 交给另一个 runtime 绕过审批。

第一阶段只提供字符串 parser，并由内嵌 manifest 的调用路径使用；它不提供目录
扫描、filesystem loader 或用户代码加载。Manifest 中存在 MCP metadata 也只表示
未来可以做映射，不表示当前已经有 MCP server discovery、process lifecycle 或
protocol framing。

## 下次怎么做

- Issue #7 已把显式 stub handler 替换成绑定 `CodingWorkspace` 的真实 coding
  tools；workspace 与 git 概念继续留在 capability 内，没有进入通用 Tool Runtime。
- Issue #8 实现 command policy 时，保持“manifest 声明 `call_dependent`、具体
  handler 分类、Agent Runtime 处理审批事件”的方向，不让 `run_command` 自己
  发 Agent Event。
- 如果以后支持用户 Capability Pack，新增独立 discovery/loading boundary；
  不要把当前的内嵌字符串 parser 直接扩成不受控的插件加载器。
