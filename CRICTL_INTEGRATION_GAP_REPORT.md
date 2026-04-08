# Crius 对比 CRI-O 的 `crictl`/CRI 差距报告

## 1. 范围、基线与判定口径

- 分析日期: `2026-04-08`
- 对比对象:
  - Crius: 当前工作区 `/root/crius`
  - CRI-O: 本机源码树 `/root/cri-o`
- 结论依据: 只按代码当前实现判定，不按 TODO、注释或预期设计判定
- 本报告关注的是“通过 `crictl` 使用时的真实 CRI 能力差距”，不是泛化的容器运行时设计评审

判定标签:

- `落后`: CRI-O 已实现明确能力，Crius 未实现或契约明显不完整
- `部分落后`: 主链路可用，但状态、过滤、恢复、错误语义或边界条件弱于 CRI-O
- `基本对齐`: 当前行为已接近 CRI-O
- `不计入差距`: CRI-O 也基本是占位，不应作为对 Crius 的额外短板

本次主要复核文件:

- Crius:
  - `src/server/mod.rs`
  - `src/image/mod.rs`
  - `src/runtime/mod.rs`
  - `src/streaming/mod.rs`
  - `src/runtime/shim_manager.rs`
  - `src/shim/io.rs`
- CRI-O:
  - `server/version.go`
  - `server/runtime_status.go`
  - `server/runtime_config.go`
  - `server/sandbox_*.go`
  - `server/container_*.go`
  - `server/image_*.go`
  - `server/server.go`

## 2. 总结结论

Crius 已经不再是“只有 CRI 接口骨架”的状态。按 `crictl` 主路径看，下面这些能力已经有可用基础:

- Pod/Container 基础生命周期: `runp`、`stopp`、`rmp`、`create`、`start`、`stop`、`rm`
- 基础查询: `pods`、`inspectp`、`ps`、`inspect`
- 流式主路径: `exec --sync`、`exec`、`attach`
- 镜像最小闭环: `pull`、`images`、`inspecti`、`rmi`

但如果基线是 CRI-O 这类“可以长期稳定被 `crictl` 当成真实 CRI runtime 使用”的实现，Crius 仍有 8 组关键差距:

1. `port-forward` 仍未真正接通
2. `stats` / `statsp` / `metricsp` / `events` 基本为空或只有快照
3. `stop/remove` 容器、Pod、镜像缺少 CRI 规定的幂等语义
4. `inspect` / `inspectp` / `info` 的状态新鲜度、verbose 信息、reason/message 等弱于 CRI-O
5. `ReopenContainerLog`、`UpdateContainerResources`、`CheckpointContainer` 仍是占位
6. 镜像管理仍是最小目录模型，过滤、not-found 契约、repo digests、fs usage、in-use 检查明显弱于 CRI-O
7. daemon 重启后的恢复只恢复“数据库快照”，没有 CRI-O 那套启动期校验、exit monitor、事件广播和网络清理
8. streaming 只做了 `exec/attach` 的基础 SPDY 数据面，`resize`、`port-forward`、runtime 接管路径都还没补齐

一句话判断:

> Crius 当前已经能支撑“基础调试型 `crictl` 使用”，但距离 CRI-O 级别的契约完整度、观测能力和恢复语义，还有一段明显距离。

## 3. 逐项差距

### 3.1 Runtime 身份与状态

#### `Version` / `crictl version`

- Crius:
  - `src/server/mod.rs:1061`
  - 直接返回静态值: `version=0.1.0`、`runtime_name=crius`、`runtime_version=0.1.0`、`runtime_api_version=v1`
- CRI-O:
  - `server/version.go:22`
  - 从版本模块读取实际构建版本，再填充 `RuntimeVersion`
- 结论:
  - `部分落后`
- 影响:
  - 主命令可用，但无法像 CRI-O 一样报告真实构建版本

#### `Status` / `crictl info` 的核心来源之一

- Crius:
  - `src/server/mod.rs:1755`
  - `RuntimeReady` 和 `NetworkReady` 都是硬编码 `true`
  - 无 `RuntimeHandlers`
  - 无 `RuntimeFeatures`
  - 忽略 `verbose` 粒度，直接返回一个简单 `info` map
- CRI-O:
  - `server/runtime_status.go:15`
  - 真实检查 CNI readiness
  - 返回 `RuntimeHandlers`
  - 返回 `RuntimeFeatures`
  - `verbose` 时返回配置 JSON
- 结论:
  - `落后`
- 影响:
  - `crictl info` 能出结果，但健康度、runtime handler 能力、cgroup/feature 能力都不可信

#### `RuntimeConfig`

- Crius:
  - `src/server/mod.rs:2971`
  - 返回空的 `LinuxRuntimeConfiguration`
- CRI-O:
  - `server/runtime_config.go:10`
  - 返回真实 `CgroupDriver`
- 结论:
  - `落后`
- 影响:
  - 上层无法通过 CRI 正确探测当前 runtime 的 cgroup driver

#### `UpdateRuntimeConfig`

- Crius:
  - `src/server/mod.rs:2864`
  - 空响应
- CRI-O:
  - `server/update_runtime_config.go:9`
  - 也是空响应
- 结论:
  - `不计入对 CRI-O 的差距`

### 3.2 Pod Sandbox 相关

#### `RunPodSandbox` / `crictl runp`

- Crius:
  - `src/server/mod.rs:1074`
  - 已打通 Pod metadata、annotations、runtime handler、pause 容器、状态持久化
- CRI-O:
  - `server/sandbox_run.go:68`
  - 配合 `server/server.go:200-340`
  - 启动期恢复、失败回滚、网络清理、IP 恢复、事件生成都更完整
- 结论:
  - `部分落后`
- 主要差距:
  - 启动失败回滚和脏资源清理体系明显弱于 CRI-O
  - 没有 CRI-O 那套启动期网络 GC 和恢复校验
  - 没有配套事件生成

#### `StopPodSandbox` / `crictl stopp`

- Crius:
  - `src/server/mod.rs:1412`
  - 会先停 Pod 内业务容器，再更新 Pod 状态
  - 若 Pod 不存在，直接 `NotFound`
- CRI-O:
  - `server/sandbox_stop.go:16`
  - 缺失 Pod 时按 CRI 语义返回空响应而非报错
  - 停止后还会联动事件路径
- 结论:
  - `落后`
- 影响:
  - 重复执行 `stopp` 时，Crius 的行为不符合 CRI-O 的幂等语义

#### `RemovePodSandbox` / `crictl rmp`

- Crius:
  - `src/server/mod.rs:1791`
  - 能级联删容器、删持久化、best-effort 清理 netns
  - 若 Pod 不存在，直接 `NotFound`
- CRI-O:
  - `server/sandbox_remove.go:16`
  - 缺失 Pod 时返回空响应
  - 删除流程包含 SHM、网络、namespace、index、event 等完整清理
- 结论:
  - `落后`
- 影响:
  - `rmp` 的运维语义和 CRI-O 不一致，重复清理不够安全

#### `PodSandboxStatus` / `crictl inspectp`

- Crius:
  - `src/server/mod.rs:1889`
  - 已能返回 `network.ip`、`additional_ips`、`runtime_handler`、`containers_statuses`
  - `verbose` 信息是自定义 key/value，不是标准 `"info"` JSON blob
- CRI-O:
  - `server/sandbox_status.go:19`
  - `verbose` 返回统一 JSON 结构
  - 事件模式下可附带时间戳和容器状态快照
- 结论:
  - `部分落后`
- 主要差距:
  - verbose 信息组织方式不兼容 CRI-O 习惯
  - 状态新鲜度仍依赖内存 + best-effort runtime 查询

#### `ListPodSandbox` / `crictl pods`

- Crius:
  - `src/server/mod.rs:2023`
  - 支持 id/state/label 基本过滤
  - 数据来源主要是内存快照
- CRI-O:
  - `server/sandbox_list.go:14`
  - 基于 sandbox 对象和索引过滤
  - 与启动恢复流程深度联动
- 结论:
  - `部分落后`

### 3.3 Container 生命周期与查询

#### `CreateContainer` / `crictl create`

- Crius:
  - `src/server/mod.rs:2080`
  - `src/runtime/mod.rs:697`
  - 已支持基础 OCI spec 组装、namespace path 注入、resources/devices/log path
- CRI-O:
  - `server/container_create.go:393`
  - `server/container_create_linux.go`
  - create 路径很大，覆盖挂载传播、镜像卷、relabel、NRI、更多 runtime 集成
- 结论:
  - `部分落后`
- 说明:
  - 主链路能用，但离 CRI-O 的 create 细节完整度还有较大距离
  - 这里不枚举所有 OCI spec 细项，只记录目前最明显、最影响 `crictl` 的差距

#### `StartContainer` / `crictl start`

- Crius:
  - `src/server/mod.rs:2463`
  - `src/runtime/mod.rs:960`
  - 启动后轮询 runtime，等待到“某个已知状态”
  - 容器已 running 时也可能被视作成功
  - 无 hook/NRI/event/failure-reason 持久化
- CRI-O:
  - `server/container_start.go:18`
  - 严格要求 `created` 状态
  - 启动失败会写入后续 `ContainerStatus.Reason/Message` 所需状态
  - 启动成功后生成事件
- 结论:
  - `落后`

#### `StopContainer` / `crictl stop`

- Crius:
  - `src/server/mod.rs:2607`
  - 不存在时返回 `NotFound`
  - 无 post-stop cleanup/event 体系
- CRI-O:
  - `server/container_stop.go:20`
  - 明确按 CRI 约定支持幂等
  - 有 post-stop cleanup、状态落盘和 hook/NRI 联动
- 结论:
  - `落后`

#### `RemoveContainer` / `crictl rm`

- Crius:
  - `src/server/mod.rs:2728`
  - 不存在时返回 `NotFound`
  - 删除逻辑基本是 stop + runc delete + 内存/DB 删除
- CRI-O:
  - `server/container_remove.go:23`
  - 明确幂等
  - 清理 exit file、storage、index、seccomp notifier，并生成删除事件
- 结论:
  - `落后`

#### `ListContainers` / `crictl ps`

- Crius:
  - `src/server/mod.rs:1580`
  - `src/server/mod.rs:666`
  - 过滤支持 `id/state/pod_sandbox_id/labels`
  - 但列表状态主要来自内存，不会像 CRI-O 那样在体系内持续刷新
- CRI-O:
  - `server/container_list.go:73`
  - 用短 ID 索引和内部容器对象过滤
  - 跳过尚未 created 的对象
- 结论:
  - `部分落后`

#### `ContainerStatus` / `crictl inspect`

- Crius:
  - `src/server/mod.rs:1534`
  - `src/server/mod.rs:522`
  - 只返回基础状态、时间戳、exit code、image、labels、annotations、log path、resources
  - 缺 `reason`、`message`、`mounts`、`user`、`verbose info`
  - 没有 CRI-O 那种“如果 stopped 但 exit code 未补齐，再主动刷新一次状态”的逻辑
- CRI-O:
  - `server/container_status.go:27`
  - 会填 `Mounts`、`Resources`、`User`、`Reason`、`Message`
  - verbose 时返回 JSON 信息
- 结论:
  - `落后`

#### `UpdateContainerResources` / `crictl update`

- Crius:
  - `src/server/mod.rs:2988`
  - 只打印日志，TODO 里明确还没真正调用 `runc update`
- CRI-O:
  - `server/container_update_resources.go:19`
  - 会校验状态、转换 OCI resources、调用 runtime 更新，并更新内存状态
- 结论:
  - `落后`

### 3.4 Streaming、日志与远程调试

#### `ExecSync` / `crictl exec --sync`

- Crius:
  - `src/server/mod.rs:1660`
  - `src/runtime/mod.rs:887`
  - 能拿到真实 `stdout/stderr/exit_code`
  - 但不先校验容器是否处于可 exec 状态
  - 错误处理和输出限制比 CRI-O 简化
- CRI-O:
  - `server/container_execsync.go:15`
  - 先检查 `c.Living()`
  - 由 runtime 层统一处理 exec sync 细节
- 结论:
  - `部分落后`

#### `Exec` / `crictl exec`

- Crius:
  - `src/server/mod.rs:1651`
  - `src/streaming/mod.rs:339`
  - 已实现基础 SPDY 升级、stream 协商、TTY PTY 桥接
  - 不支持 resize channel
  - 不支持 runtime/websocket 接管路径
  - URL 生成阶段只校验 container id，不校验容器是否 running/created
- CRI-O:
  - `server/container_exec.go:17`
  - `server/container_exec.go:46`
  - stream service 会校验容器是否可执行
  - 支持 resize channel
  - 可按 runtime handler 走 websocket/runtime monitor 路径
- 结论:
  - `部分落后`

#### `Attach` / `crictl attach`

- Crius:
  - `src/server/mod.rs:2796`
  - `src/streaming/mod.rs:757`
  - `src/shim/io.rs:141`
  - 依赖 live `attach.sock`
  - stdout/stderr framing 已有
  - 无 resize
  - 无 restart 后 attach 恢复
  - 无容器状态校验
- CRI-O:
  - `server/container_attach.go:19`
  - `server/container_attach.go:48`
  - 先更新并校验容器状态
  - 支持 resize channel
  - 可走 runtime/websocket 路径
- 结论:
  - `落后`

#### `PortForward` / `crictl port-forward`

- Crius:
  - `src/server/mod.rs:1744`
  - RPC 只返回固定 URL: `unix:///var/run/crius/crius.sock`
  - `src/streaming/mod.rs` 只有 `get_exec` / `get_attach`，根本没有 `get_port_forward` 或真正的数据面实现
- CRI-O:
  - `server/container_portforward.go:16`
  - `server/container_portforward.go:25`
  - 先准备 endpoint，再把流量真正转发进 Pod netns
- 结论:
  - `落后`
- 级别:
  - `严重`

#### `ReopenContainerLog`

- Crius:
  - `src/server/mod.rs:2787`
  - 空实现，直接成功
- CRI-O:
  - `server/container_reopen_log.go:14`
  - 校验容器 running，然后调用 runtime reopen
- 结论:
  - `落后`

#### `logs`

- Crius:
  - `src/shim/io.rs:171`
  - `src/shim/io.rs:185`
  - stdout/stderr 已能写文件
  - 但日志 reopen/rotation 契约没补齐
- CRI-O:
  - 通过 `ReopenContainerLog` 和 runtime 实现完整契约
- 结论:
  - `部分落后`

### 3.5 镜像服务

#### `ListImages` / `crictl images`

- Crius:
  - `src/image/mod.rs:520`
  - 直接返回内存里的所有镜像
  - 完全忽略 `ListImagesRequest.filter`
- CRI-O:
  - `server/image_list.go:15`
  - 支持按单个 image filter 查询
  - 同时能列 storage image 和 artifact
- 结论:
  - `落后`

#### `ImageStatus` / `crictl inspecti`

- Crius:
  - `src/image/mod.rs:537`
  - 查不到镜像时返回 gRPC `NotFound`
  - 不处理 `verbose`
- CRI-O:
  - `server/image_status.go:22`
  - 查不到时返回空的 `ImageStatusResponse{}`
  - `verbose` 时返回 OCI config 等信息
- 结论:
  - `落后`
- 这是一个高优先级契约差距:
  - not-found 行为与 CRI-O 不一致

#### `PullImage` / `crictl pull`

- Crius:
  - `src/image/mod.rs:571`
  - `src/image/mod.rs:289`
  - 支持匿名/basic auth 和 registry API fallback
  - 但没有并发 pull 去重
  - 忽略 `auth.auth` 编码字段，只读 `username/password`
  - 不做 namespaced auth/signature policy
  - 不做短名候选解析
  - 通过 OCI 库成功时，会把 digest 缩成 `sha256:<12位>` 形式写成 image id
- CRI-O:
  - `server/image_pull.go:31`
  - `server/image_pull.go:137`
  - 有 pull 去重
  - 支持 `auth.auth` 解码
  - 支持 namespaced auth / signature policy / short-name candidates
  - 返回真实 image ID
- 结论:
  - `落后`

#### `RemoveImage` / `crictl rmi`

- Crius:
  - `src/image/mod.rs:709`
  - 查不到镜像时返回 `NotFound`
  - 删除逻辑就是删内存映射和镜像目录
  - 没有 in-use 检查
- CRI-O:
  - `server/image_remove.go:17`
  - 查不到镜像时按 CRI 语义返回成功
  - 会做 untag / in-use 检查 / artifact 处理
- 结论:
  - `落后`

#### `ImageFsInfo`

- Crius:
  - `src/image/mod.rs:769`
  - 返回空数组
- CRI-O:
  - `server/image_fs_info.go:57`
  - 返回 `FilesystemUsage`、timestamp、mountpoint、bytes、inodes
- 结论:
  - `落后`

#### 镜像元数据模型

- Crius:
  - `src/image/mod.rs:24`
  - 本地元数据只持久化 `id/repo_tags/size`
- CRI-O:
  - `server/image_list.go`
  - `server/image_status.go`
  - 会返回 `repo_digests`、`uid/username`、`pinned`、`spec.annotations` 等
- 结论:
  - `落后`
- 影响:
  - `crictl images` / `inspecti` 输出的信息量明显少于 CRI-O

### 3.6 Stats、Events、Metrics

#### `ContainerStats` / `ListContainerStats`

- Crius:
  - `src/server/mod.rs:2808`
  - `src/server/mod.rs:2825`
  - 分别返回 `None` 和空数组
- CRI-O:
  - `server/container_stats.go:14`
  - `server/container_stats_list.go:12`
  - 返回真实 stats，并支持 filter
- 结论:
  - `落后`
- 级别:
  - `严重`

#### `PodSandboxStats` / `ListPodSandboxStats`

- Crius:
  - `src/server/mod.rs:2836`
  - `src/server/mod.rs:2853`
  - 分别返回 `None` 和空数组
- CRI-O:
  - `server/sandbox_stats.go:12`
  - `server/sandbox_stats_list.go:10`
  - 返回真实 pod stats，并支持 filter
- 结论:
  - `落后`
- 级别:
  - `严重`

#### `GetContainerEvents` / `crictl events`

- Crius:
  - `src/server/mod.rs:2884`
  - 启动后只把“当前容器/Pod 快照”发一遍，然后流自然结束
  - 还直接手写数值 `0`、`2` 作为事件类型，而不是统一走 CRI enum 常量
- CRI-O:
  - `server/container_events.go:29`
  - `server/server.go:979`
  - 维护持续广播流
  - create/start/stop/delete 都会生成事件
- 结论:
  - `落后`
- 级别:
  - `严重`

#### `ListMetricDescriptors` / `ListPodSandboxMetrics`

- Crius:
  - `src/server/mod.rs:2949`
  - `src/server/mod.rs:2960`
  - 都返回空集合
- CRI-O:
  - `server/metric_descriptors_list.go:10`
  - `server/sandbox_metrics_list.go:10`
  - 能返回 descriptor 和 pod metrics
- 结论:
  - `落后`

### 3.7 Checkpoint 与可恢复性

#### `CheckpointContainer`

- Crius:
  - `src/server/mod.rs:2873`
  - 空实现，直接成功
- CRI-O:
  - `server/container_checkpoint.go:17`
  - 在启用 checkpoint/restore 时能真正执行 checkpoint
- 结论:
  - `落后`

## 4. 关键基础设施差距

这一节不是单个 RPC 的差异，而是“为什么 Crius 用 `crictl` 时会比 CRI-O 更脆弱”。

### 4.1 没有 CRI-O 那种 exit monitor + 事件生成链路

- Crius:
  - `src/server/mod.rs:805` 只有 `recover_state()`，负责把 DB 内容恢复进内存
  - 没有类似 CRI-O 的 exit file watcher
  - 没有统一的 create/start/stop/delete 事件生成
- CRI-O:
  - `server/server.go:826` `StartExitMonitor`
  - `server/server.go:979` `generateCRIEvent`
- 直接后果:
  - `ps` / `inspect` / `events` 的状态新鲜度和连续性都弱于 CRI-O

### 4.2 daemon 重启后的恢复只恢复“快照”，不恢复 live runtime 语义

- Crius:
  - `src/server/mod.rs:805`
  - 能恢复 Pod/Container 基本记录
  - 但不会恢复 shim 进程登记、attach 能力、事件流上下文
- CRI-O:
  - `server/server.go:200-340`
  - 启动时会恢复 sandbox/container，对异常对象做清理，还会做网络 GC 和 IP 恢复
- 直接后果:
  - Crius 重启后，`attach`、实时状态、事件面会比 CRI-O 更容易失真

### 4.3 shim 管理是纯内存模型

- Crius:
  - `src/runtime/shim_manager.rs:74` `start_shim`
  - `src/runtime/shim_manager.rs:142` `get_exit_code`
  - `src/runtime/shim_manager.rs:197` `is_shim_running`
  - shim 进程表保存在内存里，daemon 重启后无法重建
- CRI-O:
  - 不依赖这样一份“只在内存里存在的 shim 注册表”来维持 CRI 可见状态
- 直接后果:
  - daemon 重启后，Crius 的 attach/exit-code/live-state 语义明显弱

### 4.4 image store 与 runtime 侧耦合方式脆弱

- Crius:
  - image service 用 `root_dir.join("storage")`
  - 但 runtime 解析镜像目录时硬编码 `src/runtime/mod.rs:483` 为 `/var/lib/crius/storage/images`
- CRI-O:
  - 通过统一 storage/image server 抽象访问镜像
- 直接后果:
  - 一旦不是默认 root dir，Crius 的 image/runtime 协同就可能偏离

## 5. 不应误判为“对 CRI-O 的差距”的项

下面这些项在 Crius 中还不完整，但不能简单算成“比 CRI-O 差很多”:

- `UpdateRuntimeConfig`
  - Crius: `src/server/mod.rs:2864`
  - CRI-O: `server/update_runtime_config.go:9`
  - 两边都基本是空响应
- `Version.version` 字段本身
  - 两边都返回 `0.1.0`
  - 真正差距主要在 `runtime_version` 是否为真实构建版本

## 6. 详细实施计划（WBS）

这一节不再只给优先级，而是给出可以直接开工的细化计划。推荐按下面顺序执行，因为前面的里程碑会给后面的工作提供测试基线、状态模型和通用工具。

### 6.1 执行顺序总览

1. `M0` 建立契约测试基线
2. `M1` 修正低成本高收益的 CRI 契约问题
3. `M2` 实现真正可用的 `port-forward`
4. `M3` 补齐 `stats` / `statsp` / `metricsp`
5. `M4` 把 `events` 从一次性快照改成持续事件流
6. `M5` 补齐 `logs` / `update` / `status` / `runtime-config`
7. `M6` 补强 `exec` / `attach` 契约
8. `M7` 强化恢复语义与状态对账
9. `M8` 完善镜像服务

### 6.2 交付要求

- 每个里程碑必须同时包含:
  - 代码改动
  - 单元测试
  - 集成测试
  - 至少一条真实 `crictl` 验收命令
- 每完成一个里程碑，都要同步更新本报告里的“差距结论”和“最终判断”
- 任何 RPC 契约变更都必须补“存在对象”和“不存在对象”两类测试
- 任何 streaming 改动都必须补“正常结束”“客户端断开”“runtime 异常”三类测试

### 6.3 `M0` 契约测试基线

- 目标:
  - 先把当前行为固定下来，避免后续改动把已有能力回归掉
- 主要触点:
  - `tests/integration_tests.rs`
  - `tests/integration_test.rs`
  - 建议新增 `tests/crictl_smoke.rs`
  - 建议新增 `tests/fixtures/`
- 前置依赖:
  - 无
- 任务拆分:
  - `M0-1` 抽出 daemon 启动辅助。
    完成标准: 能在临时 `root_dir`、临时 socket、临时 streaming 端口下启动 Crius，并等待 gRPC ready。
  - `M0-2` 抽出 `crictl` 调用辅助。
    完成标准: 测试里可以统一调用 `crictl --runtime-endpoint ... --image-endpoint ...`，并拿到 stdout/stderr/exit code。
  - `M0-3` 固化 Pod/Container/Image 测试夹具。
    完成标准: 至少提供 `runp` 配置、`create` 配置、`pause`/`busybox` 一类可执行镜像夹具。
  - `M0-4` 补基础 smoke case。
    范围: `version`、`info`、`runp`、`pods`、`inspectp`、`create`、`start`、`inspect`、`stop`、`rm`、`rmp`。
  - `M0-5` 补 streaming smoke case。
    范围: `exec --sync`、`exec`、`exec -it`、`attach`。
  - `M0-6` 补镜像 smoke case。
    范围: `pull`、`images`、`inspecti`、`rmi`。
  - `M0-7` 给每个 smoke case 标注“当前行为”和“目标行为”。
    完成标准: 后续改动时能明确知道是修 bug 还是改契约。
  - `M0-8` 把测试入口挂到统一命令。
    建议命令: `cargo test --test crictl_smoke -- --nocapture`
- 验收:
  - 能在本地一键执行 smoke suite
  - 测试输出中明确标出失败的是“契约差距”还是“功能损坏”

### 6.4 `M1` 修正 CRI 契约与幂等语义

- 目标:
  - 先修掉与 CRI-O 明显不一致、但实现成本低的返回值与幂等问题
- 主要触点:
  - `src/server/mod.rs`
  - `src/image/mod.rs`
  - `tests/integration_tests.rs`
  - `tests/crictl_smoke.rs`
- 前置依赖:
  - `M0`
- 任务拆分:
  - `M1-1` 修正 `StopContainer` 不存在对象时的行为。
    修改点: `src/server/mod.rs:2607` 附近。
    完成标准: 容器不存在时返回空响应，不返回 `NotFound`。
  - `M1-2` 修正 `RemoveContainer` 不存在对象时的行为。
    修改点: `src/server/mod.rs:2728` 附近。
    完成标准: 容器不存在时返回空响应。
  - `M1-3` 修正 `StopPodSandbox` 不存在对象时的行为。
    修改点: `src/server/mod.rs:1412` 附近。
    完成标准: Pod 不存在时返回空响应。
  - `M1-4` 修正 `RemovePodSandbox` 不存在对象时的行为。
    修改点: `src/server/mod.rs:1791` 附近。
    完成标准: Pod 不存在时返回空响应。
  - `M1-5` 修正 `RemoveImage` 不存在对象时的行为。
    修改点: `src/image/mod.rs:709` 附近。
    完成标准: 镜像不存在时返回成功。
  - `M1-6` 修正 `ImageStatus` not-found 契约。
    修改点: `src/image/mod.rs:537` 附近。
    完成标准: 查不到镜像时返回空 `ImageStatusResponse`，不抛 gRPC `NotFound`。
  - `M1-7` 实现 `ListImagesRequest.filter` 最小支持。
    修改点: `src/image/mod.rs:520` 附近。
    完成标准: 传入 `filter.image.image` 时，只返回匹配镜像。
  - `M1-8` 给上述每一项都补“重复执行”测试。
    完成标准: 第二次调用不报错。
  - `M1-9` 给短 ID / 前缀匹配补歧义测试。
    完成标准: 单命中成功，多命中返回 `InvalidArgument`，零命中按对应幂等语义处理。
- 验收:
  - `crictl stop/rm/stopp/rmp/rmi` 重复执行不报错
  - `crictl inspecti <不存在镜像>` 返回空对象而不是 RPC 错误

### 6.5 `M2` 实现真正可用的 `PortForward`

- 目标:
  - 把当前固定 URL 占位实现替换为真实 streaming 数据面
- 主要触点:
  - `src/server/mod.rs`
  - `src/streaming/mod.rs`
  - 建议新增 `src/streaming/portforward.rs`
  - `src/pod/mod.rs`
  - `src/network/`
  - `tests/crictl_smoke.rs`
- 前置依赖:
  - `M0`
  - 建议先完成 `M1`
- 任务拆分:
  - `M2-1` 先确认 `crictl port-forward` 的实际协议。
    完成标准: 记录 `X-Stream-Protocol-Version`、stream header 和每端口 stream 的组织方式。
  - `M2-2` 扩展 `StreamingRequest`，增加 `PortForward` 变体。
    修改点: `src/streaming/mod.rs`
  - `M2-3` 新增 `get_port_forward()` 和请求校验。
    完成标准: 校验 `pod_sandbox_id` 非空、端口列表非空、端口值合法。
  - `M2-4` 新增 `/portforward/:token` 路由。
    完成标准: URL 由 RPC 动态生成，不再返回固定 `unix:///var/run/crius/crius.sock`。
  - `M2-5` 在 RPC 侧解析并校验 Pod 状态。
    修改点: `src/server/mod.rs:1744` 附近。
    完成标准: Pod 不存在、未 ready、无 netns path 时返回明确错误。
  - `M2-6` 实现 netns 内端口连接。
    方案要求: 至少支持 TCP；明确是直接 `setns` 建连，还是复用已有 network helper。
  - `M2-7` 实现单端口数据转发。
    完成标准: 双向字节流可传输，客户端关闭后能正确清理。
  - `M2-8` 实现多端口并行转发。
    完成标准: 单次会话中多个端口互不干扰。
  - `M2-9` 实现错误流语义。
    完成标准: 目标端口不存在、连接失败、Pod netns 异常时，错误能回到 `crictl`。
  - `M2-10` 增加集成测试。
    场景: Pod 内起 TCP echo server，验证 host 侧经 `crictl port-forward` 能收发。
  - `M2-11` 增加异常测试。
    场景: 非 ready Pod、非法端口、客户端中断、多端口混合成功/失败。
- 验收:
  - `crictl port-forward <pod> <hostPort>:<containerPort>` 可实际通信
  - 多端口转发可用
  - 失败场景返回可读错误

### 6.6 `M3` 补齐 `stats` / `statsp` / `metricsp`

- 目标:
  - 让 `crictl stats`、`crictl statsp`、`crictl metricsp` 返回真实数据而不是空对象
- 主要触点:
  - `src/server/mod.rs`
  - `src/runtime/mod.rs`
  - `src/cgroups/mod.rs`
  - `src/metrics/mod.rs`
  - 建议新增 `src/cgroups/stats.rs`
  - `tests/integration_tests.rs`
- 前置依赖:
  - `M0`
- 任务拆分:
  - `M3-1` 设计统一的 cgroup 统计读取接口。
    完成标准: 至少能读取 CPU、memory、pids、fs 基础字段。
  - `M3-2` 建立“container id -> cgroup path”解析。
    来源候选: OCI spec、container annotations、runtime bundle。
  - `M3-3` 实现 `ContainerStats`。
    修改点: `src/server/mod.rs:2808`。
    完成标准: `stats` 不再返回 `None`。
  - `M3-4` 实现 `ListContainerStats`。
    修改点: `src/server/mod.rs:2825`。
    完成标准: 支持按 container id、pod id、labels 过滤。
  - `M3-5` 实现 `PodSandboxStats`。
    修改点: `src/server/mod.rs:2836`。
    完成标准: 至少能聚合 Pod 内容器统计，或者读取 Pod cgroup。
  - `M3-6` 实现 `ListPodSandboxStats`。
    修改点: `src/server/mod.rs:2853`。
    完成标准: 支持按 pod id、labels 过滤。
  - `M3-7` 明确 Crius 当前支持的 metric descriptor 集。
    完成标准: `ListMetricDescriptors` 返回非空 descriptor。
  - `M3-8` 实现 `ListPodSandboxMetrics`。
    完成标准: 至少能把 Pod 级指标和容器级指标组织出来。
  - `M3-9` 补字段单位与时间戳测试。
    完成标准: 时间戳非零，核心计数字段非负，单位一致。
  - `M3-10` 补真实运行场景测试。
    场景: 跑一个持续 CPU/memory 活动容器，验证前后两次 stats 变化。
- 验收:
  - `crictl stats`
  - `crictl statsp`
  - `crictl metricsp`
  - 三类命令都不再是空返回

### 6.7 `M4` 把 `events` 改成持续事件流

- 目标:
  - 把当前“启动时发一轮快照”的实现改成真正的 runtime 事件广播
- 主要触点:
  - `src/server/mod.rs`
  - 建议新增 `src/server/events.rs`
  - `tests/integration_tests.rs`
  - `tests/crictl_smoke.rs`
- 前置依赖:
  - `M0`
  - 建议在 `M1` 后做
- 任务拆分:
  - `M4-1` 在 `RuntimeServiceImpl` 内加入长期存活的 broadcaster。
    完成标准: 服务启动后，不必等到有客户端才创建事件基础设施。
  - `M4-2` 统一事件类型常量。
    完成标准: 不再手写 `0`、`2` 这类裸数值。
  - `M4-3` 在 `run_pod_sandbox` 完成后发送 Pod 创建/启动相关事件。
  - `M4-4` 在 `create_container` 后发送 container created 事件。
  - `M4-5` 在 `start_container` 后发送 container started 事件。
  - `M4-6` 在 `stop_container` 后发送 container stopped 事件。
  - `M4-7` 在 `remove_container` / `remove_pod_sandbox` 后发送 delete 事件。
  - `M4-8` 发送事件时补全 `pod_sandbox_status` 和 `containers_statuses` 快照。
    完成标准: 客户端拿到的事件具备最小可观测上下文。
  - `M4-9` `GetContainerEvents` 改成持续阻塞读取 broadcaster，而不是创建一次性任务。
  - `M4-10` 加入客户端断开、发送失败、背压处理。
    完成标准: 单客户端异常不会拖死全局事件流。
  - `M4-11` 补顺序性测试。
    场景: create -> start -> stop -> remove 事件按顺序到达。
  - `M4-12` 补多客户端测试。
    完成标准: 两个客户端都能收到同一事件。
- 验收:
  - `crictl events` 挂起等待时，后续 lifecycle 动作能持续输出事件
  - 客户端 Ctrl-C 退出不会导致服务端 goroutine/task 泄漏

### 6.8 `M5` 补齐 `logs` / `update` / `status` / `runtime-config`

- 目标:
  - 补全剩余控制面能力，使 `crictl info`、`crictl update`、`crictl logs` 语义更接近 CRI-O
- 主要触点:
  - `src/server/mod.rs`
  - `src/runtime/mod.rs`
  - `src/shim/io.rs`
  - `src/runtime/shim_manager.rs`
  - `src/config/mod.rs`
  - `src/cgroups/mod.rs`
- 前置依赖:
  - `M0`
- 任务拆分:
  - `M5-1` 为 runtime trait 增加 `reopen_container_log()`。
    完成标准: RPC 不再是空实现。
  - `M5-2` 在 shim IO 层增加“重开日志文件句柄”能力。
    修改点: `src/shim/io.rs`
  - `M5-3` 在 `ReopenContainerLog` RPC 中补容器存在性和 running 校验。
    修改点: `src/server/mod.rs:2787`
  - `M5-4` 为 runtime trait 增加 `update_container_resources()`。
    完成标准: 能把 CRI Linux resources 下发到 runtime。
  - `M5-5` 实现 `runc update` 参数或 JSON 配置生成。
    修改点: `src/runtime/mod.rs`
  - `M5-6` 在 `UpdateContainerResources` 成功后同步更新内部状态与持久化。
    完成标准: 后续 `inspect` 中 `resources` 能看到最新值。
  - `M5-7` 让 `Status` 返回真实 `NetworkReady`。
    数据来源: CNI 检测结果或 network manager 健康检查。
  - `M5-8` 让 `Status` 返回 `RuntimeHandlers`。
    完成标准: 包含默认 runtime 和配置的 handler 列表。
  - `M5-9` 让 `Status` 返回 `RuntimeFeatures`。
    最低要求: 填当前确实支持的能力，不要继续全空。
  - `M5-10` 让 `Status(verbose=true)` 返回结构化配置 JSON，而不是简单散列 map。
  - `M5-11` 实现 `RuntimeConfig` 的 cgroup driver 探测。
    完成标准: 返回 `systemd` 或 `cgroupfs`。
  - `M5-12` 补 `ContainerStatus` 的 `reason/message/mounts/user/verbose info`。
    修改点: `src/server/mod.rs:1534`、`src/server/mod.rs:522`
  - `M5-13` 补 `PodSandboxStatus(verbose)` 的统一 JSON info。
  - `M5-14` 补 `crictl info` / `inspect` / `inspectp` 回归测试。
- 验收:
  - `crictl info` 能看到真实 handler / feature / cgroup driver
  - `crictl update` 能影响容器 cgroup 参数
  - `crictl inspect` 能看到 `reason/message/mounts`
  - `crictl logs` 所依赖的日志 reopen 契约可用

### 6.9 `M6` 补强 `exec` / `attach` 契约

- 目标:
  - 让当前已经可用的 streaming 主路径更接近 CRI-O 的契约完整度
- 主要触点:
  - `src/server/mod.rs`
  - `src/streaming/mod.rs`
  - `src/runtime/mod.rs`
  - `src/shim/io.rs`
  - `src/shim/daemon.rs`
- 前置依赖:
  - `M0`
  - 建议在 `M5` 后做
- 任务拆分:
  - `M6-1` 在 `Exec` RPC 生成 URL 前加入容器活性校验。
    完成标准: 非 running/created 容器不能成功拿到 URL。
  - `M6-2` 在 `Attach` RPC 生成 URL 前加入容器活性校验。
  - `M6-3` 对 `exec --sync` 增加与 `exec` 相同的活性校验逻辑。
  - `M6-4` 明确并实现 resize channel 协议。
    完成标准: 识别 resize stream / channel，能解析终端宽高。
  - `M6-5` 在 PTY 模式下把 resize 应用到 slave/master。
    完成标准: 终端窗口变化后，容器内 `stty size` 可观察到变化。
  - `M6-6` 给 `attach` 增加 resize 支持。
  - `M6-7` 规范 error stream 语义。
    完成标准: 非零退出码、协议错误、spawn 失败、attach socket 失联都有明确错误回传。
  - `M6-8` 规范结束语义。
    完成标准: `stdout/stderr/error` 的 FIN 和 GOAWAY 顺序稳定，客户端不会卡住。
  - `M6-9` 评估是否增加 runtime-handler/websocket 接管层。
    说明: 如果短期不实现，也应在文档中明确标注“不支持”。
  - `M6-10` 补 `exec -it` / `attach -it` / resize / runtime 异常测试。
- 验收:
  - `crictl exec -it` 中调整终端大小后，交互正常
  - `crictl attach` 不会因正常退出或客户端中断而卡住

### 6.10 `M7` 恢复语义与状态对账

- 目标:
  - 让 daemon 重启后，Crius 的 live-state、exit-code、attach 可用性、事件面尽量接近 CRI-O
- 主要触点:
  - `src/server/mod.rs`
  - `src/runtime/shim_manager.rs`
  - `src/runtime/mod.rs`
  - `src/storage/persistence.rs`
  - `src/pod/mod.rs`
  - `src/network/`
  - 建议新增 `src/server/reconcile.rs`
- 前置依赖:
  - `M0`
  - 建议在 `M4`、`M5`、`M6` 之后做
- 任务拆分:
  - `M7-1` 去掉 runtime 侧硬编码镜像目录。
    修改点: `src/runtime/mod.rs:483`
    完成标准: runtime 与 image service 共用同一配置来源。
  - `M7-2` 把 shim 元数据落盘。
    内容建议: `container_id`、`shim_pid`、`exit_code_file`、`socket_path`、`bundle_path`。
  - `M7-3` 启动时从 shim 工作目录重建 shim 注册表。
    完成标准: daemon 重启后能重新识别 live shim。
  - `M7-4` 增加 exit monitor。
    方案可以是: 监听 exit file、监听 shim 退出、或周期性 reconcile。
  - `M7-5` 启动时对 DB 状态与 runtime 实际状态做一次对账。
    完成标准: DB 里 running 但 runtime 已 stopped 的对象会被修正。
  - `M7-6` 启动时修复 Pod IP / netns / pause container 可见状态。
  - `M7-7` 明确 daemon 重启后的 attach 语义。
    方案要求: 明确哪些场景允许恢复 attach，哪些场景返回明确“不支持”。
  - `M7-8` 对账后触发必要事件或状态刷新。
  - `M7-9` 补“daemon 重启前创建容器、重启后继续 inspect/ps/attach/stop/rm”的测试。
  - `M7-10` 补脏资源清理测试。
    场景: DB 有记录但 runtime 无对象；runtime 有对象但 DB 无记录。
- 验收:
  - daemon 重启后 `crictl ps`、`inspect`、`stopp`、`rm` 仍可正常使用
  - 状态不会长期停留在过期快照

### 6.11 `M8` 镜像服务完善

- 目标:
  - 把当前“最小可用”的镜像实现补成更接近 CRI-O 契约的版本
- 主要触点:
  - `src/image/mod.rs`
  - `src/runtime/mod.rs`
  - `src/storage/`
  - `tests/crictl_smoke.rs`
- 前置依赖:
  - `M0`
  - 建议在 `M1` 后分阶段进行
- 任务拆分:
  - `M8-1` 把镜像 ID 持久化改成完整 digest，不再截断成 12 位短 ID。
  - `M8-2` 补 `auth.auth` 字段解码。
  - `M8-3` 增加并发 pull 去重。
    完成标准: 相同镜像并发拉取时只执行一次远端拉取。
  - `M8-4` 增加短名/多候选解析策略。
  - `M8-5` 评估并补 namespaced auth / signature policy。
    说明: 如果暂不做，文档中要明确范围。
  - `M8-6` 扩展本地镜像元数据模型。
    目标字段: `repo_digests`、`uid`、`username`、`pinned`、`annotations`。
  - `M8-7` 实现 `ImageStatus(verbose)`。
  - `M8-8` 实现 `ImageFsInfo` 的真实磁盘统计。
  - `M8-9` 实现 `RemoveImage` in-use 检查。
    完成标准: 被容器引用的镜像不能被静默删掉。
  - `M8-10` 补 `ListImages` 去重与 filter 行为测试。
  - `M8-11` 补 `inspecti` / `rmi` / `imagefsinfo` 真实 `crictl` 回归测试。
- 验收:
  - `crictl images` 字段明显更完整
  - `crictl inspecti` 的 not-found 和 verbose 行为接近 CRI-O
  - `crictl imagefsinfo` 返回真实 usage

### 6.12 推荐的落地节奏

- 第 1 周:
  - `M0`
  - `M1`
- 第 2 周:
  - `M2`
  - `M3`
- 第 3 周:
  - `M4`
  - `M5`
- 第 4 周:
  - `M6`
  - `M7`
  - `M8`

如果资源有限，最小闭环顺序建议压缩为:

1. `M0`
2. `M1`
3. `M2`
4. `M3`
5. `M4`
6. `M5` 中的 `ReopenContainerLog`、`Status`、`RuntimeConfig`
7. `M6` 中的活性校验与 resize

### 6.13 完成定义（Definition of Done）

- P0 完成定义:
  - `port-forward`、`stats`、`statsp`、`events` 可真实使用
  - stop/remove 系列接口具备幂等行为
  - `ImageStatus` / `RemoveImage` 契约纠正完成
- P1 完成定义:
  - `ReopenContainerLog`、`UpdateContainerResources`、`Status`、`RuntimeConfig`、`inspect`/`inspectp` 关键字段到位
  - `exec` / `attach` 支持 resize，错误语义可预测
- P2 完成定义:
  - daemon 重启后状态与 live 行为明显稳定
  - 镜像元数据和镜像磁盘统计接近 CRI-O

### 6.14 最细粒度任务清单（按可提交单位）

这一节把上面的里程碑继续拆到更细，目标是做到“拿一条就能开干”。建议每一条任务尽量控制在单个 PR 或单次 commit 主题范围内。

#### 6.14.1 `M0` 契约测试基线最细拆分

- `M0-S01` 新增测试工作目录创建器。
  产物: 临时 `root_dir`、socket 路径、streaming 端口分配函数。
- `M0-S02` 新增 Crius daemon 子进程启动函数。
  产物: `start_crius_daemon()`。
- `M0-S03` 新增 daemon ready 探测函数。
  产物: gRPC `Version` 轮询或 socket ready 检测。
- `M0-S04` 新增 daemon 停止与清理函数。
  产物: `stop_crius_daemon()`，确保测试结束后不残留进程。
- `M0-S05` 新增 `crictl` 命令包装器。
  产物: `run_crictl(args)`，统一 runtime/image endpoint 参数。
- `M0-S06` 新增 `crictl` 结果结构体。
  产物: `stdout/stderr/exit_code` 统一封装。
- `M0-S07` 固化 `runp` fixture。
  产物: PodSandboxConfig JSON/YAML。
- `M0-S08` 固化 `create` fixture。
  产物: ContainerConfig JSON/YAML。
- `M0-S09` 固化基础镜像 fixture 描述。
  产物: `pause`、`busybox` 或等价镜像说明。
- `M0-S10` 新增 `version` smoke case。
- `M0-S11` 新增 `info` smoke case。
- `M0-S12` 新增 `runp` smoke case。
- `M0-S13` 新增 `pods` smoke case。
- `M0-S14` 新增 `inspectp` smoke case。
- `M0-S15` 新增 `create` smoke case。
- `M0-S16` 新增 `start` smoke case。
- `M0-S17` 新增 `inspect` smoke case。
- `M0-S18` 新增 `stop` smoke case。
- `M0-S19` 新增 `rm` smoke case。
- `M0-S20` 新增 `rmp` smoke case。
- `M0-S21` 新增 `exec --sync` smoke case。
- `M0-S22` 新增 `exec` smoke case。
- `M0-S23` 新增 `exec -it` smoke case。
- `M0-S24` 新增 `attach` smoke case。
- `M0-S25` 新增 `pull` smoke case。
- `M0-S26` 新增 `images` smoke case。
- `M0-S27` 新增 `inspecti` smoke case。
- `M0-S28` 新增 `rmi` smoke case。
- `M0-S29` 给每个 smoke case 标注“当前预期行为”。
- `M0-S30` 新增统一测试入口文档说明。

#### 6.14.2 `M1` 契约与幂等语义最细拆分

- `M1-S01` 为 `resolve_container_id()` 增加“零命中/单命中/多命中”测试。
- `M1-S02` 为 `resolve_pod_sandbox_id()` 增加“零命中/单命中/多命中”测试。
- `M1-S03` 调整 `StopContainer` 对不存在对象的返回语义。
- `M1-S04` 为 `StopContainer` 增加“首次停止成功”测试。
- `M1-S05` 为 `StopContainer` 增加“重复停止成功”测试。
- `M1-S06` 调整 `RemoveContainer` 对不存在对象的返回语义。
- `M1-S07` 为 `RemoveContainer` 增加“首次删除成功”测试。
- `M1-S08` 为 `RemoveContainer` 增加“重复删除成功”测试。
- `M1-S09` 调整 `StopPodSandbox` 对不存在对象的返回语义。
- `M1-S10` 为 `StopPodSandbox` 增加“重复停止成功”测试。
- `M1-S11` 调整 `RemovePodSandbox` 对不存在对象的返回语义。
- `M1-S12` 为 `RemovePodSandbox` 增加“重复删除成功”测试。
- `M1-S13` 调整 `RemoveImage` 对不存在对象的返回语义。
- `M1-S14` 为 `RemoveImage` 增加“重复删除成功”测试。
- `M1-S15` 调整 `ImageStatus` 的 not-found 行为。
- `M1-S16` 为 `ImageStatus` 增加“命中返回 image”测试。
- `M1-S17` 为 `ImageStatus` 增加“未命中返回空响应”测试。
- `M1-S18` 为 `ListImages` 增加 filter 参数解析。
- `M1-S19` 为 `ListImages` 增加“无 filter”测试。
- `M1-S20` 为 `ListImages` 增加“有 filter 命中”测试。
- `M1-S21` 为 `ListImages` 增加“有 filter 未命中”测试。
- `M1-S22` 整理 stop/rm/stopp/rmp/rmi 的契约回归清单。

#### 6.14.3 `M2` `PortForward` 最细拆分

- `M2-S01` 确认 `crictl port-forward` 对应的 CRI proto 字段。
- `M2-S02` 确认 streaming server 是否需要 SPDY 还是 websocket。
- `M2-S03` 在 `StreamingRequest` 增加 `PortForward` 分支。
- `M2-S04` 新增 `validate_port_forward_request()`。
- `M2-S05` 增加 `get_port_forward()` URL 生成函数。
- `M2-S06` 在 `handle_request()` 中增加 `portforward` 路由。
- `M2-S07` 定义 `expected_port_forward_roles()`。
- `M2-S08` 解析 `portforward` stream header。
- `M2-S09` 确认单端口 stream 与多端口 stream 的 stream id 组织方式。
- `M2-S10` 在 RPC 层补 `pod_sandbox_id` 解析。
- `M2-S11` 在 RPC 层补 Pod ready 校验。
- `M2-S12` 在 RPC 层补 netns path 获取。
- `M2-S13` 新增 netns 进入辅助函数。
- `M2-S14` 新增 TCP socket 建连辅助函数。
- `M2-S15` 完成单端口上行数据转发。
- `M2-S16` 完成单端口下行数据转发。
- `M2-S17` 处理客户端 half-close。
- `M2-S18` 处理远端 half-close。
- `M2-S19` 完成单端口异常回传。
- `M2-S20` 完成多端口会话并发模型。
- `M2-S21` 处理多端口中某一路失败时的行为。
- `M2-S22` 处理整个 session 提前结束时的清理。
- `M2-S23` 增加单端口集成测试。
- `M2-S24` 增加双端口集成测试。
- `M2-S25` 增加非法端口测试。
- `M2-S26` 增加 Pod not ready 测试。
- `M2-S27` 增加客户端中断测试。
- `M2-S28` 补 `crictl port-forward` 实机验证命令。

#### 6.14.4 `M3` `stats/statsp/metricsp` 最细拆分

- `M3-S01` 盘点 CRI `ContainerStats` 必填/常用字段。
- `M3-S02` 盘点 CRI `PodSandboxStats` 必填/常用字段。
- `M3-S03` 新增 cgroup v1 读取入口。
- `M3-S04` 新增 cgroup v2 读取入口。
- `M3-S05` 实现 CPU usage 读取。
- `M3-S06` 实现 memory usage 读取。
- `M3-S07` 实现 memory working set 读取。
- `M3-S08` 实现 pids usage 读取。
- `M3-S09` 实现 fs stats 读取。
- `M3-S10` 实现 network stats 读取占位或最小支持策略说明。
- `M3-S11` 建立 container id 到 cgroup path 映射。
- `M3-S12` 为 running 容器实现 stats 读取。
- `M3-S13` 为 exited 容器确定 stats 返回策略。
- `M3-S14` 实现 `ContainerStats` RPC 组包。
- `M3-S15` 实现 `ListContainerStats` 基础枚举。
- `M3-S16` 实现 `ListContainerStats` filter by id。
- `M3-S17` 实现 `ListContainerStats` filter by pod。
- `M3-S18` 实现 `ListContainerStats` filter by label。
- `M3-S19` 实现 Pod 级聚合策略。
- `M3-S20` 实现 `PodSandboxStats` RPC 组包。
- `M3-S21` 实现 `ListPodSandboxStats` 枚举。
- `M3-S22` 实现 `ListPodSandboxStats` filter by id。
- `M3-S23` 实现 `ListPodSandboxStats` filter by label。
- `M3-S24` 盘点当前可支持的 metric descriptor。
- `M3-S25` 实现 `ListMetricDescriptors` 非空返回。
- `M3-S26` 实现 `ListPodSandboxMetrics` Pod 级指标组包。
- `M3-S27` 实现 `ListPodSandboxMetrics` 容器级指标组包。
- `M3-S28` 新增 stats 单元测试。
- `M3-S29` 新增 stats 集成测试。
- `M3-S30` 新增活动负载前后数值变化测试。
- `M3-S31` 补 `crictl stats/statsp/metricsp` 实机验证命令。

#### 6.14.5 `M4` `events` 最细拆分

- `M4-S01` 设计服务内 broadcaster 结构。
- `M4-S02` 设计订阅者注册表。
- `M4-S03` 设计订阅者移除流程。
- `M4-S04` 统一 CRI event type 常量映射。
- `M4-S05` 提供 `emit_container_created()`。
- `M4-S06` 提供 `emit_container_started()`。
- `M4-S07` 提供 `emit_container_stopped()`。
- `M4-S08` 提供 `emit_container_deleted()`。
- `M4-S09` 提供 `emit_sandbox_created_or_started()`。
- `M4-S10` 提供 `emit_sandbox_stopped()`。
- `M4-S11` 提供 `emit_sandbox_deleted()`。
- `M4-S12` 为事件组包补 Pod 状态快照。
- `M4-S13` 为事件组包补 container 状态快照。
- `M4-S14` 在 `run_pod_sandbox` 接入事件发送。
- `M4-S15` 在 `create_container` 接入事件发送。
- `M4-S16` 在 `start_container` 接入事件发送。
- `M4-S17` 在 `stop_container` 接入事件发送。
- `M4-S18` 在 `remove_container` 接入事件发送。
- `M4-S19` 在 `stop_pod_sandbox` 接入事件发送。
- `M4-S20` 在 `remove_pod_sandbox` 接入事件发送。
- `M4-S21` `GetContainerEvents` 改成长期阻塞流。
- `M4-S22` 加入发送超时/背压策略。
- `M4-S23` 加入客户端断开清理。
- `M4-S24` 加入多客户端广播测试。
- `M4-S25` 加入顺序性测试。
- `M4-S26` 加入无订阅者时事件不会 panic 的测试。
- `M4-S27` 补 `crictl events` 实机验证命令。

#### 6.14.6 `M5` `logs/update/status/runtime-config` 最细拆分

- `M5-S01` 在 runtime trait 增加 `reopen_container_log()` 定义。
- `M5-S02` 在 `RuncRuntime` 中增加 `reopen_container_log()` stub。
- `M5-S03` 设计 shim log fd 重开协议。
- `M5-S04` 在 `IoManager` 增加 reopen 日志文件方法。
- `M5-S05` 在 shim daemon 增加接收 reopen 指令的通道。
- `M5-S06` 在 `ReopenContainerLog` RPC 中补 container id 解析。
- `M5-S07` 在 `ReopenContainerLog` RPC 中补 running 校验。
- `M5-S08` 在 `ReopenContainerLog` RPC 中补 runtime 调用。
- `M5-S09` 为 runtime trait 增加 `update_container_resources()`。
- `M5-S10` 定义 CRI resources 到 OCI resources 的转换函数。
- `M5-S11` 实现 `runc update` JSON 文件生成。
- `M5-S12` 实现 `runc update` 调用。
- `M5-S13` 更新成功后刷新内存中的 `resources`。
- `M5-S14` 更新成功后刷新持久化中的 `resources`。
- `M5-S15` 为 `UpdateContainerResources` 增加存在性校验。
- `M5-S16` 为 `UpdateContainerResources` 增加状态校验。
- `M5-S17` 为 `Status` 增加 network health 探测。
- `M5-S18` 为 `Status` 增加 runtime handler 列表组装。
- `M5-S19` 为 `Status` 增加 runtime feature 组装。
- `M5-S20` 为 `Status(verbose)` 增加结构化配置 JSON。
- `M5-S21` 为 `RuntimeConfig` 增加 cgroup driver 探测。
- `M5-S22` 为 `ContainerStatus` 增加 `reason` 字段填充。
- `M5-S23` 为 `ContainerStatus` 增加 `message` 字段填充。
- `M5-S24` 为 `ContainerStatus` 增加 `mounts` 字段填充。
- `M5-S25` 为 `ContainerStatus` 增加 `user` 字段填充。
- `M5-S26` 为 `ContainerStatus(verbose)` 增加 JSON info。
- `M5-S27` 为 `PodSandboxStatus(verbose)` 增加 JSON info。
- `M5-S28` 增加 `ReopenContainerLog` 单元测试。
- `M5-S29` 增加 `UpdateContainerResources` 单元测试。
- `M5-S30` 增加 `Status/RuntimeConfig` 单元测试。
- `M5-S31` 增加 `inspect/inspectp/info` 集成测试。
- `M5-S32` 补 `crictl info/update/inspect/inspectp` 实机验证命令。

#### 6.14.7 `M6` `exec/attach` 最细拆分

- `M6-S01` 在 `Exec` RPC 入口补存在性校验。
- `M6-S02` 在 `Exec` RPC 入口补活性校验。
- `M6-S03` 在 `Attach` RPC 入口补存在性校验。
- `M6-S04` 在 `Attach` RPC 入口补活性校验。
- `M6-S05` 在 `ExecSync` 入口补活性校验。
- `M6-S06` 盘点当前 remotecommand 版本协商逻辑。
- `M6-S07` 设计 resize stream header 解析。
- `M6-S08` 在 `exec` path 增加 resize stream 注册。
- `M6-S09` 在 `attach` path 增加 resize stream 注册。
- `M6-S10` 实现 resize payload 解析。
- `M6-S11` 实现 PTY 宽高设置。
- `M6-S12` 处理 resize 事件与 stdout/stderr 并发。
- `M6-S13` 规范 spawn 失败时 error stream 输出。
- `M6-S14` 规范非零退出码时 error stream 输出。
- `M6-S15` 规范 attach socket 连接失败时 error stream 输出。
- `M6-S16` 规范客户端主动断开时的清理顺序。
- `M6-S17` 规范服务端主动结束时的 FIN/GOAWAY 顺序。
- `M6-S18` 评估 websocket/runtime-handler 接管层接口。
- `M6-S19` 如果短期不做 websocket，补显式不支持说明。
- `M6-S20` 增加 `exec -it` resize 测试。
- `M6-S21` 增加 `attach -it` resize 测试。
- `M6-S22` 增加 exec 异常退出测试。
- `M6-S23` 增加 attach 客户端中断测试。
- `M6-S24` 补 `crictl exec/attach` 实机验证命令。

#### 6.14.8 `M7` 恢复语义最细拆分

- `M7-S01` 删除 runtime 中硬编码 image store 路径。
- `M7-S02` 把 image store 路径改成从 `RuntimeConfig` 注入。
- `M7-S03` 定义 shim 元数据文件格式。
- `M7-S04` 在 `start_shim()` 时写出元数据文件。
- `M7-S05` 在 `stop_shim()` 时更新或删除元数据文件。
- `M7-S06` 新增 `load_shims_from_disk()`。
- `M7-S07` 在 daemon 启动时调用 `load_shims_from_disk()`。
- `M7-S08` 设计 exit monitor 来源。
- `M7-S09` 实现 exit file watcher。
- `M7-S10` 实现 shim pid watcher 或轮询。
- `M7-S11` 设计状态对账入口。
- `M7-S12` 实现“DB running 但 runtime stopped”的修正逻辑。
- `M7-S13` 实现“DB created 但 runtime unknown”的修正逻辑。
- `M7-S14` 实现“runtime running 但 DB 缺失”的处理策略。
- `M7-S15` 实现 Pod IP 恢复对账。
- `M7-S16` 实现 netns path 恢复对账。
- `M7-S17` 实现 pause container id 恢复对账。
- `M7-S18` 明确 attach 重启后恢复策略。
- `M7-S19` 如果支持恢复 attach，补 socket 可用性重建。
- `M7-S20` 如果不支持恢复 attach，补明确错误返回。
- `M7-S21` 对账后触发状态刷新。
- `M7-S22` 对账后按需要补发事件或不补发策略说明。
- `M7-S23` 增加“重启后 inspect/ps”测试。
- `M7-S24` 增加“重启后 stop/rm”测试。
- `M7-S25` 增加“DB 与 runtime 不一致”测试。
- `M7-S26` 增加“shim 元数据残留”测试。

#### 6.14.9 `M8` 镜像服务最细拆分

- `M8-S01` 把镜像 ID 存储从短 digest 改成完整 digest。
- `M8-S02` 为旧短 ID 兼容读取加迁移策略。
- `M8-S03` 新增 `auth.auth` base64 解码。
- `M8-S04` 新增用户名密码与 `auth.auth` 优先级规则。
- `M8-S05` 设计 pull 去重键。
- `M8-S06` 实现 pull in-progress 注册表。
- `M8-S07` 实现 pull 等待/复用逻辑。
- `M8-S08` 设计短名候选解析流程。
- `M8-S09` 实现候选解析命中第一策略或可配置策略。
- `M8-S10` 评估 namespaced auth 目录格式。
- `M8-S11` 评估 signature policy 配置格式。
- `M8-S12` 扩展 `ImageMeta` 结构体。
- `M8-S13` 为 `repo_digests` 持久化加字段。
- `M8-S14` 为 `uid` 持久化加字段。
- `M8-S15` 为 `username` 持久化加字段。
- `M8-S16` 为 `pinned` 持久化加字段。
- `M8-S17` 为 `annotations` 持久化加字段。
- `M8-S18` 实现 `ImageStatus(verbose)` 组包。
- `M8-S19` 实现 `ListImages` 去重逻辑。
- `M8-S20` 实现 `ListImages` filter 命中完整路径测试。
- `M8-S21` 实现磁盘 usage 统计辅助函数。
- `M8-S22` 实现 `ImageFsInfo` image filesystem 填充。
- `M8-S23` 实现 `ImageFsInfo` container filesystem 填充。
- `M8-S24` 实现 `RemoveImage` in-use 检查。
- `M8-S25` 为被运行容器引用的镜像增加删除失败测试。
- `M8-S26` 为未引用镜像增加成功删除测试。
- `M8-S27` 为 verbose `inspecti` 增加测试。
- `M8-S28` 为 `imagefsinfo` 增加测试。
- `M8-S29` 补 `crictl pull/images/inspecti/rmi/imagefsinfo` 实机验证命令。

#### 6.14.10 推荐拆 issue 的方式

- 每个 `M*-S**` 都可以直接拆成 issue。
- 如果 issue 管理不想过细，建议按下面方式聚合:
  - 聚合 1: `M0-S01` 到 `M0-S08`
  - 聚合 2: `M1-S01` 到 `M1-S22`
  - 聚合 3: `M2-S01` 到 `M2-S11`
  - 聚合 4: `M2-S12` 到 `M2-S28`
  - 聚合 5: `M3-S01` 到 `M3-S16`
  - 聚合 6: `M3-S17` 到 `M3-S31`
  - 聚合 7: `M4-S01` 到 `M4-S27`
  - 聚合 8: `M5-S01` 到 `M5-S16`
  - 聚合 9: `M5-S17` 到 `M5-S32`
  - 聚合 10: `M6-S01` 到 `M6-S24`
  - 聚合 11: `M7-S01` 到 `M7-S13`
  - 聚合 12: `M7-S14` 到 `M7-S26`
  - 聚合 13: `M8-S01` 到 `M8-S11`
  - 聚合 14: `M8-S12` 到 `M8-S29`

#### 6.14.11 4 人并行开发分工表

假设 4 个人能力接近，推荐采用“单文件主责制”，尽量避免多人同时修改同一个核心文件。尤其是 `src/server/mod.rs`、`src/streaming/mod.rs`、`src/runtime/mod.rs`、`src/image/mod.rs` 这 4 个核心入口，原则上分别只给 1 个主责开发者长期持有。

| 开发者 | 角色定位 | 主责模块 | 主责文件写入边界 | 主要任务编号 | 主要交付物 |
| --- | --- | --- | --- | --- | --- |
| `Dev-A` | 服务层集成负责人 | CRI RPC、事件、状态、恢复入口 | `src/server/mod.rs`、建议新增 `src/server/events.rs`、建议新增 `src/server/reconcile.rs`、`src/config/mod.rs` | `M1-S01`~`M1-S12`、`M2-S10`~`M2-S12`、`M4-S01`~`M4-S27`、`M5-S17`~`M5-S32`、`M7-S21`~`M7-S26` | 幂等语义修正、事件广播、`Status/RuntimeConfig`、`inspect/inspectp` 字段补齐、恢复入口整合 |
| `Dev-B` | Streaming/日志/转发负责人 | `port-forward`、`exec/attach`、log reopen 传输层 | `src/streaming/*`、`src/shim/io.rs`、`src/shim/daemon.rs`、必要时触及 `src/network/*` 的端口转发辅助 | `M2-S01`~`M2-S09`、`M2-S13`~`M2-S28`、`M5-S01`~`M5-S08`、`M6-S01`~`M6-S24` | `port-forward` 数据面、`exec/attach` resize、日志 reopen 通道、streaming 错误语义收敛 |
| `Dev-C` | Runtime/资源/恢复底座负责人 | runtime、cgroup、metrics、shim 元数据、状态对账底层 | `src/runtime/*`、`src/cgroups/*`、`src/metrics/*`、`src/storage/persistence.rs` | `M3-S01`~`M3-S31`、`M5-S09`~`M5-S16`、`M7-S01`~`M7-S20` | stats/metrics、`runc update`、shim 元数据落盘与恢复、runtime/DB 对账底座 |
| `Dev-D` | 镜像与测试负责人 | 镜像服务、测试基线、验收回归 | `src/image/*`、`tests/*`、`tests/fixtures/*`、文档中的测试命令清单 | `M0-S01`~`M0-S30`、`M1-S13`~`M1-S22`、`M8-S01`~`M8-S29` | smoke 测试基线、image 契约修正、image 元数据扩展、`imagefsinfo`、实机回归用例 |

#### 6.14.12 每个人的详细模块任务

##### `Dev-A` 服务层集成负责人

| 模块 | 任务编号 | 说明 | 前置依赖 | 不应主动修改 |
| --- | --- | --- | --- | --- |
| 容器/Pod 幂等语义 | `M1-S01`~`M1-S12` | 修 stop/rm/stopp/rmp 的 RPC 契约和短 ID 解析测试 | `M0` | `src/runtime/*`、`src/image/*` |
| `port-forward` RPC 入口 | `M2-S10`~`M2-S12` | 只负责 Pod ID 解析、ready 校验、netns path 获取 | `M2-S01`~`M2-S09` | `src/streaming/*` 核心数据面 |
| 事件框架 | `M4-S01`~`M4-S27` | broadcaster、订阅者管理、生命周期事件接线 | `M0`，建议 `M1` 完成 | `src/runtime/*` stats 采集逻辑 |
| `Status/RuntimeConfig` | `M5-S17`~`M5-S21` | RPC 组包、verbose 配置输出、handler/features 暴露 | `M5-S09`~`M5-S16` 部分完成更好 | `src/cgroups/*`、`src/runtime/*` 内部实现 |
| `inspect` / `inspectp` | `M5-S22`~`M5-S27` | `reason/message/mounts/user/info` 补齐 | `M5-S09`~`M5-S16` | `src/image/*` |
| 恢复入口整合 | `M7-S21`~`M7-S26` | 调用 runtime/DB 对账、暴露恢复策略、补恢复测试入口 | `M7-S01`~`M7-S20` | `src/runtime/shim_manager.rs` 细节实现 |

`Dev-A` 的完成标准:

- 只要是进入 `src/server/mod.rs` 的改动，都由 `Dev-A` 合并整合
- 其他开发者如果需要 server 层接线，先提交 helper API，再由 `Dev-A` 接入

##### `Dev-B` Streaming/日志/转发负责人

| 模块 | 任务编号 | 说明 | 前置依赖 | 不应主动修改 |
| --- | --- | --- | --- | --- |
| `port-forward` 协议与路由 | `M2-S01`~`M2-S09` | 协议确认、stream role、URL 生成、路由扩展 | `M0` | `src/server/mod.rs` 业务校验逻辑 |
| `port-forward` 数据面 | `M2-S13`~`M2-S22` | netns 进入、socket 建连、双向转发、session 清理 | `M2-S01`~`M2-S12` | `src/runtime/*` |
| `port-forward` 测试 | `M2-S23`~`M2-S28` | 单端口/双端口/异常/实机验证命令 | 数据面完成 | `src/image/*` |
| log reopen 传输 | `M5-S01`~`M5-S08` | runtime trait 衔接、shim reopen 通道、RPC 所需传输路径 | `M0` | `src/server/mod.rs` 字段补齐逻辑 |
| `exec/attach` 契约强化 | `M6-S01`~`M6-S24` | 活性校验接入点、resize、error stream、结束语义 | 建议 `M5` 基本完成 | `src/image/*`、`src/storage/*` |

`Dev-B` 的完成标准:

- `src/streaming/*` 与 `src/shim/*` 由 `Dev-B` 单人主改
- 任何新增 stream header、channel、protocol 约定，都要同步写测试

##### `Dev-C` Runtime/资源/恢复底座负责人

| 模块 | 任务编号 | 说明 | 前置依赖 | 不应主动修改 |
| --- | --- | --- | --- | --- |
| stats/metrics 底座 | `M3-S01`~`M3-S31` | cgroup 读取、stats 组包辅助、descriptor、metrics 数据 | `M0` | `src/image/*`、`tests/crictl_smoke.rs` 主体结构 |
| `UpdateContainerResources` 底层 | `M5-S09`~`M5-S16` | CRI 到 OCI 转换、`runc update`、状态刷新 | `M0` | `src/server/mod.rs` 组包逻辑 |
| 恢复与对账底座 | `M7-S01`~`M7-S20` | image path 去硬编码、shim 元数据落盘、启动恢复、exit monitor、DB/runtime 对账 | 建议 `M4`、`M5`、`M6` 基础完成后并行推进 | `src/image/*` |

`Dev-C` 的完成标准:

- `src/runtime/*`、`src/cgroups/*`、`src/metrics/*` 原则上只由 `Dev-C` 长期持有
- 所有 server 层只调用稳定 helper，不直接把 cgroup/runtime 细节写进 `src/server/mod.rs`

##### `Dev-D` 镜像与测试负责人

| 模块 | 任务编号 | 说明 | 前置依赖 | 不应主动修改 |
| --- | --- | --- | --- | --- |
| 测试基线 | `M0-S01`~`M0-S30` | smoke 基线、fixture、daemon/crictl helper、统一测试入口 | 无 | `src/server/mod.rs` 大规模业务逻辑 |
| 镜像契约修正 | `M1-S13`~`M1-S22` | `ImageStatus` not-found、`RemoveImage` 幂等、`ListImages` filter | `M0` | `src/runtime/*` |
| 镜像服务完善 | `M8-S01`~`M8-S29` | image id、auth、pull 去重、元数据扩展、`imagefsinfo`、实机验证 | `M0`，建议 `M1` 完成 | `src/server/mod.rs` 非 image RPC |

`Dev-D` 的完成标准:

- `src/image/*` 与 `tests/*` 由 `Dev-D` 单人主改
- 所有里程碑的验收命令、测试用例模板、fixture 统一由 `Dev-D` 维护

#### 6.14.13 4 人并行开发时的推荐排期

| 阶段 | `Dev-A` | `Dev-B` | `Dev-C` | `Dev-D` |
| --- | --- | --- | --- | --- |
| 第 1 阶段 | `M1-S01`~`M1-S12` | `M2-S01`~`M2-S09` 先做协议/路由 | `M3-S01`~`M3-S11` 先做 stats 底座 | `M0-S01`~`M0-S30` + `M1-S13`~`M1-S22` |
| 第 2 阶段 | `M4-S01`~`M4-S13` | `M2-S13`~`M2-S28` | `M3-S12`~`M3-S31` | `M8-S01`~`M8-S11` |
| 第 3 阶段 | `M4-S14`~`M4-S27` + `M5-S17`~`M5-S27` | `M5-S01`~`M5-S08` + `M6-S01`~`M6-S12` | `M5-S09`~`M5-S16` + `M7-S01`~`M7-S10` | `M8-S12`~`M8-S21` |
| 第 4 阶段 | `M5-S28`~`M5-S32` + `M7-S21`~`M7-S26` | `M6-S13`~`M6-S24` | `M7-S11`~`M7-S20` | `M8-S22`~`M8-S29` + 全量回归 |

#### 6.14.14 冲突规避规则

- `src/server/mod.rs` 只允许 `Dev-A` 做最终合并改动
- `src/streaming/*`、`src/shim/*` 只允许 `Dev-B` 主改
- `src/runtime/*`、`src/cgroups/*`、`src/metrics/*` 只允许 `Dev-C` 主改
- `src/image/*`、`tests/*` 只允许 `Dev-D` 主改
- 跨模块需求通过“先提 helper API，再由主责人接线”的方式协作
- 每周至少一次由 `Dev-D` 跑全量 smoke，用来发现集成回归
- 任何新增 proto 契约、事件字段、stream header，都必须先在文档里补一句说明再开写

#### 6.14.15 4 人版本的最优组合

如果目标是尽快打到 P0，可优先保证下面这组组合先完成:

- `Dev-A`: `M1` + `M4`
- `Dev-B`: `M2` + `M6`
- `Dev-C`: `M3` + `M5-S09`~`M5-S16`
- `Dev-D`: `M0` + `M1` 的 image 部分 + `M8` 中 `ImageStatus/RemoveImage/ImageFsInfo`

这样最先闭环的是:

- `port-forward`
- `stats/statsp/metricsp`
- `events`
- stop/remove 幂等语义
- image 基础契约修正

## 7. 最终判断

与 `/root/cri-o` 对比后，Crius 当前最接近的定位不是“CRI-O 级别的完整 CRI runtime”，而是:

> 一个已经能跑通基础 `crictl` 生命周期和基本流式调试，但在观测、幂等、日志契约、端口转发、统计指标、事件流、重启恢复等方面仍明显落后于 CRI-O 的实现。

如果目标是“能用 `crictl` 正常调试容器”，Crius 已经过了最初可用线。

如果目标是“像 CRI-O 一样长期稳定接 kubelet / 运维工具链”，则 P0 和 P1 里的差距仍然必须补齐。
