# Shadow以太坊测试网性能优化最终建议

## 🎉 重大发现

**shadow-time已经实现了共享内存时间优化！**

在 `/home/ins0/Repos/all-shadows/shadow-time/src/lib/shadow-shim-helper-rs/src/shim_shmem.rs`:
```rust
pub struct HostShmem {
    // Current simulation time.
    pub sim_time: AtomicEmulatedTime,  // ✅ 已经在共享内存中！
    
    pub unblocked_vdso_latency: SimulationTime,  // 默认10ns
    pub max_unapplied_cpu_latency: SimulationTime,  // 默认1μs
}
```

---

## 🤔 核心问题

**如果已经有共享内存时间，为什么clock_gettime仍占96.53%？**

可能原因：

### 1. vDSO补丁可能未生效

**检查方法**：
```bash
cd /home/ins0/Repos/shadow-gen/MyTest
rm -rf shadow.data
RUST_LOG=trace /home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow shadow.yaml 2>&1 | grep -i "vdso\|patch" | head -20
```

查找日志中是否有：
- "Successfully patched vDSO"
- "Failed to patch vDSO"
- "vDSO not found"

### 2. 应用程序可能绕过了vDSO

Go程序可能使用直接syscall而不是libc的`clock_gettime`：
```go
// Go runtime可能这样调用
syscall.Syscall(syscall.SYS_clock_gettime, ...)
```

**验证方法**：
检查strace日志中是否都是syscall而非vDSO调用。

### 3. 配置参数需要调优

当前默认配置可能对于以太坊测试网不够激进。

---

## 🚀 推荐优化配置

### 方案A：激进优化（最大性能）

在`shadow.yaml`中添加experimental选项：

```yaml
general:
  stop_time: 3m
  log_level: warning
  parallelism: 16
  progress: false
  model_unblocked_syscall_latency: false  # ⚠️ 禁用syscall延迟建模

experimental:
  max_unapplied_cpu_latency: 100 ms       # 🔥 大幅增加
  unblocked_syscall_latency: 1 ns         # 🔥 降低到最小
  unblocked_vdso_latency: 1 ns            # 🔥 降低到最小
```

**预期效果**：
- 减少时间更新频率
- 最大化批处理效益
- 牺牲一点准确性换取极致性能

### 方案B：平衡优化（推荐）

```yaml
general:
  stop_time: 3m
  log_level: warning
  parallelism: 16
  progress: false
  model_unblocked_syscall_latency: false

experimental:
  max_unapplied_cpu_latency: 10 ms        # 适度增加
  unblocked_syscall_latency: 10 ns       # 降低延迟
  unblocked_vdso_latency: 5 ns           # 略微降低
```

### 方案C：调试模式（验证vDSO是否工作）

```yaml
general:
  stop_time: 30s                          # 短时间测试
  log_level: debug                        # 🔍 详细日志
  parallelism: 1                          # 单线程便于调试
  progress: true
```

运行并检查日志：
```bash
/home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow shadow.yaml 2>&1 | grep -i "vdso\|clock_gettime\|patch" | tee vdso_debug.log
```

---

## 📋 立即行动计划

### 步骤1：验证vDSO状态

```bash
cd /home/ins0/Repos/shadow-gen/MyTest

# 创建调试配置
cat > shadow_debug.yaml << 'EOF'
general:
  stop_time: 30s
  log_level: debug
  parallelism: 1
  progress: false

network:
  graph:
    type: gml
    inline: |
      graph [
        directed 0
        node [
          id 0
          host_bandwidth_up "1000 Gbit"
          host_bandwidth_down "1000 Gbit"
        ]
        edge [
          source 0
          target 0
          latency "1 us"
          packet_loss 0.0
        ]
      ]

hosts:
  test-node:
    network_node_id: 0
    processes:
    - path: /usr/bin/sleep
      args: 10
      start_time: 1
EOF

# 运行调试测试
/home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow shadow_debug.yaml 2>&1 | tee vdso_debug.log

# 检查vDSO信息
grep -i "vdso\|patch" vdso_debug.log
```

### 步骤2：应用激进优化配置

如果vDSO正常工作，更新`shadow.yaml`：

```bash
cd /home/ins0/Repos/shadow-gen/MyTest
cp shadow.yaml shadow.yaml.backup

cat >> shadow.yaml << 'EOF'

experimental:
  max_unapplied_cpu_latency: 100 ms
  unblocked_syscall_latency: 1 ns
  unblocked_vdso_latency: 1 ns
EOF
```

### 步骤3：测试性能提升

```bash
cd /home/ins0/Repos/shadow-gen/MyTest
echo "=== 优化前（基线） ==="
grep "Elapsed" optimized_baseline.log

echo ""
echo "=== 测试优化后性能 ==="
rm -rf shadow.data
/usr/bin/time -v /home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow shadow.yaml 2>&1 | tee final_optimized.log

echo ""
echo "=== 性能对比 ==="
echo -n "优化前: "
grep "Elapsed" optimized_baseline.log
echo -n "优化后: "
grep "Elapsed" final_optimized.log

# 计算加速比
echo ""
python3 << 'PY'
import re

def get_time(filename):
    with open(filename) as f:
        content = f.read()
        match = re.search(r'Elapsed.*?(\d+):(\d+\.\d+)', content)
        if match:
            mins, secs = float(match.group(1)), float(match.group(2))
            return mins * 60 + secs
    return None

before = get_time('optimized_baseline.log')
after = get_time('final_optimized.log')

if before and after:
    speedup_before = 180 / before
    speedup_after = 180 / after
    improvement = (speedup_after / speedup_before - 1) * 100
    
    print(f"运行时间对比:")
    print(f"  优化前: {before:.2f}秒 (加速比 {speedup_before:.2f}x)")
    print(f"  优化后: {after:.2f}秒 (加速比 {speedup_after:.2f}x)")
    print(f"  性能提升: {improvement:+.1f}%")
    print(f"  加速比提升: {speedup_before:.2f}x → {speedup_after:.2f}x")
PY
```

### 步骤4：验证strace数据

```bash
# 短时间strace测试
cp shadow.yaml shadow_short.yaml
sed -i 's/stop_time: 3m/stop_time: 30s/' shadow_short.yaml

echo "=== 运行strace分析 ==="
rm -rf shadow.data
timeout 60 /home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow --strace-logging-mode=standard shadow_short.yaml

echo ""
echo "=== 分析优化后的syscall分布 ==="
python3 analyze_strace.py shadow.data/hosts/prysm-beacon-1/beacon-chain.1000.strace

# 对比优化前后
echo ""
echo "=== 对比结果 ==="
echo "优化前: clock_gettime占96.53%"
echo -n "优化后: "
python3 analyze_strace.py shadow.data/hosts/prysm-beacon-1/beacon-chain.1000.strace 2>/dev/null | grep "时间相关syscall"
```

---

## 🎯 预期结果

### 如果vDSO正常工作

**保守估计**：
- 运行时间：28秒 → 10-15秒
- 加速比：6.38x → 12-18x
- clock_gettime占比：96.53% → 50-70%

**理想情况**：
- 运行时间：28秒 → 3-6秒
- 加速比：6.38x → 30-50x
- clock_gettime占比：96.53% → <10%

### 如果vDSO未工作

需要进一步调试：
1. 检查Go程序是否直接syscall
2. 验证shim库是否正确加载
3. 检查vDSO补丁机制

---

## 🔍 故障排查

### 问题1：性能无明显提升

**可能原因**：
- vDSO补丁未生效
- Go程序绕过了vDSO
- 配置参数未被应用

**解决方案**：
```bash
# 检查配置是否生效
/home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow --show-build-config

# 检查日志
RUST_LOG=shadow_rs=debug /home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow shadow.yaml 2>&1 | grep -A5 -B5 "clock_gettime\|vdso"
```

### 问题2：模拟结果异常

**可能原因**：
- 时间粒度太粗，影响共识
- 事件处理顺序变化

**解决方案**：
- 降低`max_unapplied_cpu_latency`
- 启用`model_unblocked_syscall_latency`
- 对比日志验证正确性

### 问题3：strace显示clock_gettime仍然很高

**说明**：
- strace记录的是syscall拦截，不是实际执行
- 即使通过共享内存优化，strace仍会记录
- 关键看实际运行时间，而非strace统计

---

## 📊 性能基准

| 配置 | 运行时间 | 加速比 | clock_gettime占比 |
|------|---------|--------|------------------|
| 当前基线 | 28.23秒 | 6.38x | 96.53% |
| 禁用syscall延迟 | ?秒 | ?x | ? |
| 激进优化 | ?秒 | ?x (目标30x+) | ? |

填写上表需要运行上述测试。

---

## 📝 配置文件模板

### 最终优化配置（shadow_optimized.yaml）

```yaml
general:
  stop_time: 3m
  log_level: warning
  parallelism: 16
  progress: false
  model_unblocked_syscall_latency: false

experimental:
  # 关键优化参数
  max_unapplied_cpu_latency: 100 ms
  unblocked_syscall_latency: 1 ns
  unblocked_vdso_latency: 1 ns
  
  # 其他优化
  use_cpu_pinning: true
  use_worker_spinning: true

network:
  graph:
    type: gml
    inline: |
      graph [
        directed 0
        node [
          id 0
          host_bandwidth_up "1000 Gbit"
          host_bandwidth_down "1000 Gbit"
        ]
        edge [
          source 0
          target 0
          latency "1 us"
          packet_loss 0.0
        ]
      ]

hosts:
  # ... (保持原有hosts配置)
```

---

## ✅ 完成检查清单

- [ ] 步骤1：验证vDSO状态（运行调试配置）
- [ ] 步骤2：应用激进优化配置
- [ ] 步骤3：测试性能提升
- [ ] 步骤4：验证strace数据
- [ ] 记录最终加速比
- [ ] 验证模拟正确性（区块生成、共识是否正常）
- [ ] 更新性能报告

---

## 🎓 关键洞察

1. **shadow-time已经很先进**：
   - 共享内存时间 ✅
   - vDSO补丁机制 ✅
   - 批量延迟累积 ✅

2. **瓶颈可能在配置层面**：
   - 默认配置过于保守（准确性优先）
   - 以太坊测试网可以牺牲部分准确性
   - 需要找到性能/准确性的最佳平衡点

3. **Go程序的特殊性**：
   - Go runtime高度优化
   - 可能绕过标准库直接syscall
   - vDSO补丁需要特殊处理

---

**下一步**：立即运行步骤1-4，验证优化效果！

**预期时间**：完成所有测试约需10-15分钟

**最终目标**：加速比从6.38x提升到25x+（4倍性能提升）

---

**文档日期**：2025-10-06  
**状态**：待执行测试

