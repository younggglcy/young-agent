# 从 0 到 1 构建自己的 Agent

这门课程跟随 `young-agent` 的真实构建过程，记录我们如何从一个空仓库
逐步收敛出 Agent Kernel、Coding Capability、Event Log、Tool Runtime
和 CLI Proof Surface。

它不是 API 手册，也不是 PRD 的改写。它要回答的是：如果一个人想理解
“我们为什么这样构建自己的 agent”，应该按什么顺序读、每一步要看见
什么判断、哪些产物证明这一步真的完成了。

## 读者

- 想理解 agent 系统从 0 到 1 如何拆解的人。
- 想参与 `young-agent` 后续实现，但还没有完整上下文的人。
- 想学习如何把一个模糊想法沉淀成术语、架构边界、任务和验证路径的人。

## 学习目标

读完这门课程后，读者应该能解释：

- 为什么先做 Agent Kernel，而不是先做完整产品 UI。
- 为什么第一条验证路径选择 Coding Capability。
- 为什么 CLI 只是 Proof Surface，不是最终产品形态。
- 为什么 Event Log、Tool Runtime、Model Runtime 要各自有清楚边界。
- 怎样把一个 agent 想法拆成 PRD、ADR、issues、代码和测试。

## 阅读顺序

1. [课程日志：从空仓库到 Agent Kernel](00-course-log.md)
2. 后续章节将随着实现推进补充。

## 当前项目锚点

这些文件不是课程章节，但它们是课程叙事背后的项目事实：

- [`../../../CONTEXT.md`](../../../CONTEXT.md)：项目共享术语。
- [`../../../README.md`](../../../README.md)：仓库当前形态和 crate 边界。
- [`../../prd/agent-kernel-cli-proof-surface.md`](../../prd/agent-kernel-cli-proof-surface.md)：第一阶段 PRD。
- [`../../prd/first-batch-implementation-tasks.md`](../../prd/first-batch-implementation-tasks.md)：第一批实现任务。
- [`../../adr/`](../../adr/)：已经接受的架构决策。
- [`../../issues/agent-kernel-cli-proof-surface/`](../../issues/agent-kernel-cli-proof-surface/)：可执行任务拆分。
- [`../../lessons/`](../../lessons/)：实现过程中沉淀的独立经验。

## 章节写法

新增章节时，优先使用下面的结构：

```md
# 章节标题

## 这一章解决什么问题

用人话说明本章要回答的问题。

## 我们当时怎么判断

解释选择背后的约束、取舍和被放弃的选项。

## 产物

链接实际代码、PRD、ADR、issue、测试或验证命令。

## 学到什么

提炼可复用的经验，而不是只复述改动。

## 下一步

说明读者接下来应该读什么，或者项目下一步要补什么。
```
