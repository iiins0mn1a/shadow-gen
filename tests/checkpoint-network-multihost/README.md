# Multi-host Network Checkpoint/Restore Test

这是当前最小的网络 cp/restore 验收：

- 一个长期 TCP 连接
- 一对双向 UDP 心跳
- restore 后 TCP 继续推进且不重连
- restore 后 UDP 双向继续收发

文件：

- `orchestrator_verify.py`: 验证脚本
- `net_app.py`: TCP/UDP 角色程序
- `shadow_network.yaml`: TCP+UDP 混合拓扑
- `shadow_tcp_only.yaml`: TCP-only 拓扑

运行：

```bash
python3 tests/checkpoint-network-multihost/orchestrator_verify.py \
  --shadow-bin build/src/main/shadow \
  --config tests/checkpoint-network-multihost/shadow_network.yaml \
  --work-dir tests/checkpoint-network-multihost/run \
  --clean-data \
  --verify-label cp_network_verify
```

如果需要显式指定 CRIU：

```bash
CRIU_BIN=/path/to/criu \
python3 tests/checkpoint-network-multihost/orchestrator_verify.py \
  --shadow-bin build/src/main/shadow \
  --config tests/checkpoint-network-multihost/shadow_network.yaml \
  --work-dir tests/checkpoint-network-multihost/run \
  --clean-data

# 如需打开恢复阶段的重诊断输出（/proc tcp、子进程状态、短 strace）：
# 追加 --diagnostics
```

如果只想看 TCP，可使用 `--mode tcp`。它会在 restore 后按小步推进，检查 TCP 是否持续推进：

```bash
CRIU_BIN=/path/to/criu \
python3 tests/checkpoint-network-multihost/orchestrator_verify.py \
  --shadow-bin build/src/main/shadow \
  --config tests/checkpoint-network-multihost/shadow_tcp_only.yaml \
  --work-dir tests/checkpoint-network-multihost/run \
  --clean-data \
  --mode tcp \
  --post-restore-step-ns 1000000000 \
  --post-restore-steps 10
```

如需把应用层 `time.monotonic_ns()` 也纳入门禁，可追加 `--strict-tcp-time`。

判定重点：

- warmup 和 advance 阶段都必须已有通信
- restore 后 TCP 继续推进且没有 reconnect churn
- restore 后 UDP 双向继续推进
- TCP-only 模式下还要求 post-restore 多 step 持续推进
