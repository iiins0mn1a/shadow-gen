# Shadow性能瓶颈评估方法论

## 📚 目录

1. [Shadow性能分析工具](#shadow性能分析工具)
2. [数据收集方法](#数据收集方法)
3. [数据分析方法](#数据分析方法)
4. [瓶颈识别模式](#瓶颈识别模式)
5. [Shadow源码层面的clock_gettime优化方案](#shadow源码层面的clock_gettime优化方案)
6. [完整评估流程](#完整评估流程)

---

## Shadow性能分析工具

### 1. Shadow内置选项

#### 1.1 Strace日志记录

```bash
shadow --strace-logging-mode=MODE config.yaml
```

**模式选项**：
- `off` (默认): 不记录syscall
- `standard`: 记录所有syscall到 `<host>/<process>.strace`
- `deterministic`: 记录syscall并包含确定性信息

**用途**：
- 分析syscall频率
- 识别高频syscall模式
- 理解应用行为

**输出格式**：
```
时间戳 [tid 线程ID] syscall_name(参数...) = 返回值
示例：
00:00:16.000000000 [tid 1000] clock_gettime(...) = 0
```

#### 1.2 日志级别控制

```yaml
general:
  log_level: error|warning|info|debug|trace
```

**性能影响**：
- `error/warning`: 最小I/O开销，推荐用于性能测试
- `info`: 中等开销，提供基本信息
- `debug/trace`: **严重影响性能**，仅用于调试

**关键洞察**：日志I/O本身可能成为瓶颈！

#### 1.3 并行度配置

```yaml
general:
  parallelism: N  # 工作线程数，建议=CPU核心数
```

**注意**：过高的并行度可能导致锁竞争

#### 1.4 系统调用延迟模型

```yaml
general:
  model_unblocked_syscall_latency: true|false
```

- `true`: 为unblocked syscall建模延迟（更真实，但更慢）
- `false`: 零延迟（更快，但不真实）

**用于测试**：通过开关此选项可以量化syscall开销占比

#### 1.5 进度显示

```yaml
general:
  progress: true|false
```

- 关闭可减少终端I/O开销

### 2. 外部性能工具

#### 2.1 Perf (Linux性能分析器)

```bash
# 安装
sudo apt-get install linux-tools-common linux-tools-generic

# 使用方式
perf record -F 1000 -g --call-graph dwarf -- shadow config.yaml
perf report --stdio --sort comm,symbol > perf_report.txt
```

**分析内容**：
- CPU热点函数
- 调用栈分析
- Shadow vs 应用程序的CPU占比

**局限性**：
- Shadow使用spinlock会干扰结果
- 需要过滤掉spinlock相关符号

#### 2.2 时间测量

```bash
# 基础计时
time shadow config.yaml

# 详细资源使用
/usr/bin/time -v shadow config.yaml
```

**关键指标**：
- User time: 用户态CPU时间
- System time: 内核态CPU时间
- Wall time: 真实运行时间
- 加速比 = 模拟时间 / Wall time

---

## 数据收集方法

### 方法1: 最小化配置测试

**目的**：建立性能基线

```yaml
# 最简配置
general:
  stop_time: 2m  # 短时间快速迭代
  log_level: warning
  parallelism: 16
  progress: false
```

### 方法2: Strace数据收集

```bash
#!/bin/bash
# 标准数据收集流程
rm -rf shadow.data
shadow --strace-logging-mode=standard config.yaml 2>&1 | tee shadow_run.log

# 提取关键指标
echo "=== 运行时间 ==="
grep "real" shadow_run.log || time shadow config.yaml

echo "=== 进程退出状态 ==="
grep "exited with status" shadow_run.log | wc -l

echo "=== Strace文件大小 ==="
du -sh shadow.data/hosts/*/*.strace
```

### 方法3: 分层数据收集

```bash
# Level 1: 无strace（测基线性能）
time shadow config.yaml

# Level 2: 有strace（测syscall开销）
time shadow --strace-logging-mode=standard config.yaml

# Level 3: Perf分析（测CPU热点）
perf record -F 1000 -g -- shadow config.yaml

# 对比性能差异
# Level2 - Level1 = strace overhead
# Level3可识别热点函数
```

---

## 数据分析方法

### 3.1 Syscall频率分析

#### 核心脚本逻辑

```python
import re
from collections import Counter

def parse_strace_line(line):
    """提取syscall名称"""
    match = re.match(r'^[\d:.]+\s+\[tid\s+\d+\]\s+(\w+)\(', line)
    return match.group(1) if match else None

def analyze_frequency(strace_file):
    """统计syscall频率"""
    syscalls = Counter()
    
    with open(strace_file) as f:
        for line in f:
            syscall = parse_strace_line(line)
            if syscall:
                syscalls[syscall] += 1
    
    return syscalls

# 关键指标
total = sum(syscalls.values())
for name, count in syscalls.most_common(10):
    pct = count / total * 100
    per_sec = count / simulation_time
    print(f"{name}: {count:,} ({pct:.1f}%), {per_sec:.0f}/s")
```

#### 分析维度

1. **全局频率**：识别最频繁的syscall
2. **每秒调用率**：评估syscall密度
3. **按主机分组**：识别问题组件
4. **按类别分组**：识别瓶颈类型

### 3.2 Syscall分类法

```python
# 标准分类体系
categories = {
    '时间相关': [
        'clock_gettime', 'gettimeofday', 'time', 
        'clock_nanosleep', 'nanosleep'
    ],
    '内存管理': [
        'mmap', 'munmap', 'madvise', 'mprotect', 'brk'
    ],
    '文件I/O': [
        'read', 'write', 'readv', 'writev', 
        'pread64', 'pwrite64', 'lseek', 'fsync'
    ],
    '网络I/O': [
        'socket', 'bind', 'listen', 'accept', 'connect',
        'send', 'sendto', 'recv', 'recvfrom',
        'sendmsg', 'recvmsg', 'setsockopt', 'getsockopt'
    ],
    '事件轮询': [
        'poll', 'epoll_wait', 'epoll_ctl', 
        'epoll_create', 'select', 'ppoll'
    ],
    '进程/线程': [
        'clone', 'fork', 'futex', 'sched_yield'
    ],
}

def categorize_syscalls(syscalls):
    """按类别统计"""
    category_stats = {}
    for cat_name, syscall_list in categories.items():
        count = sum(syscalls.get(s, 0) for s in syscall_list)
        category_stats[cat_name] = count
    return category_stats
```

### 3.3 瓶颈识别阈值

```python
def identify_bottlenecks(syscalls, total):
    """基于阈值识别瓶颈"""
    
    issues = []
    
    # 规则1: 时间syscall > 50% = 严重瓶颈
    time_pct = (syscalls.get('clock_gettime', 0) + 
                syscalls.get('nanosleep', 0)) / total * 100
    if time_pct > 50:
        issues.append(f"时间syscall占{time_pct:.1f}% - 严重瓶颈")
    
    # 规则2: futex > 5% = 锁竞争
    futex_pct = syscalls.get('futex', 0) / total * 100
    if futex_pct > 5:
        issues.append(f"Futex占{futex_pct:.1f}% - 锁竞争")
    
    # 规则3: epoll_pwait > 15% = 忙等待
    epoll_pct = syscalls.get('epoll_pwait', 0) / total * 100
    if epoll_pct > 15:
        issues.append(f"Epoll占{epoll_pct:.1f}% - 可能忙等待")
    
    # 规则4: I/O > 20% = I/O密集
    io_pct = sum(syscalls.get(s, 0) for s in 
                 ['read', 'write', 'pread64', 'pwrite64']) / total * 100
    if io_pct > 20:
        issues.append(f"I/O syscall占{io_pct:.1f}% - I/O密集")
    
    return issues
```

### 3.4 加速比分析

```python
def analyze_speedup(sim_time, real_time, syscall_stats):
    """分析加速比与瓶颈关系"""
    
    actual_speedup = sim_time / real_time
    total_syscalls = sum(syscall_stats.values())
    
    # 估算syscall开销
    # 假设每次syscall平均开销10μs
    syscall_overhead_sec = total_syscalls * 10e-6
    syscall_overhead_pct = syscall_overhead_sec / real_time * 100
    
    # 理论加速比（假设无syscall开销）
    theoretical_speedup = sim_time / (real_time - syscall_overhead_sec)
    
    print(f"实际加速比: {actual_speedup:.1f}x")
    print(f"Syscall开销占比: {syscall_overhead_pct:.1f}%")
    print(f"理论加速比（无syscall开销）: {theoretical_speedup:.1f}x")
    print(f"加速比损失: {theoretical_speedup - actual_speedup:.1f}x")
```

---

## 瓶颈识别模式

### 模式1: 时间轮询瓶颈（本案例）

**特征**：
- `clock_gettime` 占比 > 50%
- 每秒调用数万次
- 主要来自应用程序，非Shadow

**根因**：
- Go runtime的调度器
- 应用层的定时器/超时机制
- Event-driven模型不匹配

**验证方法**：
```bash
# 禁用model_unblocked_syscall_latency看性能变化
# 如果提升显著 → 确认是syscall开销问题
```

### 模式2: 锁竞争瓶颈

**特征**：
- `futex` 占比 > 5%
- 随并行度增加而增加

**根因**：
- Shadow内部锁竞争
- 应用层多线程竞争

**验证方法**：
```bash
# 测试不同parallelism值
for p in 1 4 8 16; do
    sed "s/parallelism: .*/parallelism: $p/" config.yaml > test.yaml
    time shadow test.yaml
done
```

### 模式3: I/O瓶颈

**特征**：
- `read/write` 占比 > 20%
- 日志级别对性能影响大

**验证方法**：
```bash
# 测试不同日志级别
for level in error warning info; do
    sed "s/log_level: .*/log_level: $level/" config.yaml > test.yaml
    time shadow test.yaml
done
```

### 模式4: 网络包处理瓶颈

**特征**：
- `sendto/recvfrom` 频率高
- Shadow日志显示大量包处理

**验证方法**：
```bash
# 检查包数量
grep "Packet has destination" shadow.data/shadow.log | wc -l

# 调整网络参数
# 注意：延迟降低 ≠ 性能提升（如你所指出）
# 关键是减少包数量，而非延迟
```

---

## Shadow源码层面的clock_gettime优化方案

### 5.1 问题分析

**当前实现**（推测）：
```rust
// Shadow当前的clock_gettime处理
fn syscall_handler_clock_gettime() {
    // 1. Context switch到Shadow
    // 2. 查询当前虚拟时间
    // 3. 写入用户空间
    // 4. Context switch回应用
}
```

**开销来源**：
- 每次调用需要2次context switch
- 系统调用trap的overhead
- Shadow的调度器介入

**1,528,582次调用 × 估计10μs/次 ≈ 15秒开销**

### 5.2 优化方案：vDSO风格的快速路径

#### 方案A: 共享内存时间缓存（推荐）⭐⭐⭐⭐⭐

**核心思想**：Shadow在共享内存中维护当前虚拟时间，进程直接读取

```rust
// Shadow侧实现
struct SharedTime {
    virtual_time_ns: AtomicU64,  // 原子操作保证可见性
    virtual_time_sec: AtomicU64,
}

impl Shadow {
    fn update_shared_time(&mut self, host_id: HostId) {
        let vtime = self.get_virtual_time(host_id);
        let shared = &self.host_shared_memory[host_id].time;
        shared.virtual_time_ns.store(vtime.as_nanos() as u64, Ordering::Release);
        shared.virtual_time_sec.store(vtime.as_secs(), Ordering::Release);
    }
}

// Shim侧实现（进程内）
#[inline(always)]
fn fast_clock_gettime(clockid: i32, tp: *mut timespec) -> i32 {
    // 快速路径：直接从共享内存读取
    if clockid == CLOCK_REALTIME || clockid == CLOCK_MONOTONIC {
        unsafe {
            let shared = get_shared_time_ptr();  // mmap的共享内存
            let secs = (*shared).virtual_time_sec.load(Ordering::Acquire);
            let nsecs = (*shared).virtual_time_ns.load(Ordering::Acquire) % 1_000_000_000;
            
            (*tp).tv_sec = secs as i64;
            (*tp).tv_nsec = nsecs as i64;
            return 0;
        }
    }
    
    // 慢速路径：走正常syscall
    real_syscall(SYS_clock_gettime, clockid, tp)
}
```

**实现步骤**：

1. **在Shadow中添加共享内存区域**
   ```rust
   // src/main/host/process.rs
   pub struct Process {
       // ... 现有字段
       shared_time_mapping: Option<MemoryMapping>,
   }
   
   impl Process {
       pub fn create_shared_time_mapping(&mut self) -> Result<()> {
           // 创建共享内存
           let mapping = MemoryMapping::new(
               std::mem::size_of::<SharedTime>(),
               PROT_READ,  // 只读，提高安全性
               MAP_SHARED,
           )?;
           
           self.shared_time_mapping = Some(mapping);
           Ok(())
       }
   }
   ```

2. **在事件处理时更新共享时间**
   ```rust
   // src/main/core/worker.rs
   impl Worker {
       fn process_event(&mut self, event: Event) {
           // 处理事件...
           
           // 更新虚拟时间（批量更新，不是每个事件）
           if self.should_update_time() {
               for host in self.active_hosts() {
                   host.update_shared_time();
               }
           }
       }
       
       fn should_update_time(&self) -> bool {
           // 策略：每N个事件更新一次
           self.event_count % 100 == 0
       }
   }
   ```

3. **在Shim层拦截clock_gettime**
   ```rust
   // src/lib/shim/shim_syscall.rs
   
   #[no_mangle]
   pub extern "C" fn syscall_clock_gettime(
       clockid: i32,
       tp: *mut libc::timespec
   ) -> i32 {
       // 尝试快速路径
       if let Some(shared_time) = get_process_shared_time() {
           return fast_read_time(clockid, tp, shared_time);
       }
       
       // 降级到正常syscall
       shadow_syscall(SYS_clock_gettime, clockid as u64, tp as u64, 0, 0, 0, 0)
   }
   
   #[inline(always)]
   fn fast_read_time(
       clockid: i32, 
       tp: *mut libc::timespec,
       shared: &SharedTime
   ) -> i32 {
       match clockid {
           libc::CLOCK_REALTIME | libc::CLOCK_MONOTONIC => {
               unsafe {
                   let ns = shared.virtual_time_ns.load(Ordering::Acquire);
                   (*tp).tv_sec = (ns / 1_000_000_000) as i64;
                   (*tp).tv_nsec = (ns % 1_000_000_000) as i64;
               }
               0
           }
           _ => {
               // 其他时钟类型走正常路径
               shadow_syscall(SYS_clock_gettime, clockid as u64, tp as u64, 0, 0, 0, 0)
           }
       }
   }
   ```

**预期效果**：
- 减少 **95-99%** 的clock_gettime开销
- 从 10μs/call → 50ns/call（200x加速）
- 总体加速比: 6.5x → **30-50x** 🚀

**准确性考虑**：
- 时间可能有轻微滞后（最多100个事件的延迟）
- 对大多数应用可接受
- 可通过调整更新频率平衡准确性vs性能

#### 方案B: VDSO注入（高级）⭐⭐⭐⭐

**原理**：像Linux vDSO一样，在进程地址空间注入快速时间查询代码

```rust
// Shadow注入一段代码到进程地址空间
fn inject_vdso_page(&mut self, process: &Process) -> Result<()> {
    // 1. 分配一页可执行内存
    let vdso_page = mmap_executable_page()?;
    
    // 2. 写入汇编代码（x86_64示例）
    let code = assemble_fast_clock_gettime();
    copy_to_process_memory(process, vdso_page, code)?;
    
    // 3. 修改进程的auxv，让它使用我们的vDSO
    modify_auxv(process, vdso_page)?;
    
    Ok(())
}

// 生成的汇编代码（伪代码）
fn assemble_fast_clock_gettime() -> Vec<u8> {
    // mov rax, [shared_time_address]  ; 从共享内存读取
    // mov [rdi], rax                   ; 写入timespec
    // xor eax, eax                     ; 返回0
    // ret
    vec![/* 机器码 */]
}
```

**优点**：
- 最快（直接内存读取，无函数调用开销）
- 接近硬件RDTSC性能

**缺点**：
- 实现复杂
- 需要处理多架构
- 调试困难

#### 方案C: 延迟更新策略（简单）⭐⭐⭐

**思想**：应用调用clock_gettime时返回缓存值，仅在必要时更新

```rust
// 每个进程维护上次返回的时间
struct ProcessTimeCache {
    last_returned_time: EmulatedTime,
    last_update_event: u64,
}

fn handle_clock_gettime(&mut self, ctx: &SyscallContext) -> SyscallResult {
    let cache = &mut self.process_time_cache;
    let current_event = self.event_counter;
    
    // 策略：最多100个事件才更新一次时间
    if current_event - cache.last_update_event > 100 {
        cache.last_returned_time = self.current_virtual_time();
        cache.last_update_event = current_event;
    }
    
    // 返回缓存的时间
    write_time_to_user(ctx, cache.last_returned_time)?;
    Ok(0)
}
```

**预期效果**：
- 减少Shadow调度器介入
- 实现简单
- 加速比提升: 6.5x → **10-15x**

### 5.3 实现优先级建议

1. **Phase 1**: 方案C（延迟更新）- 快速验证概念
2. **Phase 2**: 方案A（共享内存）- 平衡性能与实现复杂度
3. **Phase 3**: 方案B（VDSO）- 极致性能优化

### 5.4 验证方法

```bash
# 测试优化效果
echo "=== 优化前 ==="
time shadow config.yaml
python3 analyze_syscalls.py  # 记录clock_gettime占比

# 应用优化
cd shadow-src
git checkout -b optimize-clock-gettime
# ... 实现方案A/B/C

# 重新编译
cargo build --release

# 测试优化后
echo "=== 优化后 ==="
time ../shadow/target/release/shadow config.yaml
python3 analyze_syscalls.py  # 对比clock_gettime占比

# 计算加速比提升
# 预期：clock_gettime占比从87% → 10%以下
```

---

## 完整评估流程

### Step 1: 基线测试

```bash
#!/bin/bash
# baseline_test.sh

echo "=== Phase 1: 无strace基线 ==="
rm -rf shadow.data
time shadow config.yaml 2>&1 | tee baseline.log

echo "=== Phase 2: 带strace测试 ==="
rm -rf shadow.data
time shadow --strace-logging-mode=standard config.yaml 2>&1 | tee strace.log

echo "=== Phase 3: 分析syscall ==="
python3 analyze_syscalls.py > syscall_report.txt
```

### Step 2: 识别瓶颈

```bash
# 检查syscall报告
cat syscall_report.txt | grep "🔥 高频"

# 识别模式
if grep -q "clock_gettime.*[5-9][0-9]%" syscall_report.txt; then
    echo "瓶颈类型: 时间轮询"
    echo "建议: 实现快速时间查询"
elif grep -q "futex.*[5-9]%" syscall_report.txt; then
    echo "瓶颈类型: 锁竞争"
    echo "建议: 降低并行度或优化锁"
fi
```

### Step 3: 验证优化

```bash
# 实现优化后
./test_optimization.sh

# 对比结果
echo "优化前加速比: $(calculate_speedup baseline.log)"
echo "优化后加速比: $(calculate_speedup optimized.log)"
echo "提升倍数: $(bc <<< "scale=2; $(get_speedup optimized.log) / $(get_speedup baseline.log)")"
```

### Step 4: 迭代优化

```bash
# 持续监控关键指标
watch_metrics() {
    while true; do
        clear
        echo "=== 当前性能指标 ==="
        echo "加速比: $(get_speedup)"
        echo "Clock_gettime占比: $(get_clock_pct)"
        echo "Futex占比: $(get_futex_pct)"
        echo ""
        echo "按Ctrl+C退出监控"
        sleep 5
    done
}
```

---

## 关键洞察总结

### 你的分析的正确性 ✅

1. **网络延迟优化无效** ✅
   - 原因：事件数量不变，仅改变事件间隔
   - 在event-driven模型中，间隔长短不影响处理速度

2. **Max-peer优化效果有限** ✅
   - 原因：87%开销在时间查询，网络开销<1%
   - 减少peer仅能优化边际收益

3. **Clock_gettime是核心瓶颈** ✅
   - 76.88%的syscall是clock_gettime
   - 每次调用的context switch开销累积巨大

### 方法论的价值

1. **系统化**：从工具→数据→分析→识别→优化
2. **可重复**：标准化的流程可应用于任何Shadow模拟
3. **可验证**：每步都有量化指标
4. **可迭代**：优化后重新评估，形成闭环

### 下一步行动

```bash
# 1. 在Shadow源码实现共享内存时间缓存
cd /path/to/shadow
git checkout -b fast-clock-gettime

# 2. 参考上述方案A的代码实现

# 3. 验证效果
cargo build --release
cd /home/ins0/Repos/shadow-gen/MyTest
time ../shadow/target/release/shadow config.yaml

# 4. 预期结果
# - Clock_gettime占比: 87% → <10%
# - 加速比: 6.5x → 30-50x
```

---

## 附录：完整分析脚本

详见：
- `analyze_syscalls.py`: Syscall频率统计
- `PERFORMANCE_ANALYSIS_REPORT.md`: 详细分析报告
- 本文档: 方法论总结

---

**最后更新**: 2025-09-30
**版本**: 2.0
