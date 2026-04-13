# checkpoint-network-multihost 任务交接（给下一位 agent）

## 1. 任务目标（当前定义）

在 `tests/checkpoint-network-multihost` 场景下，验证并修复 Shadow 的 checkpoint/restore 网络语义，要求：

- restore 后多节点 **UDP 双向通信恢复**；
- restore 后 TCP client/server 语义恢复，不依赖 `net_app.py` 自愈重建；
- 优先 Shadow 内核侧修复（而不是应用层 workaround）；
- 最终通过 `orchestrator_verify.py` 的自动验收。

---

## 2. 当前最关键挑战（已验证）

### 挑战 A：真正主阻塞点是 Layer 0（进程再驱动）

现象一致指向“阻塞语义恢复失败”：

- restore 后 UDP/TCP 进程长期处于 `futex_wait_queue`；
- TCP client restore 后日志 `mono_ns` 出现冻结/不前进；
- 单纯修 socket/event source（Layer 1-5 中 1-4）无法恢复流量；
- 一次性 EINTR（单次去阻塞）已实测不足，线程会重入旧等待路径。

结论：L1-L5 模型不够，必须加入 **Layer 0: 进程可被 Shadow 再驱动**。

### 挑战 B：直接改 `ProcessShmem` 结构有高风险

已尝试把 generation counter 放进 `ProcessShmem`，结果出现严重回归：

- warmup 阶段即全进程 `SIGABRT`；
- `sim_time_ns` 出现异常值；
- 说明共享内存 ABI/初始化路径非常敏感，不能直接扩展关键结构体字段后就上线。

当前已回滚该原型，恢复到可运行基线。

### 挑战 C：TCP 语义恢复仍不完整

已知：

- listener fixup 可以 `listen()` 成功；
- client fixup 可触发 `connect()` 并返回 `EINPROGRESS`；
- 但 server 侧仍可见 `ENOTCONN`，说明 accepted child 连接语义未闭环。

---

## 3. 项目结构速览（只列本任务核心）

### 测试与编排层

- `tests/checkpoint-network-multihost/shadow_network.yaml`
  - 两主机四进程拓扑定义（tcp_client/tcp_server/udp_peer_a/udp_peer_b）。
- `tests/checkpoint-network-multihost/net_app.py`
  - 测试应用（保持“无自愈”版本），输出 `NETLOG`。
- `tests/checkpoint-network-multihost/orchestrator_verify.py`
  - 自动化流程：warmup -> checkpoint -> advance -> restore -> post-restore 验证。
  - 已加入多种诊断：进程状态、`/proc/*/net/tcp`、`ss -tanp`、子进程状态采样等。

### Shadow restore 核心路径

- `src/main/core/manager.rs`
  - `apply_host_checkpoint()`：restore 主入口；
  - `post_restore_socket_fixup`：restore 后 socket bind/listen/connect 补偿；
  - `ResumeProcess` 调度（含日志/多次 nudge）。
- `src/main/host/managed_thread.rs`
  - `from_checkpoint()`：恢复 `current_event`；
  - `resume()`：线程恢复执行；
  - 已做“强制一次 EINTR”实验及诊断日志。
- `src/main/host/process.rs`
  - descriptor snapshot/replay；
  - socket descriptor 重建逻辑（UDP/TCP）。

### Checkpoint schema

- `src/main/core/checkpoint/snapshot_types.rs`
  - `ProcessCheckpoint.descriptors`（fd 级元数据）；
  - `DescriptorSocketTransport`、local/peer/listen 等字段。

---

## 4. 已完成/已验证事实（避免重复踩坑）

1. 仅依赖 old/inherited descriptor table 路线不可行，checkpoint descriptor 元数据才是可靠来源。
2. restore 时 socket fixup 已可执行，且能看到 bind/listen/connect 返回值（包含 `EINVAL/EADDRINUSE/EINPROGRESS`）。
3. 一次性 EINTR 真实触发过，但不能根治 Layer 0。
4. restore 后 `current_event` 真实是 syscall 事件（如 `pselect6`/`recvfrom` 类），与“旧阻塞语义重入”一致。
5. 直接在 `ProcessShmem` 加 generation 字段导致 warmup 崩溃（已回滚）。

---

## 5. 推荐下一步（给新 agent 的执行顺序）

1. **先做 Layer 0 的“非侵入 generation 通道”**
   - 避免直接改 `ProcessShmem` 内存布局；
   - 使用现有 IPC/control 路径或独立轻量状态通道传递 restore epoch；
   - 保留一次唤醒（EINTR/等效机制）作为触发器；
   - 目标：防止 shim 从唤醒后重入旧等待路径。

2. **Layer 0 验证基准**
   - restore 后进程不再长期停在 `futex_wait_queue`；
   - `mono_ns` 连续推进；
   - UDP 至少恢复 `start/tx/rx` 事件增长。

3. **再做 TCP listener child 注入**
   - 基于地址路由模型，不引入 ConnectionRegistry；
   - 恢复 listener 的 `conn_map + accept_queue`；
   - 关注 `associate_socket` 前置条件，规避重复绑定导致的 `EADDRINUSE/EINVAL`。

4. **最终回归**
   - `tests/checkpoint-network-multihost/orchestrator_verify.py` 必过；
   - 同时跑 `tests/checkpoint-multihost` 回归，确认不破坏已有 checkpoint 能力。

---

## 6. 常用命令（复现/验证）

```bash
cmake --build build -j4 --target shadow

CRIU_BIN=/home/ins0/workspace-for-agent/user_data/task/criu_demo/criu-src/criu/criu \
python3 tests/checkpoint-network-multihost/orchestrator_verify.py \
  --shadow-bin build/src/main/shadow \
  --config tests/checkpoint-network-multihost/shadow_network.yaml \
  --work-dir tests/checkpoint-network-multihost/run \
  --clean-data \
  --verify-label cp_network_verify
```

---

## 7. 当前状态总结（一句话）

**当前主要瓶颈不是 socket 对象本身，而是 restore 后 shim/Shadow 阻塞语义再同步（Layer 0）未闭环；TCP child 注入应在 Layer 0 稳定后推进。**
