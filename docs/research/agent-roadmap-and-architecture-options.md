# 自研 Agent Roadmap 与技术选型调研

日期：2026-07-07

## 范围

本报告合成四类材料：

- 前期 OpenAI/Anthropic agent 构建指南阅读笔记：`/Users/young/Documents/Codex/2026-07-07/re/outputs/openai-ai-agents-guide-reading-notes.md`、`/Users/young/Documents/Codex/2026-07-07/re/outputs/anthropic-agent-building-resources.md`。
- 前期 Pi 仓库实现分析：`/Users/young/projects/pi/docs/research/repo-implementation-overview.md`。
- 本轮对 `~/projects/hermes-agent` 的源码调研。
- 本轮对 Codex、OpenHands、aider、SWE-agent、goose、Cline、Continue、LangGraph、AutoGPT/Forge、CrewAI 等开源 agent 项目的 README/docs/source 横向调研。

这里的 `pi mono` 指 `~/projects/pi` 这个 Pi monorepo。第一阶段把它作为结构参考，不作为 Agent Kernel 的实现依赖或必需 provider gateway。

## 结论先行

当前架构决策更新为：**Rust core + TypeScript surface**。第一阶段收窄为 **Agent Kernel + CLI Proof Surface + 必要验证**，不做桌面、IDE、web 或完整 TypeScript surface。更稳的起点是一个通用 agent kernel，并用 coding capability 做第一个端到端验证场景。

第一阶段核心模块：

1. `model-runtime`：Rust crate，统一模型传输和事件协议。
2. `agent-runtime`：Rust crate，单 agent loop、state machine、budget、interrupt、retry。
3. `tool-runtime`：Rust crate，工具定义、权限、执行、结果规整、approval。
4. `event-store`：Rust crate，append-only run log、replay、trace。
5. `coding` capability：第一组工具、instructions、evals，用来验证 agent 能力，但不进入通用 core。
6. `agent-cli`：Rust CLI Proof Surface，用来验证 kernel，不代表最终 CLI 产品形态。

这回答了“是不是应该先从 Model 上的 SDK 层开始做起”：是，但前提是这个 SDK 层不是 `openai.chat.completions` 的薄包装，而是一个为 agent loop 服务的 runtime contract。第一版至少要稳定这些概念：`ModelClient.stream()`、`Message`、`ToolDefinition`、`ToolCall`、`ToolResult`、`StreamEvent`、`Usage`、`ModelCapabilities`、`ErrorKind`、`ProviderOptions`、trace metadata。

对代码起点的判断：

| 选项 | 判断 |
| --- | --- |
| 直接 fork Hermes | 不推荐作为基础。Hermes 是成熟产品和经验库，但不是干净 SDK。它的 `AIAgent` 仍是状态中心，`conversation_loop` / `tool_executor` 通过 `agent` attribute lookup 访问大量状态，适合学习约束，不适合直接作为自己的底座。 |
| 直接采用 LangGraph/CrewAI 托管核心 | 暂不推荐。它们适合 Python workflow / multi-agent orchestration，但如果目标是本地 coding agent、IDE/CLI、MCP、权限、事件流和可恢复执行，自建 runtime 更能守住产品控制权。 |
| 用 Pi monorepo 作为结构参考或 fork 基础 | 值得优先评估。Pi 的 `pi-ai`、`pi-agent-core`、`pi-coding-agent`、`pi-tui` 分层比 Hermes 更清楚，更适合作为自研基础形状。 |
| 完全自研 | 可行，但第一阶段应只自研 `model-runtime + agent-runtime` 的 contract，不要自研完整 provider matrix、UI、插件市场、复杂 memory 和 multi-agent。 |
| 使用 OpenHands/Cline/goose 作为运行时 | 适合作为对照和局部借鉴。OpenHands 更像 code-agent SDK；Cline/goose 更像多入口产品 runtime；但直接绑定会引入它们自己的产品假设。 |

我的推荐路线是：以 Rust 实现通用 agent kernel，以 Pi 的模块边界作为结构参考，借 Hermes 的产品经验和 hard-won invariants，并参考 Codex/goose 这类 Rust-native 本地 agent 的工程形态。第一梯队 provider 明确为 DeepSeek API、Qoder API、Codex API；其他 provider 延后。agent loop、tool schema、event log 不依赖任何一个 provider 的类型。

## 前期材料合成

OpenAI 的指南把 agent 基础组件压缩成 `Model + Tools + Instructions`，并强调先判断是否真的需要 agent、建立 eval baseline、从 single-agent loop 开始，再逐步加入 guardrails、human review、tracing 和更复杂 orchestration。对应笔记见 `/Users/young/Documents/Codex/2026-07-07/re/outputs/openai-ai-agents-guide-reading-notes.md:11-39`。

Anthropic 的 `Building effective agents` 路线更强调 workflow 与 agent 的区别：不要默认上 agent，先从最简单可行方案开始；agentic systems 通过 latency/cost 换 task performance，因此必须证明这个 tradeoff 值得。后续 Claude Agent SDK 材料把 loop 讲成 `gather context -> take action -> verify work -> repeat`。对应笔记见 `/Users/young/Documents/Codex/2026-07-07/re/outputs/anthropic-agent-building-resources.md:9-27`。

Pi 的实现分析给了一个很重要的结构参照：`pi-ai` 是模型/API 抽象层，`pi-agent-core` 是模型无关 agent loop，`pi-coding-agent` 是 CLI/SDK/RPC 产品层，`pi-tui` 是终端 UI。这个分层比 Hermes 更接近我们要的“自研 agent 底座”。对应笔记见 `/Users/young/projects/pi/docs/research/repo-implementation-overview.md:19-25`、`/Users/young/projects/pi/docs/research/repo-implementation-overview.md:61-81`。

从这三份前期材料得到的原则：

1. 先做 single-agent，不从 multi-agent 默认起步。
2. 先做 evaluation 和 trace，不等“做大了”再补。
3. Tools 是契约，不是函数列表；要有权限、幂等性、错误语义、审计和测试。
4. Model 层要为 loop 服务，而不是只屏蔽不同 SDK 的调用参数。
5. Coding 是第一个 capability pack，不是 kernel 的边界。
6. UI、gateway、scheduler、memory、plugins 都应晚于 runtime contract。

## Hermes 实现解剖

### 产品形态

Hermes 自述是一个 personal AI agent，同一个 core 跑 CLI、messaging gateway、TUI 和 Electron desktop；它会跨 session 学习，支持 memory + skills、subagents、scheduled jobs、terminal 和 browser。源码约定里明确写了两个核心设计镜头：per-conversation prompt caching is sacred，以及 core 是 narrow waist、能力应放在 edges。见 `/Users/young/projects/hermes-agent/AGENTS.md:7-27`。

README 也显示 Hermes 已经是完整产品而不是 SDK 原型：它有 self-improving learning loop、memory/skills/session search/user model、多平台 gateway、cron、subagent delegation、六类 terminal backend、trajectory generation/compression 等能力。见 `/Users/young/projects/hermes-agent/README.md:19-31`。

依赖策略也体现出产品化成熟度。核心依赖精确 pin，且注释要求只有每个 session 都用的包进入 core dependencies，provider-specific 或 tool-specific 依赖放 extras/lazy install，以降低 supply-chain blast radius。见 `/Users/young/projects/hermes-agent/pyproject.toml:24-45`、`/Users/young/projects/hermes-agent/pyproject.toml:143-164`。

### Core narrow waist

Hermes 的 Footprint Ladder 值得直接借鉴：优先 extend existing code，其次 CLI command + skill，再是 service-gated tool、plugin、MCP server，最后才 new core tool。这个排序的核心理由是每个 core model tool 都会进入每次 API call 的 schema。见 `/Users/young/projects/hermes-agent/AGENTS.md:182-211`。

它的 `_HERMES_CORE_TOOLS` 也说明 core 已经很大：web、terminal/process、file、vision/image、skills、browser、TTS、todo/memory、session_search、clarify、execute_code/delegate_task、cron、Home Assistant、kanban、computer_use 等都在内。即便如此，desktop project tools 仍被刻意排除在 core schema 外，只在 GUI gateway 启用。见 `/Users/young/projects/hermes-agent/toolsets.py:29-80`。

对我们来说，这条经验比具体工具列表重要：第一版 toolset 应小到能解释、能测、能缓存，而不是“把可能有用的都塞进去”。

### AIAgent 与 loop

Hermes 的 `AIAgent` 是实际中心。构造函数暴露 provider/base_url/api_mode/model、toolsets、callbacks、platform/user/chat/thread、session DB、iteration budget、fallback model、credential pool、checkpoint 等大量参数，然后 forward 到 `agent.agent_init.init_agent`。见 `/Users/young/projects/hermes-agent/run_agent.py:393-563`。

`agent/conversation_loop.py` 的 docstring 说明它是从 `run_agent.AIAgent` 抽出的约 3900 行 turn loop，仍以 parent `AIAgent` 为第一个参数，通过 attribute lookup 访问状态。见 `/Users/young/projects/hermes-agent/agent/conversation_loop.py:1-15`。

loop 的关键流程包括：

- per-turn prologue：stdio guard、retry counter reset、message sanitization、system prompt restore/build、crash persistence、preflight compression、plugin hook、external-memory prefetch。见 `/Users/young/projects/hermes-agent/agent/conversation_loop.py:568-592`。
- 主循环用 `max_iterations` 和 `iteration_budget` 控制，支持 grace call 和 interrupt。见 `/Users/young/projects/hermes-agent/agent/conversation_loop.py:638-663`。
- plugin/external memory context 只注入 API-call copy 的当前 user message，不 mutate canonical messages。见 `/Users/young/projects/hermes-agent/agent/conversation_loop.py:787-808`。
- system prompt built once per session 并 byte-stable replay；plugin context 不进 system prompt，避免破坏 prompt cache prefix。见 `/Users/young/projects/hermes-agent/agent/conversation_loop.py:832-851`。
- Anthropic prompt caching、orphan tool result sanitization、thinking-only turn cleanup、tool-call JSON normalization 都在 API-call copy 上完成。见 `/Users/young/projects/hermes-agent/agent/conversation_loop.py:883-947`。

这个实现说明 Hermes 的经验非常丰富，但也说明它不是一个干净的 foundational SDK：状态和产品约束已经深深进入 `AIAgent`。

### Model/provider 层

Hermes 有 `ProviderProfile`，把 provider 的 auth、endpoint、client quirks、request quirks、fallback models、capabilities 和 hooks 放在声明式 profile 中；但 profile 不拥有 client construction、credential rotation 或 streaming，这些仍在 `AIAgent`。见 `/Users/young/projects/hermes-agent/providers/base.py:1-10`、`/Users/young/projects/hermes-agent/providers/base.py:38-217`。

这给我们的启示是两面：

- 值得借鉴：provider behavior 应声明化，避免到处散落 boolean flags。
- 不应照搬：如果新系统从头做，client construction、credential rotation、streaming、retry、usage、capability probing 应属于 `model-runtime`，不要让 `Agent` 变成 provider 细节宿主。

### Tool registry 与 execution

Hermes 的 `model_tools.py` 是 registry 上的一层 thin orchestration。每个 tool file 在 import 时自注册 schema、handler 和 metadata。MCP discovery 被从 module side effect 移出，避免 gateway asyncio event loop 被慢 MCP server 冻住。见 `/Users/young/projects/hermes-agent/model_tools.py:1-21`、`/Users/young/projects/hermes-agent/model_tools.py:184-208`。

`tools/registry.py` 是中心 registry：工具文件通过 `registry.register()` 注册；registry 有 `ToolEntry`，支持 schema、handler、check_fn、async、max_result_size 和 dynamic schema overrides；`check_fn` 有 TTL cache 和 last-good grace，避免外部 backend probe 瞬时失败导致工具突然从 schema 消失。见 `/Users/young/projects/hermes-agent/tools/registry.py:1-15`、`/Users/young/projects/hermes-agent/tools/registry.py:78-139`。

registry 还使用 `RLock` 和 generation counter 处理动态 MCP refresh/plugin mutation，definition cache 可以 keyed on generation。见 `/Users/young/projects/hermes-agent/tools/registry.py:208-230`。

`get_definitions()` 只返回 `check_fn` 通过的 OpenAI-format tool schemas，并应用 dynamic schema overrides。`dispatch()` 统一执行 sync/async handler，异常转 JSON error。见 `/Users/young/projects/hermes-agent/tools/registry.py:521-600`。

工具执行层的经验也值得借鉴：

- tool guardrails 是 side-effect-free primitives，runtime 决定 warn/block/halt，而不是 guardrail 直接做副作用。见 `/Users/young/projects/hermes-agent/agent/tool_guardrails.py:1-7`。
- mutating/idempotent tools 被显式分类，重复失败和 no-progress 有阈值。见 `/Users/young/projects/hermes-agent/agent/tool_guardrails.py:20-82`。
- delegate subagents 默认隔离 context、限制 toolsets、有自己的 terminal sessions，parent 只看 delegation call 和 summary，不看 child intermediate tool calls/reasoning。见 `/Users/young/projects/hermes-agent/tools/delegate_tool.py:1-17`。
- subagent 默认不允许递归 delegation、clarify、memory、send_message、execute_code、cronjob 等工具，且 dangerous command approval 默认 auto-deny。见 `/Users/young/projects/hermes-agent/tools/delegate_tool.py:44-112`。

### State、memory、compression

Hermes 用 SQLite state store 替代 per-session JSONL，支持 WAL、FTS5、compression-triggered session splitting、source tagging。见 `/Users/young/projects/hermes-agent/hermes_state.py:1-15`。

它显式处理 WAL 在网络文件系统上的不兼容，失败时回退 `journal_mode=DELETE`，牺牲并发但保证功能可用。见 `/Users/young/projects/hermes-agent/hermes_state.py:132-148`。

context compression 是单独模块，用 auxiliary model 总结中间 turns，保护 head/tail。summary prefix 明确标注 compressed content 是 reference only，不是 active instructions；metadata key 以下划线开头，wire sanitizer 会去掉，避免 strict provider 拒绝 unknown fields。见 `/Users/young/projects/hermes-agent/agent/context_compressor.py:1-17`、`/Users/young/projects/hermes-agent/agent/context_compressor.py:44-86`。

对我们来说，第一版不一定要上 SQLite + FTS5 + memory，但 event log 和 compaction metadata 的设计不能晚到最后再补；否则很难做 replay、eval、debug 和恢复。

### Plugins 与 surfaces

Hermes plugin loader 支持 bundled、user `~/.hermes/plugins`、project `./.hermes/plugins` opt-in、pip entry point 四类来源；目录 plugin 需要 `plugin.yaml` 和 `__init__.py register(ctx)`，plugin 可以注册 hooks 和 tools。见 `/Users/young/projects/hermes-agent/hermes_cli/plugins.py:1-32`。

Hermes 的 UI/surface 也很多：classic CLI、Ink TUI、dashboard embedded TUI、Electron desktop、messaging gateway。`AGENTS.md` 明确 TUI 是 Node Ink 通过 stdio JSON-RPC 连 Python `tui_gateway`，Python owns sessions/tools/model calls/slash logic；desktop 是独立 Electron + React + JSON-RPC/WS surface，不依赖 dashboard。见 `/Users/young/projects/hermes-agent/AGENTS.md:428-505`。

这强化了一个选型结论：如果我们目标是构建自己的底座，不应从 surface 开始。UI/gateway 只能在 runtime event contract 稳定后自然接入。

### Hermes 可借鉴与不应照搬

应借鉴：

- prompt cache invariants：system prompt byte-stable，ephemeral context 注入 user message copy。
- core narrow waist：新增能力先问是否能是 command/skill/plugin/MCP，而不是 core tool。
- tool registry：check_fn TTL、last-good grace、generation counter、dynamic schema overrides。
- tool guardrails：side-effect-free decision primitives，mutating/idempotent 分类。
- subagent isolation：child context、tool blocklist、approval defaults、parent 只看 summary。
- state/event/compression：持久化、FTS/search、compaction metadata、reference-only summary。

不应照搬：

- `AIAgent` 作为 all-in-one 状态中心。
- provider streaming/auth/retry 继续挂在 agent 身上。
- 一开始复制 Hermes 的 tool surface、gateway、desktop、cron、memory、skills。
- 用历史兼容需求驱动第一版接口形状。

## 开源项目横向对比

### Codex

OpenAI Codex CLI 是一个本地运行的 terminal coding agent；公开仓库当前主 README 面向 Rust 实现，历史 discussion 中维护者说明 `rust-v0.2.0` 是第一个 Rust-powered official release，README 也从旧 TypeScript 实现切到 Rust 实现。来源：[openai/codex README](https://github.com/openai/codex)、[Rust CLI cutover discussion](https://github.com/openai/codex/discussions/1266)、[Codex releases](https://github.com/openai/codex/releases)。

架构启示：Codex 证明 Rust 非常适合本地 agent core：分发为单 binary、进程/文件/PTY/权限控制更直接、性能和安装体验更稳定。对我们来说，它支持“Rust core + TS surface”的路线，而不是“全栈 Rust”。Rust 应拥有 runtime、tool execution、event store、sandbox 这类高约束层；TS 仍适合 IDE、Electron、Web、配置 UI 和插件开发体验。

### OpenHands / software-agent-sdk

OpenHands Software Agent SDK 的定位是 Python/REST APIs for agents that work with code。它的公开 README 展示了 `LLM`、`Agent`、`Conversation`、`Tool`、`TerminalTool`、`FileEditorTool`、`TaskTrackerTool` 这样的入口，支持 local workspace 或 Docker/Kubernetes ephemeral workspace。来源：[OpenHands Software Agent SDK](https://github.com/OpenHands/software-agent-sdk)。

架构启示：这是最像“code-agent SDK”的外部参照。它把 LLM、agent、conversation、event store、tools、MCP、plugin 分开，适合作为我们定义 package boundaries 的参考。

### aider

aider 是 terminal pair programmer，强项是 repo map、git integration、自动 commit、lint/test auto-fix、voice/images/web 等面向开发者的高效体验。来源：[Aider README](https://github.com/aider-ai/aider)、[Aider repository map docs](https://aider.chat/docs/repomap.html)、[Aider git integration docs](https://aider.chat/docs/git.html)。

架构启示：aider 不是通用 agent runtime，但它证明 coding agent 的 context UX 很关键。repo map、显式文件集合、git-first undo/review 可能比复杂 multi-agent 更早产生价值。

### SWE-agent

SWE-agent 的核心贡献是 Agent-Computer Interface。官方 ACI 文档强调工具和交互格式会显著影响 agent 表现；它提供专门 file viewer，而不是直接 `cat` 文件；edit command 会跑 linter，不让语法错误编辑落地。来源：[SWE-agent ACI docs](https://swe-agent.com/0.7/background/aci/)。

架构启示：工具不是普通 shell wrapper。工具返回多少上下文、怎么分页、什么时候拒绝修改、是否内置 lint/test feedback，都会改变 agent 的能力上限。第一版 tool-runtime 应投入设计，而不是只暴露 `bash`。

### goose

goose 是 Rust 原生 local agent，公开定位是 desktop app、CLI、API for code/workflows/everything，并强调 15+ providers、70+ MCP extensions、API keys or subscriptions via ACP。来源：[goose README](https://github.com/aaif-goose/goose)、[goose docs](https://goose-docs.ai/)。

架构启示：MCP extension manager 可以成为核心增长机制。goose 的方向支持一个判断：如果我们想要长期扩展能力，MCP/extension 不是边角料，而是 agent-runtime 同级的基础设施。

### Cline

Cline 当前公开 README 把自己描述为 SDK、CLI、Kanban、VS Code extension、JetBrains plugin 共享同一 engine；SDK 支持 custom tools、multi-agent teams、connectors、scheduled automations。来源：[Cline README](https://github.com/cline/cline)、[Cline CLI](https://cline.bot/cli)。

架构启示：成熟 coding agent 正在把产品 runtime 抽成 SDK。Cline 的方向和我们目标接近，但直接采用它会继承 Cline 自己的 IDE/product assumptions；更适合作为 event/runtime 分层参考。

### Continue

Continue 是 CLI、VS Code extension、JetBrains plugin 形态的 coding agent，但当前官方 README 标注 `continuedev/continue` repo 不再活跃维护且 read-only。来源：[Continue README](https://github.com/continuedev/continue)、[Continue docs](https://docs.continue.dev/)。

架构启示：它的多 IDE host、LLM provider abstraction、MCP connection 有参考价值；但作为代码起点风险较高，因为维护状态已经变化。

### LangGraph

LangGraph 不是 coding agent 产品，而是 orchestration runtime。官方 overview 说它聚焦 durable execution、streaming、human-in-the-loop、persistence；它用 `StateGraph`、nodes、edges、checkpointer、store 表达状态和执行。来源：[LangGraph overview](https://docs.langchain.com/oss/python/langgraph/overview)、[LangGraph persistence](https://docs.langchain.com/oss/python/langgraph/persistence)。

架构启示：我们应借它的显式状态机和 durable execution 思想，但不一定把核心托管给它。若目标是 TypeScript/IDE/local runtime，LangGraph 更像设计参照或某些 Python workflow 的 adapter。

### AutoGPT / Forge

AutoGPT 现在更像 build/deploy/run continuous agents 的平台；Classic Forge 是 ready-to-go toolkit，处理 boilerplate，并配套 agbenchmark 和 agent protocol。来源：[AutoGPT README](https://github.com/Significant-Gravitas/AutoGPT)、[AutoGPT Classic docs](https://agpt.co/docs/classic)。

架构启示：Agent Protocol 和 benchmark 思路值得借鉴，尤其是把 agent 跑法变成可评估任务/step，而不是只看聊天 transcript。

### CrewAI

CrewAI 的核心是 Crews 和 Flows，更偏 multi-agent orchestration。它有 Agent、Crew、Task、Flow、event bus、tools/MCP/sandbox 集成。来源：[CrewAI README](https://github.com/crewAIInc/crewAI)。

架构启示：如果目标是业务 workflow / multi-agent team，它很合适；如果目标是自己的本地 coding agent runtime，它的抽象可能过早把问题推向 multi-agent，而不是先稳定 single-agent loop。

## 技术选型矩阵

| 维度 | Rust core + TS surface | Pi monorepo | Hermes | OpenHands SDK | LangGraph/CrewAI |
| --- | --- | --- | --- | --- | --- |
| Model/provider 控制 | 高，facade 自有，provider 可先接 Pi/OpenAI-compatible | 中高，取决于 Pi provider 抽象 | 中，provider 细节仍绑在 AIAgent 周围 | 中 | 低到中，通常用框架模型抽象 |
| Agent loop 控制 | 高，Rust state machine 自有 | 高，`pi-agent-core` 可参考 | 中，历史状态重 | 中 | 低到中，由框架运行 |
| Tool/runtime 控制 | 高，适合文件/进程/PTY/sandbox/approval | 中高 | 高但复杂 | 中高 | 中 |
| Event/replay/eval | 可从第一天设计为 append-only protocol | Pi 已有 event/session 方向 | Hermes 很强但耦合 | OpenHands 很强 | LangGraph 强在 durable state |
| MCP/extensions | Rust core 预留 registry/protocol，TS 可做管理 UI | 需确认现状 | Hermes 强 | OpenHands 强 | 框架可接但产品边界弱 |
| 分发体验 | 高，单 binary，适合本地 CLI | 取决于 Node/包结构 | Python + Node 混合，重 | Python SDK | Python workflow |
| Surface 迭代 | TS surface 快；Rust core 稳 | TS 快 | Python/TS 混合 | Python | Python |
| 长期所有权 | 高 | 中高 | 低 | 中 | 低到中 |
| 主要风险 | Rust 早期接口变更成本较高；provider SDK 生态不如 TS/Python | 被现有包形状约束 | 继承复杂产品历史 | 被 code-agent SDK 假设约束 | 产品控制权被框架形状约束 |

推荐组合：

1. **实现形态**：Rust core + TypeScript surface。
2. **产品边界**：通用 agent kernel + capability packs；coding 只是第一个 pack。
3. **结构参考**：Pi 的 `model-runtime / agent-runtime / product surface` 分层。
4. **产品约束参考**：Hermes 的 prompt cache、narrow waist、tool gating、plugin/MCP、state/compression。
5. **Rust 参照**：Codex/goose 的本地 agent binary、provider/runtime/tool/event 分层。
6. **ACI 细节参考**：SWE-agent/aider 的 coding tools 设计，先把读写代码、patch、测试反馈做好。
7. **Durable state 参考**：LangGraph 的显式状态和持久化思路，但不把核心 loop 托管给 LangGraph。

## 推荐 Roadmap

### Phase 0: 定义通用 kernel 边界和 coding eval seed

目标不是写一个“coding-only framework”，而是先定义通用 kernel，然后用 coding 场景证明它能处理真实 agent 难点。

coding seed tasks 先选 3-5 个：

- repo 代码阅读并输出架构报告。
- 小型 bugfix：读文件、改文件、跑测试、解释 diff。
- docs/research 任务：网页/本地资料整理成可引用 Markdown。
- 可选：qoder-work 里一个低风险、本地可验证的维护任务。

每个任务要有：输入、允许 capability、期望 artifact、失败条件、人工介入条件、可回放 trace。

产物：

- `docs/evals/seed-tasks/*.md`
- `docs/architecture/agent-kernel-contract.md`
- `docs/architecture/capability-pack-contract.md`
- `docs/architecture/cli-proof-surface.md`
- `capability-coding` 的内置 TOML manifest 草案
- 一个最小 trace schema 草案

### Phase 1: Rust `model-runtime`

先做模型 facade，但只做支撑 agent runtime 的最小合同。Rust core 里的核心 trait 可以从这个形状起步：

```rust
#[async_trait::async_trait]
pub trait ModelClient: Send + Sync {
    async fn stream(
        &self,
        request: ModelRequest,
        context: ModelCallContext,
    ) -> Result<ModelStream, ModelError>;
}

pub enum ModelStreamEvent {
    MessageStart { id: String, model: String },
    TextDelta { text: String },
    ReasoningDelta { text: String, encrypted: bool },
    ToolCallDelta { id: String, name: Option<String>, arguments_delta: String },
    ToolCall { id: String, name: String, arguments: serde_json::Value },
    Usage { usage: Usage },
    MessageStop { finish_reason: FinishReason },
}
```

第一梯队真实 provider 包括三类，但第一阶段验收只要求 Qoder 跑通：

1. `DeepSeekApiModelClient`。
2. `QoderApiModelClient`。
3. `CodexApiModelClient`。
4. `FakeModelClient`，用于 deterministic eval 和 loop 测试。

这里不要做完整 provider matrix。OpenHands/SWE-agent/CrewAI 借 LiteLLM、goose/Cline/Continue 自研 provider interface 的共同结论是：产品应拥有 facade，但 provider 细节可以先靠现成中间层。第一阶段只把 `QoderApiModelClient` 打磨到可用于 kernel eval；DeepSeek API 和 Codex API 保留为第一梯队后续 provider，不作为第一阶段完成条件。

### Phase 2: Rust `agent-runtime` single-agent kernel

`agent-runtime` 是通用 kernel，不知道 coding、repo、git、patch 或测试。最小 loop：

1. load run state + active capability set。
2. build request messages + stable system/developer instructions。
3. call `ModelClient::stream()`。
4. collect assistant text/tool calls/reasoning/usage。
5. dispatch tools through `tool-runtime`。
6. append tool results。
7. repeat until final answer、budget exhausted、interrupt、approval required、guardrail halt。

必须从第一版就有：

- turn budget 和 max iterations。
- interrupt/cancel。
- event stream。
- tool result size limit。
- model error 分类：retryable、auth、rate_limit、context_length、invalid_request、tool_schema、unknown。
- transcript 与 event log 分离：UI 看 events，replay/eval 看 durable log。
- stable prompt/cache boundary：ephemeral context 不 mutate canonical transcript。

### Phase 3: Rust `tool-runtime` + coding capability pack

`tool-runtime` 是通用工具宿主；`coding` 是第一组 capability。第一版 coding tools 建议只有：

- `read_file`
- `search_files`
- `list_files`
- `patch`
- `run_command`
- `approval_request`

每个工具要定义：

- schema
- idempotent/mutating
- permission level
- workspace boundary
- git worktree safety behavior
- max result size
- timeout
- structured error
- audit fields

不要裸暴露无限 shell。SWE-agent 的 ACI 经验和 Hermes guardrails 都说明：工具设计是 agent 表现的一部分。`run_command` 第一版只在本机 workspace root 内运行，应从第一版就有 cwd、timeout、env policy、approval policy、output truncation 和 cancellation。Docker 与远程 workspace 不进入第一阶段。

### Phase 4: Rust `event-store`、trace、eval

不要等 UI 做完再补观测。第一版就应有 append-only event store：

- `run_started`
- `turn_started`
- `model_event`
- `tool_requested`
- `approval_requested`
- `tool_completed`
- `turn_completed`
- `run_completed`
- `run_failed`

存储可以先 JSONL，不必第一天上 SQLite。关键是每个 event 都有 `run_id`、`turn_id`、`parent_event_id`、timestamp、payload、redaction marker。Hermes 的 SQLite/FTS5 是后续产品化方向，Pi 的 JSONL session 也足够第一阶段验证。

第一阶段 Event Log 是 canonical source，不导入、不导出 OpenAI、Anthropic、Codex、Claude Code 或其他外部 session 格式。外部 session 兼容性以后通过 adapter 做，不能反向定义 kernel event model。

### Phase 5: CLI Proof Surface

第一阶段只实现 Rust CLI Proof Surface，不实现桌面、IDE、web 或 TypeScript consumer。CLI Proof Surface 需要证明这些最小交互：

1. 启动一个 Agent Run。
2. 输入用户目标。
3. 流式展示 AgentEvent。
4. 展示并响应 approval request。
5. 支持 interrupt/cancel。
6. 输出 final outcome 和 event log 路径。

Rust core 与 TypeScript surface 的稳定协议仍是长期方向，但第一阶段只需让 CLI Proof Surface 使用同一套内部事件模型，不要求生成 TypeScript declarations。

### Phase 6: MCP/extensions

第一阶段不实现 MCP Runtime，只保留 MCP Boundary：`ToolDefinition`、`ToolResult`、permission、approval、untrusted-output 字段要足够让未来 MCP tools 映射进 Tool Runtime。等 core tools 稳定后再接 MCP。顺序建议：

1. tool registry 支持 dynamic tools。
2. MCP client adapter 把 MCP tool 映射为内部 `ToolDefinition`。
3. capability/check_fn/gating。
4. tool namespace，避免冲突。
5. approval policy 和 untrusted output framing。

这一步不应抢在 core loop 前面，但接口要在 Phase 2/3 预留。

### Phase 7: memory/skills/subagents

这些都应晚于 event log 和 eval：

- memory：先做 session search，再做 user memory。
- skills：先做 file-based instruction expansion，再做 autonomous skill creation。
- subagents：先作为 tool 调用单个 child run，child context 隔离，parent 只收 summary。
- multi-agent：只有当 eval 显示单 agent 因 instruction/tool overload 明显失败时再引入。

Hermes 的 `delegate_task`、background review、skills 自改进都很有参考价值，但第一版照搬会过重。

## 初始模块边界建议

建议从 Rust workspace + TypeScript packages 开始：

```text
crates/
  model-runtime/
    src/types.rs
    src/model_client.rs
    src/providers/openai_compatible.rs
    src/providers/fake.rs
  agent-runtime/
    src/kernel.rs
    src/loop.rs
    src/events.rs
    src/state.rs
    src/errors.rs
  tool-runtime/
    src/registry.rs
    src/tool.rs
    src/permissions.rs
    src/result.rs
  event-store/
    src/jsonl.rs
    src/replay.rs
  capability-coding/
    src/read_file.rs
    src/search_files.rs
    src/patch.rs
    src/run_command.rs
  agent-cli/
    src/main.rs

packages/
  protocol-types/
  desktop/
  ide-extension/
  web-console/

docs/
  architecture/
  evals/
```

长期通用 agent 的能力应按 capability pack 增长：

```text
capabilities/
  coding/
  research/
  browser/
  desktop-automation/
  personal-memory/
  scheduler/
  messaging/
  data-analysis/
```

核心约束：`agent-runtime` 只依赖通用 `tool-runtime` 和 capability manifest，不直接依赖 `capability-coding` 的实现细节。第一阶段 manifest 使用 TOML，只加载内置 `capability-coding`，不支持用户自定义 capability。

## 第一阶段可执行任务

1. 写 `docs/architecture/agent-kernel-contract.md`：固定 Rust core 的核心类型、事件协议、状态模型。
2. 写 `docs/architecture/capability-pack-contract.md`：定义 TOML capability manifest、instructions、tools、eval seeds 的加载方式。
3. 写 `docs/architecture/cli-proof-surface.md`：定义第一阶段 CLI 交互、approval、interrupt、event log 展示。
4. 建 Rust workspace：`model-runtime`、`agent-runtime`、`tool-runtime`、`event-store`、`capability-coding`、`agent-cli`。
5. 实现 `FakeModelClient` 和 fake tools，先跑 deterministic loop tests。
6. 实现 JSONL event log 和 replay test。
7. 接入最小 coding tools：read/search/patch/run_command。
8. 接入 `QoderApiModelClient`，只做真实 provider integration smoke。
9. 用 Phase 0 的 seed tasks 做第一轮 eval。

第一阶段完成标准：

- 能通过 Rust CLI 跑一个 end-to-end coding task。
- 能保存完整 event log。
- 能 replay 一次 run。
- 能看到每个 model event、tool call、tool result、usage、error。
- 能用 fake model 写 deterministic unit tests。
- CLI Proof Surface 能展示 AgentEvent、处理 approval request、支持 interrupt/cancel。
- `QoderApiModelClient` 通过真实 provider integration smoke，不作为所有测试前提。
- 文件工具和命令执行遵守本机 cwd workspace boundary，并以 git worktree safety 作为第一阶段安全边界。
- `run_command` 默认允许读/验证类低风险命令，写入/破坏性/安装/后台进程/跨 workspace 命令必须 approval。
- `capability-coding` 能通过内置 TOML manifest 加载；第一阶段不支持用户自定义 capability。
- Tool contract 保留 MCP Boundary，但不实现 MCP Runtime。
- Event Log 是第一阶段 canonical source，不支持外部 session import/export。

## 已定决策

1. 第一梯队 provider：DeepSeek API、Qoder API、Codex API。
2. 第一阶段验收只要求 `QoderApiModelClient` 跑通真实 provider smoke。
3. 其他模型厂商和 provider matrix 延后，不作为第一阶段目标。
4. 第一阶段 workspace/sandbox 边界：本机 cwd + git worktree safety；Docker 和远程 workspace 延后。
5. 第一阶段 `run_command` 权限：读/验证类低风险命令默认允许，写入/破坏性/安装/后台进程/跨 workspace 命令必须 approval。
6. 第一阶段 Surface：只做 Rust CLI Proof Surface；TypeScript desktop/IDE/web surface 延后。
7. 第一阶段 Capability Manifest：使用 TOML，只加载内置 `capability-coding`，不支持用户自定义 capability。
8. 第一阶段 MCP：不实现 MCP Runtime，只保留 MCP Boundary。
9. 第一阶段 Session 兼容性：只使用自己的 Canonical Event Log，不导入、不导出 OpenAI、Anthropic、Codex、Claude Code 或其他外部 session 格式。
10. `pi mono` 指 `~/projects/pi`；第一阶段只作为结构参考，不作为实现依赖。

## 待讨论决策

无。

## 当前建议

短期最值得做的不是选一个“大框架”，而是先写出我们自己的 Rust agent kernel contract。只要 `ModelClient`、`AgentEvent`、`ToolDefinition`、`ToolResult`、`RunState`、`CapabilityManifest` 这些边界稳，后面无论接 Pi、OpenAI-compatible、LiteLLM、OpenHands、LangGraph，还是自研 provider，都不会反过来绑死 agent core。

因此下一步我建议进入一个小设计阶段：先把 Rust `model-runtime + agent-runtime + tool-runtime + event-store` 的 public interface 设计两版，按 Hermes/Pi/Codex/goose/OpenHands/Cline/LangGraph 的经验逐项拷打，再决定是在 `young-agent` 直接新建 Rust workspace，还是先 fork/裁剪 Pi 的包结构作为对照实现。
