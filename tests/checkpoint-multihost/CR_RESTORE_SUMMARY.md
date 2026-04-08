# Checkpoint/Restore Multi-host 测试总结

## 1) 本次测试目标

- 验证 Shadow 在多 host 场景下的 checkpoint/restore 能力是否闭环：
  - host 运行态（进程/线程与调度推进）是否可恢复；
  - 外部 DB 依赖是否可在 restore 后与 checkpoint 时刻一致；
  - restore 后是否能继续推进仿真时间并产生预期业务增量。
- 验证失败路径是否可控：
  - 避免 `shim_shmem_lock.is_none()` 这类二次崩溃掩盖主因；
  - 失败时输出单一主错误，避免“半恢复污染”。

## 2) 主要困难

- **语义误区（native pid 稳定性）**
  - 初期把 checkpoint 中的 `native_pid` 当作稳定身份，导致 Phase1 误杀。
  - 实际上 CRIU restore 可能返回新 PID，需要重绑定。

- **restore pipeline 阶段错位**
  - `build_host` 阶段触发 `Process::from_checkpoint`，会构造依赖 `Worker::with_active_host` 的 descriptor/listener/eventsource。
  - 但该阶段早于 Worker TLS 初始化，导致 `with_active_host(None)` 触发崩溃。

- **恢复后无任务推进**
  - 即使 restore + DB 回滚成功，post-restore `continue_for` 初期出现计数不增长。
  - 根因是恢复后的运行进程缺少立即可执行的 resume 触发，事件队列仅有定时类任务。

- **退出路径噪声**
  - 出现 `Dropped without calling explicit_drop`，虽不一定影响功能正确性，但污染测试输出。

## 3) 解决方案

### A. 两阶段 restore 收敛

- Phase1（前置校验）：
  - 校验 native 状态可绑定（pid 可枚举、handle 可反序列化、event bytes 尺寸一致）。
  - 失败直接 fail fast，不进入对象图恢复。

- Phase2（对象恢复）：
  - 将 `apply_host_checkpoint` 从 `build_host` 移到 Worker TLS 初始化之后执行。
  - 避免在无 active host 语境下构造 descriptor/listener/eventsource。

### B. CRIU 侧收敛

- restore 使用 `--restore-sibling`，避免 restore 后进程立刻因父子关系退出导致“看似没恢复”。

### C. 恢复后推进保障

- 在 host checkpoint replay 后，对 running process 注入一次 `ResumeProcess` 任务（按 checkpoint cpu_now 时间），确保恢复出的 workload 能继续推进。

### D. clean exit 收尾

- verify 场景退出时不再直接 terminate Shadow；
- 改为自动继续运行到仿真自然结束，拿到 `shadow exit code: 0`；
- 对 release 构建下的 `explicit_drop` 报错降为 debug 级别，避免误导性错误噪声。

## 4) 当前 C/R 整体 flow（多 host）

1. 正常运行到 checkpoint 时间（例：10s）。
2. 记录并备份外部 DB。
3. 发起 checkpoint：
   - 序列化 host/process/thread/task 元数据；
   - 执行 CRIU dump（保留运行）。
4. 继续运行到分叉时间（例：20s）以制造差异。
5. restore 前恢复外部 DB 到 checkpoint 备份。
6. 发起 restore：
   - 恢复 shmem 文件；
   - CRIU restore 进程树并更新 checkpoint 中 pid；
   - 进入新一轮 manager 初始化。
7. Worker TLS 初始化完成后，按 host 执行 checkpoint replay：
   - 重建 process/thread 对象；
   - 重建事件队列与 host 计数器；
   - 注入一次 resume kick 保障继续推进。
8. post-restore 继续运行（例：+5s），验证：
   - DB 先回到 checkpoint 值；
   - 再按时间继续增长；
   - 多 host 一致通过。
9. verify 完成后继续到模拟自然结束并 clean exit。

## 5) 验收结论

- 多 host verify 已可同时覆盖：
  - host checkpoint/restore 基本恢复能力；
  - 外部 DB 回滚一致性；
  - restore 后继续推进正确性；
  - clean exit（exit code 0）。

