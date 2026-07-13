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
这里还要区分“检查本身失败”和“已经证明 entry 或 payload 不匹配”：前者最多只能作为
`recovery_candidates` 暴露，包括 xattr/ACL 等 security metadata 无法接受或检查失败；
后者必须标记为 unlocated，不能把并发替换后的路径放进 `recovery_files`。大文件验证也不必
在每个阶段重复读盘：初次读取和 staging 写入可以直接
对内存中的同一份 bytes 计算 digest，提交前仍对原目标做一次完整 digest 复核；rename 前的
其他检查先比较 retained handle 与 slot metadata，rename 后再做完整 payload 验证。
Recovery slot 的复核还要对 entry 做 rename 前后的 `lstat`/opened-handle identity 比较；
`O_NOFOLLOW` 不能被当作唯一证据，symlink 或 `ELOOP` 都是 structural mismatch。
Search 的扫描预算也不等于输出构建预算：path/query metadata 必须先做输入和序列化限长，
结果过大时用对数次 size probe 选择最大的 match prefix，不能每删一个 match 就重新序列化
整份输出。若全局 byte budget 在文件中途耗尽，该文件尚未完成整文件 UTF-8 验证，必须回滚
它已经产生的 provisional matches，同时保留已经消耗的全局扫描预算；逐行扫描状态还应复用
可见内容 buffer，避免短行密集文件为每一行重新分配。

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
unreaped identity 作为 PGID reservation。仅重复固定次数的 `yield + killpg` 不能证明 cleanup
完成：最后一次 signal 之后，并发 fork 仍可能才完成。child 应额外继承一个匿名 ownership
token，正常 fork 会原子继承它；parent 先终止 background 与同组残余成员，只有观察到 token
EOF 才能完成 leader reap 并返回成功，否则把 child、token 与 permit 一起转交 supervisor。
token source 必须由消费型 prepared-command API 持有，spawn 返回前自动从 parent 关闭；
`CommandProcess` 也不能用 `DerefMut` 暴露 raw child，而要用显式的 terminate、observe、seal、
wait 方法维护“token EOF before reap”不变量。token endpoint 可能被后代写入，因此每次观察还
必须有固定 read budget；未见 EOF 就返回 pending，不能为了 drain payload 卡死唯一 worker。
成功提交不应依赖
`/proc` 或 `libproc` 的非原子成员快照；无法提供非回收终态观察的平台应在 spawn 前拒绝。
Linux 额外用 `no_new_privs` 阻止后代通过 exec 获得新的 signal 权限；macOS 没有等价的
portable primitive，因此 manifest 与 output 只能承诺向仍可 signal 的同组成员请求终止，
不能把 credential-changing descendant，或主动关闭 tracking descriptor 的后代，描述为已验证回收。
即使 wrapper 与 direct group kill 同时失败，也不能在 leader 仍存活时直接 drop child handle：
应把 ownership 交给单一的进程级 supervisor registry/event loop，做有界 termination retry，
之后继续持有并最终 reap；不能为每条失败命令创建一个可能永久存活的 detached thread。
supervisor 必须在 child spawn 前通过可重试的启动 preflight，启动失败时不创建命令；handoff
仍需保留 ownership，避免 caller 因同步兜底而无限阻塞，并让后续 preflight 能恢复 worker。
preflight 还要预留有界 supervision slot；permit 跟随 foreground command，并在 handoff 时转交
registry，确保 child 启动后不会因容量已满而失去接管路径。deadline 应通过 heap 调度到期项，
避免 staggered commands 反复全量扫描和搬移。worker 从 heap 取出的单条 command 必须放在
in-flight RAII guard 中，任何 panic 都自动把同一 ownership 重新入队。若同一 command 持续
触发内部 panic，原 deadline 直接重入会长期霸占 heap 顶部；因此要按 command 记录 panic、
指数退避并让其他到期项先运行。admission health 与 worker lifecycle 必须是正交状态：达到
阈值后把 admission 粘性标记为 degraded、fail closed 地拒绝新命令，但 cleanup worker 仍可在
退出后重启并继续清理已经接管的 ownership。
测试不能用固定 sleep 猜测这条异步路径已经完成：先用 non-reaping 观察确认 fixture leader
terminal，再等待对应 registry entry 的 completion barrier，才能证明 supervisor 已 seal 并 reap。
只有 `wait` 确认 reap 成功才能触发 completion；reap error 必须保留 entry 并重试。
fail-closed 的非 Unix 路径也必须能通过编译，
因此 Unix-only snapshot protocol 在其他平台需要可解析的 opaque token，即使调用总是返回
`Unsupported`。
cleanup 与 supervisor 也必须用 `waitid(..., WNOWAIT)` 观察 leader 终态；即使一次 group kill
返回成功，也要先确认 leader terminal，并在保留 PGID reservation 时等待 inherited token
关闭，最后才能 reap。否则 macOS 的 partial signal success、leader 先退出竞态，或终态观察
附近仍在完成的 fork 都可能泄漏同组成员。
只要 stdout/stderr 都没有 I/O progress，状态轮询就应指数退避到有限上限，避免仍保持 pipe
打开的长时间静默命令固定频率唤醒；一旦任一 pipe 又有进展就恢复低延迟轮询。
read-only 与 mutating command 的细粒度分类属于 Issue #8。

## 下次怎么做

- 所有新文件工具都复用 `CodingWorkspace` 的 directory handle，不自行使用 ambient path。
- lexical 检查负责尽早给出清晰错误；capability-relative 操作才是最终权限边界。
- 用同目录临时文件与原子 `rename` 提交修改，不让多文件 patch 产生部分成功语义。
- Git worktree probe 清除 `GIT_DIR`、`GIT_WORK_TREE` 等 inherited repository 环境；
  只有确认的 “not a repository” 才能映射为无 worktree，其余错误必须显式上报。
- 不把 workspace 或 git worktree 语义下沉到通用 `young-tool-runtime`。
- 不把 `current_dir` 描述成 shell isolation；需要更强隔离时新增独立执行边界。
