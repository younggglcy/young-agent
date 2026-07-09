# 课程日志：从空仓库到 Agent Kernel

这份日志按构建顺序追踪 `young-agent` 的成长。它不是 commit history 的
替代品，而是把每个里程碑整理成学习者能理解的路线图。

## 路线图

| 顺序 | 阶段 | 关键产物 | 这一阶段学什么 |
| --- | --- | --- | --- |
| 0 | 创建空仓库 | `README.md` | 先把项目从“想做一个 agent”落到一个可以持续演进的仓库。 |
| 1 | 统一语言 | `CONTEXT.md` | 在写代码前先定义 Agent Kernel、Capability Pack、Surface、Event Log 等术语，减少后续讨论歧义。 |
| 2 | 收敛第一阶段 | `docs/prd/agent-kernel-cli-proof-surface.md` | 把“构建自己的 agent”缩小成可验证的第一阶段：Rust Agent Kernel + Coding Capability + CLI Proof Surface。 |
| 3 | 记录关键架构判断 | `docs/adr/` | 用 ADR 固化已接受的取舍，例如 Rust core、coding first、canonical Event Log first、defer MCP runtime。 |
| 4 | 拆成可执行任务 | `docs/prd/first-batch-implementation-tasks.md`, `docs/issues/agent-kernel-cli-proof-surface/` | 把宏观方向拆成 agent 可以接力执行的小任务，并保留任务之间的依赖顺序。 |
| 5 | 搭出 Rust workspace | `Cargo.toml`, `crates/` | 让架构边界先体现在包结构上：model runtime、agent runtime、tool runtime、event store、coding capability、CLI proof surface。 |
| 6 | 沉淀协作规则 | `CONTRIBUTING.md`, `AGENTS.md`, `docs/lessons/` | 让人和 agent 都知道如何维护这个仓库，尤其是 PR 标题、文档位置和经验沉淀方式。 |
| 7 | 补上可视化架构 | `docs/diagrams/` | 用图把长期架构和 agent harness 关系讲清楚，帮助读者跨过纯文字理解的门槛。 |
| 8 | 开始课程化沉淀 | `docs/courses/` | 把 PRD、ADR、issues、代码和经验串成面向学习者的顺序材料。 |

## 当前课程进度

课程现在只完成了入口和路线图。下一批章节可以按真实实现顺序展开：

1. 为什么一开始要先建立共享语言。
2. 为什么选择 Agent Kernel + Coding Capability 作为第一阶段。
3. Rust workspace 的 crate 边界如何对应架构边界。
4. Model Runtime、Tool Runtime、Agent Runtime 的第一版合同如何相互配合。
5. Canonical Event Log 为什么要早于产品化 surface。
6. 如何用 FakeModelClient 和 fake tools 建立可测试的 agent loop。

## 写作原则

- 每一章都从一个真实问题开始。
- 每一章都链接到实际产物，而不是只讲抽象道理。
- 每一章都保留当时的约束：哪些东西现在做，哪些东西故意延后。
- 课程内容可以比代码慢半拍，但不能和当前仓库事实冲突。
