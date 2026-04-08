# Crius 对接 `crictl` 实现现状报告

## 1. 范围与基线

- 分析日期: `2026-04-08`
- 对照客户端: `crictl v1.29.0`
- 复核基线: `273e53a` `feat: implement exec streaming and tty handling`
- 复核对象: 当前工作区代码实现
- 当前工作区状态: 除本文档外，代码实现与 `HEAD` 一致；`.codex/`、`.tmp/` 为本地临时目录

本报告不再沿用旧版“逐轮追加补丁”的写法，而是直接基于当前代码重整为一份“现状说明 + 能力矩阵 + 明确待办”文档。

## 2. 核心结论

当前 Crius 已经具备 CRI Runtime/ImageService 的主框架，并且以下主路径已经达到“可实际使用”的水平：

- Pod 沙箱生命周期 `runp / pods / inspectp / stopp / rmp`
- 容器基础生命周期 `create / start / stop / rm / inspect`
- 流式执行 `exec -s / exec / exec -i / exec -it`
- `attach` 的 fresh live container 主路径

但距离“像 containerd / CRI-O 一样完整支持 `crictl` 调试”仍有明显差距，当前最主要的剩余问题集中在：

- `attach` 的 `resize / exit-status / 重启恢复`
- `port-forward`
- `logs` 的 `ReopenContainerLog` 与完整契约验证
- `stats / statsp / metricsp`
- `events`
- `update / checkpoint / runtime-config / imagefsinfo`
- daemon 重启后的 live 流式与观测语义恢复

一句话总结当前状态：

> Crius 已经不是“只有骨架的 CRI 原型”，而是“Pod/Container 基础生命周期可用、exec/attach 主路径已打通，但观测、控制面和恢复语义仍明显不足”的实现。

## 3. 关键实现入口

代码主入口集中在以下文件：

- `src/main.rs`
  - daemon 启动
  - runtime handler / pause image 初始化
  - streaming server 启动
- `src/server/mod.rs`
  - RuntimeService 主要 RPC 实现
  - 持久化状态恢复
  - CRI 请求到内部 runtime/pod/image 的映射
- `src/pod/mod.rs`
  - Pod sandbox 管理
  - pause 容器创建
  - netns / Pod 网络状态探测
- `src/runtime/mod.rs`
  - `runc` 适配层
  - OCI spec 生成
  - 容器创建、启动、停止、删除、exec
- `src/streaming/mod.rs`
  - `/exec/:token` 与 `/attach/:token`
  - SPDY 协议握手
  - `exec` / `attach` 数据面桥接
- `src/shim/daemon.rs`
  - shim 启动与容器前台/TTY 管理
- `src/shim/io.rs`
  - attach socket
  - stdout/stderr 日志落盘
  - console bridge
- `src/image/mod.rs`
  - ImageService 实现

## 4. 能力矩阵

评级标准：

- `L3`: 主链路闭环，字段与语义基本正确
- `L2`: 基础可用，但还缺部分字段、边界能力或恢复语义
- `L1`: 能调通，但返回值/语义明显不完整
- `L0`: 占位、空实现或关键链路未接通

### 4.1 Pod 沙箱命令

| 命令 | 评级 | 现状 |
| --- | --- | --- |
| `version` | `L3` | 已实现，足以支撑 `crictl version` |
| `info` | `L1` | 可返回结果，但健康状态基本为硬编码 |
| `runp` | `L3` | 已接通 Pod 创建、pause 容器、runtime handler 校验、安全上下文与持久化 |
| `pods` | `L3` | 已支持 metadata/labels/runtime_handler 等列表展示与恢复回填 |
| `inspectp` | `L3` | 已回填 `runtimeHandler`、`linux.namespaces`、`logDirectory`、`netnsPath`、`pauseContainerId`、网络状态 |
| `stopp` | `L3` | 已停止 Pod 与 Pod 内业务容器，并更新状态与退出码 |
| `rmp` | `L3` | 已级联清理容器、Pod、持久化与 fallback netns |

### 4.2 容器生命周期与查询命令

| 命令 | 评级 | 现状 |
| --- | --- | --- |
| `create` | `L2` | 关键 CRI 字段已开始落地，包括 log path、resources、设备、namespace path |
| `run` | `L2` | 依赖 `runp + create + start`，基础可用 |
| `start` | `L2` | 基础可用 |
| `stop` | `L2` | 基础可用，退出码语义已有改进 |
| `rm` | `L2` | 基础可用 |
| `ps` | `L1` | 可列出，但仍偏内存快照，实时性不足 |
| `inspect` | `L2` | 已回填 `startedAt/finishedAt/exitCode/logPath/resources`，但 `reason/message/mounts/verbose` 仍缺 |

### 4.3 流式与日志命令

| 命令 | 评级 | 现状 |
| --- | --- | --- |
| `exec --sync` | `L2` | 已返回真实 `stdout/stderr/exit_code`，且 timeout 生效 |
| `exec` | `L2` | `stdout/stderr/stdin/tty` 主路径已打通，TTY 现在走本地 PTY 桥接；高版本 remotecommand 语义仍未补齐 |
| `attach` | `L2` | fresh live container 主路径可用，但还缺 `resize / exit-status / error / 重启恢复` |
| `logs` | `L1` | host-file 日志链路已存在，但 `ReopenContainerLog` 与完整契约仍缺 |
| `port-forward` | `L0` | 仍是占位返回 |

### 4.4 镜像命令

| 命令 | 评级 | 现状 |
| --- | --- | --- |
| `pull` | `L2` | 基础拉取可用，元数据与认证语义不完整 |
| `images` | `L1` | 可列出，但无 filter、可能重复 |
| `inspecti` | `L1` | 已命中可查，未命中仍错误返回 `NotFound` |
| `rmi` | `L1` | 能删除目录，但未接层引用计数/GC |
| `imagefsinfo` | `L0` | 空返回 |

### 4.5 观测与控制面命令

| 命令 | 评级 | 现状 |
| --- | --- | --- |
| `stats` | `L0` | 空返回 |
| `statsp` | `L0` | 空返回 |
| `metricsp` | `L0` | 空返回 |
| `events` | `L0` | 不是持续事件流，且事件类型值使用错误 |
| `update` | `L0` | 仅占位，未调用 `runc update` |
| `checkpoint` | `L0` | 空实现 |
| `runtime-config` | `L0/L1` | 可调用，但返回近乎空配置 |

## 5. 分模块分析

### 5.1 Pod 沙箱路径

主要入口：

- `src/server/mod.rs`
  - `run_pod_sandbox`
  - `stop_pod_sandbox`
  - `remove_pod_sandbox`
  - `pod_sandbox_status`
  - `list_pod_sandbox`
- `src/pod/mod.rs`
  - `create_pod_sandbox`
  - `create_pause_container`
  - `stop_pod_sandbox`
  - `remove_pod_sandbox`

当前已完成的内容：

- `runtime_handler` 已支持配置化列表，而不再只接受单一默认值
- Pod `SELinux / seccomp` 已下传到 pause 容器与 OCI spec
- Pod 创建时会 best-effort 探测 netns 地址并回填 `inspectp.network`
- `inspectp` 已补充 `runtimeHandler / linux.namespaces / logDirectory / netnsPath / pauseContainerId`
- `stopp` 已回写 Pod 内容器停止状态，而不是固定写 `Stopped(0)`
- `rmp` 已支持恢复后级联清理

当前仍缺：

- 真正多 runtime backend 调度
- 更稳定的网络可见性回填
- `stopp -> inspectp` 仍可能存在极短的状态收敛窗口

### 5.2 容器生命周期与状态语义

主要入口：

- `src/server/mod.rs`
  - `create_container`
  - `start_container`
  - `stop_container`
  - `remove_container`
  - `container_status`
  - `list_containers`
- `src/runtime/mod.rs`
  - `create_container`
  - `start_container`
  - `stop_container`
  - `remove_container`
  - `container_status`
  - `create_spec`

当前已完成的内容：

- OCI spec 已接入 `tty`、只读 rootfs、capabilities、apparmor、seccomp、selinux、sysctls、resources、devices
- `namespace_options` 的 `pid/ipc` 共享语义已开始真正落地
- `run_as_username` 已能回退到用户名路径
- `ContainerStatus` 已回填 `startedAt/finishedAt/exitCode/logPath/resources`

当前仍缺：

- `inspect` 的 `reason/message/mounts/verbose`
- `ps` 的实时状态刷新
- 更强的一致性退出码来源

### 5.3 `exec` / `attach` / `logs` / `port-forward`

主要入口：

- `src/server/mod.rs`
  - `exec`
  - `exec_sync`
  - `attach`
  - `port_forward`
  - `reopen_container_log`
- `src/streaming/mod.rs`
  - `StreamingServer::start`
  - `get_exec`
  - `get_attach`
  - `serve_exec_spdy`
  - `serve_attach_spdy`
- `src/shim/io.rs`
  - `start_attach_server`
  - `write_stdout`
  - `write_stderr`
  - `start_console_bridge`

#### `exec --sync`

当前状态：

- 已返回真实 `stdout/stderr/exit_code`
- `timeout` 已生效，超时后返回 `DeadlineExceeded`
- 运行时侧新增了统一的 exec 命令构造与输出捕获能力

#### `exec`

当前状态：

- `/exec/:token` 已不再是 `501`
- 已完成 SPDY 握手、stream 协商、`stdin/stdout/stderr` 桥接
- `tty=false` 时走普通 pipe
- `tty=true` 时已改为本地 PTY 桥接，而不是错误依赖 `runc exec -i`
- `exec -s / exec / exec -i / exec -it` 的 fresh live container 主路径已打通

当前仍缺：

- `v3+/resize` 等更高阶 remotecommand 语义
- 更完整的 error/status 组织方式

#### `attach`

当前状态：

- 已完成 SPDY upgrade、streamtype 识别、shim `attach.sock` 桥接
- non-TTY stdout/stderr 分流已可用
- fresh live container 主路径已经可用

当前仍缺：

- resize
- exit-status / error 细化语义
- daemon 重启后的 attach 恢复

#### `logs`

当前状态：

- `CreateContainer` 已处理 `log_path`
- `RunPodSandbox` 已处理 `log_directory`
- shim `IoManager` 已会把 stdout/stderr 写入日志文件
- `ContainerStatus.log_path` 已能返回真实路径

当前仍缺：

- `ReopenContainerLog`
- 日志轮转/重开语义
- 专门面向 `crictl logs` 的契约验证

#### `port-forward`

当前状态：

- 仍是固定 URL 占位

### 5.4 ImageService

主要入口：

- `src/image/mod.rs`
  - `load_local_images`
  - `list_images`
  - `image_status`
  - `pull_image`
  - `remove_image`
  - `image_fs_info`

当前已完成的内容：

- 本地镜像加载
- 基础镜像拉取
- 基础镜像查询与删除

当前仍缺：

- `pull` 的认证字段支持不完整
- `images` 无 filter 且可能重复
- `inspecti` 未命中时返回语义错误
- `rmi` 未接 `LayerManager`
- `imagefsinfo` 未实现

### 5.5 观测与控制面

当前状态：

- `MetricsCollector` 已存在，但 `stats / statsp / metricsp` 尚未接入
- `events` 目前只是把当前内存状态扫一遍发出去，不是持续流
- `update` 仅占位
- `checkpoint` 仅占位
- `runtime-config` 仅返回默认空配置

这部分仍是当前最大的 `L0` 集中区。

### 5.6 恢复语义

主要入口：

- `src/server/mod.rs`
  - `recover_state`
  - `build_container_status_snapshot`
  - `pod_network_status_from_state`

当前已恢复的内容：

- `containers` 顶层内存对象
- `pod_sandboxes` 顶层内存对象
- `pod_manager` 内部 Pod 记录
- Pod `runtime_handler`
- `netns_path`
- `pause_container_id`
- Pod `ip/additional_ips`
- 部分容器时间戳与退出码

当前仍未恢复的内容：

- live shim 会话
- attach socket 可用性
- 日志 reopen/文件句柄状态
- network manager 的瞬时运行态
- 持续事件流与观测态

## 6. 本轮直接验证结果

### 6.1 已直接验证

本轮直接通过真实 `crictl` 验证了以下路径：

1. Pod 沙箱生命周期
   - `runp`
   - `pods`
   - `inspectp`
   - `stopp`
   - `rmp`

2. `exec`
   - `exec -s`
   - `exec`
   - `exec -i`
   - `exec -it`

验证方式均使用隔离 socket，避免系统默认 `/run/crius/crius.sock` 干扰。

### 6.2 本轮已确认的结论

- `runp/pods/inspectp/stopp/rmp` 当前已形成闭环
- `exec -s` 现在确实会返回 `stdout/stderr`
- `exec -s --timeout` 现在确实会超时失败
- streaming `exec` 当前已能处理非 TTY、stdin、TTY shell 进入
- `exec -it /bin/bash` 的终端错位与退出卡住问题，本轮已按 CRI-O 风格改成 PTY 方案并完成自动化回归

### 6.3 本轮未重新实测

以下结论本轮主要基于当前代码与既有实现判断，未重新逐项实机复测：

- `attach` 的全部历史路径
- `logs`
- 镜像子命令
- `events / stats / metricsp / update / checkpoint / runtime-config`

## 7. 当前最值得做的事

### P0

1. 完成 `attach`
   - 补 `resize`
   - 补 `exit-status / error`
   - 明确并改进 daemon 重启后的 attach 语义

2. 完成 `logs`
   - 实现 `ReopenContainerLog`
   - 补 `crictl logs` 契约验证

3. 完成 `port-forward`
   - 从占位返回升级为真实转发链路

### P1

1. 接入 `MetricsCollector`
   - `stats`
   - `statsp`
   - `metricsp`

2. 改造 `events`
   - 接入真实事件源
   - 修正事件类型值

3. 收敛查询一致性
   - `ps`
   - `inspect`
   - `inspectp`

### P2

1. 完成镜像命令族
   - `images`
   - `inspecti`
   - `rmi`
   - `imagefsinfo`

2. 完成控制面占位能力
   - `update`
   - `checkpoint`
   - `runtime-config`

### P3

1. 提升 daemon 重启恢复
   - live shim
   - attach
   - logs reopen
   - network manager 运行态

2. 增强高级能力
   - 多 runtime backend
   - `CDI_devices`
   - 更稳定的网络可见性

## 8. 结论

当前 Crius 的状态可以概括为：

- Pod sandbox 生命周期已达到 `L3`
- 容器基础生命周期已达到 `L2`
- `exec` 与 `attach` 已不再是占位，而是进入“主路径可用”的阶段
- 日志、镜像、观测、事件、控制面、恢复语义仍有明显缺口

因此，如果目标是“可以用 `crictl` 做基础调试和生命周期验证”，当前实现已经具备相当实用性。

但如果目标是“像 containerd / CRI-O 一样完整、稳定、可观测”，接下来最值得投入的仍然是：

- `attach`
- `port-forward`
- `logs`
- `stats / metricsp / events`
- daemon 重启后的流式与观测恢复
