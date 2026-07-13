# Workspace Boundary 必须约束实际文件操作

## 背景

Issue #7 为 Coding Capability 加入本地 workspace、git worktree context，以及
`read_file`、`search_files`、`apply_patch`、`run_command`。只把进程 cwd 设为
workspace 并不能形成文件边界：`..`、absolute path 和 symlink 都可能把实际访问
带到根目录之外。

## 经验

路径校验本身不能构成完整的文件边界。lexical 检查仍应先拒绝 workspace 外的
absolute path 与逃逸 workspace 的 `..`；但先 `canonicalize`、再用 ambient path 打开文件会留下
check-then-use 竞态：校验后如果某级目录被替换成 symlink，真正的读写仍可能越界。

文件工具应在启动时打开 workspace directory handle，之后通过 capability-relative
API 完成 `open`、遍历、创建、删除与 `rename`。这样边界会在每次实际文件操作时生效，
而不是只在前置检查时成立。现存文件的修改还应先写同目录临时文件，再以 capability
内的 `rename` 原子替换，避免失败时留下半写状态。原子替换会改变 inode，因此遇到
hard link、extended ACL、xattr 或无法保留的 owner/group 时应明确拒绝，而不是静默
丢失安全 metadata；新文件则用平台的 no-replace rename 提交，避免并发创建竞态。
失败输出还要把 publication state 与 recovery evidence 分开：只有 recovery namespace
identity/policy、entry identity 和 payload snapshot 在 move 后复核通过，才能标记为
`LocatedVerified`；任一证据缺失都应降级为 content/policy unverified 或 unlocated。
这里还要区分“检查本身失败”和“已经证明 entry 不匹配”：前者最多只能作为
`recovery_candidates` 暴露，后者必须标记为 unlocated，不能把并发替换后的路径放进
`recovery_files`。大文件验证也不必在每个阶段重复读盘：初次读取和 staging 写入可以直接
对内存中的同一份 bytes 计算 digest，提交前仍对原目标做一次完整 digest 复核；rename 前的
其他检查先比较 retained handle 与 slot metadata，rename 后再做完整 payload 验证。

用户选定的 workspace root 是权限边界；所属 git worktree 是上下文，不应反过来
扩大权限。worktree root、per-worktree git dir 和 common git dir 可以记录到工具
metadata，供 Agent Runtime、Event Log 和后续 Surface 展示。

命令工具的 cwd 同样不等于 shell sandbox，而且不能先比较 inode 再用 ambient path
启动进程，否则仍有 check-then-use 窗口。Unix child 在 `pre_exec` 中只执行
async-signal-safe 的 `fchdir`，直接绑定已打开的 workspace handle；无法提供等价语义的
平台应安全拒绝。Issue #7 还限制完整序列化输出、把取消传播到整个进程组，并让 pipe
reader 可停止回收。进程组 leader 退出后不能先 reap 再继续用它的 PID 作为 PGID：
该数值可能被复用，后续探测或取消会等待、甚至终止无关进程。macOS/Linux 实现应先用
`waitid(..., WNOWAIT)` 观察终态。用户 shell 源码保持原样；leader 终态后仍保留其
unreaped identity 作为 PGID reservation，给已经开始的 fork 让出有限调度机会后，先终止
background 与同组残余成员，再完成 leader reap。成功提交不应依赖
`/proc` 或 `libproc` 的非原子成员快照；无法提供非回收终态观察的平台应在 spawn 前拒绝。
Linux 额外用 `no_new_privs` 阻止后代通过 exec 获得新的 signal 权限；macOS 没有等价的
portable primitive，因此 manifest 与 output 只能承诺向仍可 signal 的同组成员请求终止，
不能把 credential-changing descendant 描述为已验证回收。
当 stdout/stderr 已经 EOF、但 leader 仍在运行时，状态轮询应指数退避到有限上限，避免
长时间静默命令每毫秒唤醒一次；一旦 pipe 又有进展就恢复低延迟轮询。
read-only 与 mutating command 的细粒度分类属于 Issue #8。

## 下次怎么做

- 所有新文件工具都复用 `CodingWorkspace` 的 directory handle，不自行使用 ambient path。
- lexical 检查负责尽早给出清晰错误；capability-relative 操作才是最终权限边界。
- 用同目录临时文件与原子 `rename` 提交修改，不让多文件 patch 产生部分成功语义。
- Git worktree probe 清除 `GIT_DIR`、`GIT_WORK_TREE` 等 inherited repository 环境；
  只有确认的 “not a repository” 才能映射为无 worktree，其余错误必须显式上报。
- 不把 workspace 或 git worktree 语义下沉到通用 `young-tool-runtime`。
- 不把 `current_dir` 描述成 shell isolation；需要更强隔离时新增独立执行边界。
