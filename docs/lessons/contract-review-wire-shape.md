# Contract Review 先收紧 Wire Shape

## 背景

Issue 3 引入了 model、tool 和 agent run event 的第一批持久化 contract。
review 反馈里混在一起的，既有具体的 wire shape 风险，也有更长远的
provider 演进设想。

## 经验

做早期协议设计时，优先处理能减少持久化数据歧义的反馈，但不要提前猜未来
provider 的复杂行为。这次 object-shaped metadata 和单一 terminal run status
值得立即修，因为它们能避免不同 consumer 对同一份日志产生不同解释。多模态
message parts、更丰富的 stream deltas、provider-specific lifecycle states 这类
更大的表达能力，应该等真实 adapter 带来实现压力之后再设计。

如果一个字段组合本身就有语义约束，优先把约束编码进类型形态和 round-trip
测试里，而不是只靠注释提醒 producer。例如 tool result message 必须携带工具名
和 tool call id，普通 model message 则不应该承载这两个字段。

## 下次怎么做

review contract PR 时，先把评论分成三类：

- 现在就该修的 wire-shape ambiguity；
- 现在就该写清楚的 semantic rules；
- 等真实 consumer 需要时再扩展的 future expressiveness。
