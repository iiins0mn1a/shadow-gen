# Ethereum-like Network Checkpoint/Restore PoC

这个 PoC 不实现真实以太坊协议，而是覆盖更接近测试网骨架的网络形态：

- `bootnode` 接收多 peer 的 TCP 邻居连接
- `peer-a/peer-b/peer-c` 维持多条长期 TCP 会话
- UDP 侧有 discovery-like `discover_ping/discover_pong`
- restore 后要求多 peer 持续推进，而不是单条连接偶发恢复

相比 `checkpoint-network-multihost`，这个 PoC 额外覆盖：

- 多条并发长期 TCP 邻居连接
- bootnode fan-in
- 多 peer 的 UDP discovery-like 往返
- stepped post-restore 时间窗口中的连续推进

运行方式：

```bash
CRIU_BIN=/path/to/criu \
python3 tests/checkpoint-network-eth-poc/orchestrator_verify.py \
  --shadow-bin build/src/main/shadow \
  --config tests/checkpoint-network-eth-poc/shadow_eth_poc.yaml \
  --work-dir tests/checkpoint-network-eth-poc/run \
  --clean-data \
  --verify-label cp_eth_poc_verify
```

当前判定标准：

- warmup 阶段每个 peer 都有 `tcp_tx/tcp_rx_ack/udp_rx_ack`
- bootnode 在 warmup 和 post-restore 阶段都能从多个 peer 收到 TCP/UDP 流量
- restore 后所有 peer 继续推进，不出现 TCP reconnect churn
- restore 后各 peer 的应用 `mono_ns` 跨 stepped window 持续推进

现状判断：

- 这个 PoC 比 `checkpoint-network-multihost` 更接近“一个进程维护多邻居连接”的测试网骨架
- 如果它失败，而当前 `checkpoint-network-multihost` 仍通过，通常说明 Shadow 现在的网络 restore 还不足以覆盖更复杂的多 socket / 多邻居进程模型
