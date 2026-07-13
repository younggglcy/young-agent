# Workspace Boundary 需要两层路径检查

## 背景

Issue #7 为 Coding Capability 加入本地 workspace、git worktree context，以及
`read_file`、`search_files`、`apply_patch`、`run_command`。只把进程 cwd 设为
workspace 并不能形成文件边界：`..`、absolute path 和 symlink 都可能把实际访问
带到根目录之外。

## 经验

文件工具需要同时做 lexical 与 canonical 两层检查。lexical 检查先归一化 `.` 和
`..`，因此即使目标不存在，也能拒绝明显的 traversal；canonical 检查再解析实际
存在的路径，阻止 symlink escape。新建文件没有可 canonicalize 的 leaf，所以要
canonicalize 它最近的现存 parent，再确认 parent 仍位于 workspace 内。

用户选定的 workspace root 是权限边界；所属 git worktree 是上下文，不应反过来
扩大权限。worktree root、per-worktree git dir 和 common git dir 可以记录到工具
metadata，供 Agent Runtime、Event Log 和后续 Surface 展示。

命令工具的 cwd 同样不等于 shell sandbox。Issue #7 只固定 cwd、限制输出并默认
要求审批；read-only 与 mutating command 的细粒度分类属于 Issue #8。

## 下次怎么做

- 所有新文件工具都复用 `CodingWorkspace` 的路径解析，不自行拼接路径。
- 写入不存在的目标时检查现存 parent；写入现存 symlink 时检查 resolved target。
- 不把 workspace 或 git worktree 语义下沉到通用 `young-tool-runtime`。
- 不把 `current_dir` 描述成 shell isolation；需要更强隔离时新增独立执行边界。
