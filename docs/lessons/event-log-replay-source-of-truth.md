# Event Log 是 Replay 的唯一真相

## 背景

第一版 Canonical Event Log 需要同时服务调试、测试、未来 Surface 和确定性
Replay。它不能只把事件写进文件，还要保证读回后的状态没有脱离原始事件，
也不能替尚未存在的 runtime contracts 猜测语义。

## 经验

JSONL 的物理行顺序就是第一版 timeline 顺序。写入前应先把一个完整
`AgentEvent` 序列化到内存，再一次追加记录和换行；读取失败时保留文件路径、
1-based 行号和底层错误，才能快速定位 malformed、truncated 或 unsupported
record。终止换行同时是第一版的 record commit marker：即使尾部 JSON 在语法上
完整，没有换行也仍视为 truncated；后续 append 必须拒绝这种日志，不能通过补
换行把可能未完成的写入升级为 canonical event。

Runtime 写出的每个 `AgentEvent` 还需要正式的 `event_sequence` wire 字段。物理
行顺序决定 timeline，而 sequence 让持久化错误发生在 commit 前还是 commit 后都
能幂等协调，即使相邻事件 payload 完全相同。它必须属于公开 `AgentEvent`
contract，不能由 Store 私下塞入一个公开 decoder 会拒绝的未知字段。Store 在同一
文件锁临界区内先校验 expected sequence 再 append；旧的全量 unsequenced 日志仍可
读取，但不能和 sequenced runtime 日志混写。

Replay model 应保留 ordered events 作为 canonical truth，只在其上派生 run
status、tool call、approval request、error 和 terminal result。当前 contracts 中：

- 只有 `RunFinished` 能决定 terminal status；`Error` 即使不可恢复，也不能代替
  terminal event。
- `ApprovalRequested` 只能证明 run 正在等待；在正式的 approval decision event
  落地前，Replay 不能声称恢复了批准或拒绝结果。
- `ToolResult` 已经证明该调用完成，因此它后面的 `ApprovalRequested` 是损坏的
  lifecycle，而不是新的等待状态。
- `TurnStarted` 会切换唯一 active turn；新 turn 不能越过未完成的 tool call，且
  后续 model、tool、approval、error 事件不能回流到已失活的旧 turn。
- model-owned `ModelToolCallId` 和 kernel-owned `ToolCallId` 必须同时保留；
  `ToolResult` 只能通过后者关联具体执行。

这种做法让派生状态可查询，同时避免再创造一套会和 Event Log 漂移的 session
格式。

## 下次怎么做

扩展 Replay 时先问：这个状态能否由一个明确的 `AgentEvent` 证明？如果不能，
应先补 contract，而不是在 Replay 里推断。

实现 Agent Runtime 与 Event Store 集成时还要避免 Cargo 依赖环：
`young-event-store` 已经依赖 `young-agent-runtime` 的 `AgentEvent`。事件写入 port
应由 consumer 一侧定义，再由 Event Store 实现并在 Surface 组装；不要让
`young-agent-runtime` 反向依赖具体 JSONL store。
