# Shadow优化提案：快速时间查询机制

## 📋 提案概要

**问题**：在模拟Go程序（如Ethereum客户端）时，`clock_gettime`系统调用占用高达87%的syscall频率，严重限制了Shadow的加速比（当前~6x，理论应>30x）。

**解决方案**：实现类似Linux vDSO的共享内存时间缓存机制，让进程无需syscall即可读取虚拟时间。

**预期效果**：
- `clock_gettime`开销降低 **95-99%**
- 加速比提升至 **30-50x**（5-8倍改善）
- 对现有代码影响最小

---

## 🔍 问题分析

### 实际测量数据

**测试环境**：
- 模拟内容：Ethereum PoS测试网（2个beacon节点，2个validator，1个geth节点）
- 模拟时间：2分钟
- 实际运行时间：18秒
- 加速比：6.67x

**Syscall统计**：
```
Syscall         调用次数      占比      每秒调用
-----------------------------------------------
clock_gettime   1,528,582    76.88%    12,738/s
nanosleep         208,269    10.48%     1,736/s
epoll_pwait       144,402     7.26%     1,203/s
其他               106,898     5.38%       891/s
-----------------------------------------------
总计            1,988,151   100.00%    16,568/s
```

**时间相关syscall总计占比：87.4%**

### 问题根源

1. **Go Runtime的特性**
   - Go调度器频繁查询时间用于goroutine调度
   - Timer/Ticker机制依赖高频时间查询
   - 网络超时、上下文超时都需要时间

2. **Shadow的开销**
   ```
   每次clock_gettime调用流程：
   1. 用户态 → 内核态 trap
   2. Shadow拦截syscall
   3. Context switch到Shadow进程
   4. 查询当前虚拟时间
   5. 写入用户空间
   6. Context switch回用户进程
   7. 内核态 → 用户态返回
   
   估算：每次调用 ~10μs
   总开销：1,528,582 × 10μs ≈ 15秒
   ```

3. **与Event-Driven模型的矛盾**
   - Shadow是event-driven，应能"跳过"空闲时间
   - 但频繁的时间查询迫使Shadow持续处理"伪事件"
   - 破坏了event-driven的核心优势

---

## 💡 解决方案设计

### 方案：共享内存时间缓存

**核心思想**：
- Shadow在共享内存维护当前虚拟时间
- Shim层直接从共享内存读取，无需syscall
- Shadow在处理事件时批量更新时间

### 架构设计

```
┌─────────────────────────────────────────────────────────┐
│                     User Process                         │
│                                                           │
│  Application Code                                         │
│       ↓                                                   │
│  libc: clock_gettime()                                    │
│       ↓                                                   │
│  Shim: fast_clock_gettime() ←─┐                         │
│       ↓                        │                          │
│  [Shared Memory Read]          │ NO SYSCALL!             │
│       ↓                        │                          │
│  Return immediately            │                          │
│                                │                          │
└────────────────────────────────┼──────────────────────────┘
                                 │
                     ┌───────────┘
                     │ mmap
                     ↓
┌─────────────────────────────────────────────────────────┐
│              Shared Memory Region                        │
│                                                           │
│  struct SharedTime {                                      │
│      virtual_time_ns: AtomicU64,    // 纳秒时间戳       │
│      sequence: AtomicU32,            // 版本号（可选）   │
│  }                                                        │
│                                                           │
└────────────────────────────────────────────────────────┬─┘
                                                          │
                     ┌────────────────────────────────────┘
                     │ mmap (RW)
                     ↓
┌─────────────────────────────────────────────────────────┐
│                    Shadow Process                        │
│                                                           │
│  Event Loop:                                              │
│    process_event() {                                      │
│        // ... 处理事件                                    │
│        if (should_update_time()) {                        │
│            update_shared_time();  ←─ 批量更新            │
│        }                                                  │
│    }                                                      │
│                                                           │
└─────────────────────────────────────────────────────────┘
```

### 详细实现

#### 1. 共享内存数据结构

```rust
// src/main/host/shared_memory.rs

/// 与进程共享的时间信息
#[repr(C)]
pub struct SharedTime {
    /// 虚拟时间（纳秒）
    /// 使用Relaxed ordering足够，因为：
    /// 1. 单写者（Shadow）
    /// 2. 读者只需要"足够新"的时间，不需要严格同步
    pub virtual_time_ns: AtomicU64,
    
    /// 可选：序列号，用于检测并发更新（如需要）
    /// 类似Linux vDSO的seqlock机制
    pub sequence: AtomicU32,
    
    /// Padding到缓存行大小，避免false sharing
    _padding: [u8; 64 - 12],
}

impl SharedTime {
    pub fn new(initial_time: EmulatedTime) -> Self {
        Self {
            virtual_time_ns: AtomicU64::new(initial_time.as_nanos() as u64),
            sequence: AtomicU32::new(0),
            _padding: [0; 64 - 12],
        }
    }
    
    /// Shadow调用：更新时间
    pub fn update(&self, new_time: EmulatedTime) {
        // 可选：增加sequence（实现seqlock）
        // self.sequence.fetch_add(1, Ordering::Release);
        
        self.virtual_time_ns.store(
            new_time.as_nanos() as u64,
            Ordering::Release  // 确保之前的写入对读者可见
        );
        
        // self.sequence.fetch_add(1, Ordering::Release);
    }
    
    /// Shim调用：读取时间（快速路径）
    #[inline(always)]
    pub fn read(&self) -> u64 {
        // 简单版本：直接读取
        self.virtual_time_ns.load(Ordering::Acquire)
        
        // 高级版本：使用seqlock保证一致性
        // loop {
        //     let seq1 = self.sequence.load(Ordering::Acquire);
        //     if seq1 & 1 != 0 { continue; }  // 写入中
        //     
        //     let time = self.virtual_time_ns.load(Ordering::Acquire);
        //     
        //     let seq2 = self.sequence.load(Ordering::Acquire);
        //     if seq1 == seq2 { return time; }  // 一致
        // }
    }
}
```

#### 2. Shadow侧实现

```rust
// src/main/host/process.rs

pub struct Process {
    // ... 现有字段
    
    /// 与进程共享的时间缓存
    shared_time: Option<Arc<SharedTime>>,
    shared_time_mapping: Option<MemoryMapping>,
}

impl Process {
    /// 创建共享时间映射
    pub fn setup_shared_time(&mut self, initial_time: EmulatedTime) -> Result<()> {
        // 1. 创建共享内存对象
        let shm_name = format!("/shadow-time-{}", self.id());
        let shm_fd = shm_open(
            shm_name.as_str(),
            O_CREAT | O_RDWR,
            0o600
        )?;
        
        // 2. 设置大小
        ftruncate(shm_fd, std::mem::size_of::<SharedTime>() as i64)?;
        
        // 3. Shadow侧映射（读写）
        let shadow_mapping = unsafe {
            mmap(
                std::ptr::null_mut(),
                std::mem::size_of::<SharedTime>(),
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                shm_fd,
                0
            )?
        };
        
        // 4. 初始化SharedTime
        let shared_time = unsafe {
            let ptr = shadow_mapping as *mut SharedTime;
            ptr.write(SharedTime::new(initial_time));
            Arc::new(&*ptr)
        };
        
        // 5. 通知Shim共享内存地址（通过环境变量或auxv）
        self.set_env("SHADOW_SHARED_TIME_FD", shm_fd.to_string())?;
        self.set_env("SHADOW_SHARED_TIME_NAME", shm_name)?;
        
        self.shared_time = Some(shared_time);
        Ok(())
    }
    
    /// 更新共享时间
    #[inline]
    pub fn update_shared_time(&self, new_time: EmulatedTime) {
        if let Some(ref shared) = self.shared_time {
            shared.update(new_time);
        }
    }
}

// src/main/core/worker.rs

impl Worker {
    /// 批量更新策略
    fn process_events(&mut self) {
        let mut events_processed = 0;
        
        while let Some(event) = self.event_queue.pop() {
            self.process_single_event(event);
            events_processed += 1;
            
            // 策略：每处理N个事件更新一次时间
            // N的选择平衡准确性vs性能
            if events_processed % self.config.time_update_interval == 0 {
                self.update_all_shared_times();
            }
        }
        
        // 确保最后时间是最新的
        self.update_all_shared_times();
    }
    
    fn update_all_shared_times(&mut self) {
        let current_time = self.current_time();
        for host in self.active_hosts() {
            for process in host.processes() {
                process.update_shared_time(current_time);
            }
        }
    }
}
```

#### 3. Shim侧实现

```rust
// src/lib/shim/shim_syscall.rs

use std::sync::atomic::{AtomicU64, Ordering};

/// 缓存的共享时间指针
static SHARED_TIME_PTR: AtomicUsize = AtomicUsize::new(0);

/// 初始化（在shim启动时调用）
pub fn init_shared_time() -> Result<()> {
    // 从环境变量获取共享内存信息
    let shm_name = std::env::var("SHADOW_SHARED_TIME_NAME")?;
    
    // 打开共享内存
    let shm_fd = shm_open(shm_name.as_str(), O_RDONLY, 0)?;
    
    // 映射到进程地址空间（只读）
    let ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            std::mem::size_of::<SharedTime>(),
            PROT_READ,  // 只读，提高安全性
            MAP_SHARED,
            shm_fd,
            0
        )?
    };
    
    SHARED_TIME_PTR.store(ptr as usize, Ordering::Release);
    close(shm_fd)?;
    
    Ok(())
}

/// 快速时间查询（关键路径）
#[no_mangle]
#[inline(always)]
pub extern "C" fn shim_clock_gettime(
    clockid: libc::clockid_t,
    tp: *mut libc::timespec
) -> libc::c_int {
    // 检查是否支持快速路径
    match clockid {
        libc::CLOCK_REALTIME | libc::CLOCK_MONOTONIC => {
            let shared_ptr = SHARED_TIME_PTR.load(Ordering::Acquire);
            
            if shared_ptr != 0 {
                // 快速路径：直接读取共享内存
                unsafe {
                    let shared = &*(shared_ptr as *const SharedTime);
                    let ns = shared.read();
                    
                    (*tp).tv_sec = (ns / 1_000_000_000) as i64;
                    (*tp).tv_nsec = (ns % 1_000_000_000) as i64;
                }
                return 0;
            }
        }
        _ => {}
    }
    
    // 慢速路径：走正常syscall处理
    // （用于其他时钟类型或初始化失败情况）
    shadow_syscall_handler(SYS_clock_gettime, clockid as u64, tp as u64, 0, 0, 0, 0)
}
```

### 配置选项

```yaml
# shadow.yaml 新增配置
experimental:
  # 启用快速时间查询
  fast_clock_gettime: true
  
  # 时间更新间隔（事件数）
  # 更小 = 更准确但更频繁的更新
  # 更大 = 更好的性能但可能有轻微滞后
  time_update_interval: 100  # 默认值
```

---

## 📊 性能评估

### 理论分析

**优化前**：
```
每次clock_gettime:
  - Syscall trap: ~1μs
  - Context switch: ~2μs
  - Shadow处理: ~5μs
  - Context switch回: ~2μs
  总计: ~10μs

1,528,582次 × 10μs = 15.3秒
```

**优化后**：
```
每次clock_gettime:
  - 内存读取: ~10ns (L1 cache命中)
  - 原子操作: ~20ns
  总计: ~50ns

1,528,582次 × 50ns = 0.076秒
```

**开销降低**：15.3秒 → 0.076秒（**200倍改善**）

### 预期加速比

```
当前：
  实际运行时间 = 18秒
  其中clock_gettime开销 = 15秒
  其他开销 = 3秒
  加速比 = 120s / 18s = 6.67x

优化后：
  实际运行时间 = 3秒 + 0.076秒 ≈ 3.1秒
  加速比 = 120s / 3.1s = 38.7x
  
提升倍数 = 38.7 / 6.67 ≈ 5.8倍
```

### 准确性影响

**时间滞后**：
- 最大滞后 = `time_update_interval` × 平均事件处理时间
- 默认100个事件，假设每个事件10μs → 最大滞后1ms
- 对大多数应用可接受

**可调节**：
```rust
// 对时间敏感的应用可降低间隔
time_update_interval: 10  // 更频繁更新

// 追求极致性能可增大间隔
time_update_interval: 1000  // 更少更新
```

---

## 🔄 兼容性

### 向后兼容

- **默认禁用**：通过配置选项启用，不影响现有模拟
- **降级机制**：如果共享内存初始化失败，自动回退到标准syscall处理
- **选择性启用**：可针对特定host或process启用

### 多架构支持

- **x86_64**：AtomicU64有硬件支持，性能最优
- **ARM64**：同样支持64位原子操作
- **其他**：可能需要使用锁（但仍比syscall快）

---

## 🛠️ 实现计划

### Phase 1: 原型验证（1-2周）
- [ ] 实现基础SharedTime结构
- [ ] 在Shadow侧添加共享内存创建
- [ ] 在Shim侧实现快速读取
- [ ] 基础测试：验证功能正确性

### Phase 2: 性能优化（1周）
- [ ] 优化内存布局（cache line对齐）
- [ ] 实现批量更新策略
- [ ] 性能测试：测量实际加速比

### Phase 3: 完善功能（1-2周）
- [ ] 添加配置选项
- [ ] 实现降级机制
- [ ] 完整测试套件
- [ ] 文档更新

### Phase 4: 社区反馈（持续）
- [ ] 发布PR到Shadow仓库
- [ ] 收集用户反馈
- [ ] 迭代改进

---

## 🧪 测试策略

### 功能测试

```rust
#[test]
fn test_shared_time_basic() {
    let shared = SharedTime::new(EmulatedTime::from_secs(100));
    
    // 读取初始值
    assert_eq!(shared.read(), 100_000_000_000);
    
    // 更新
    shared.update(EmulatedTime::from_secs(200));
    assert_eq!(shared.read(), 200_000_000_000);
}

#[test]
fn test_concurrent_read_write() {
    // 测试一个写者多个读者的场景
    // 确保没有race condition
}
```

### 性能测试

```rust
#[bench]
fn bench_shared_time_read(b: &mut Bencher) {
    let shared = SharedTime::new(EmulatedTime::SIMULATION_START);
    b.iter(|| {
        black_box(shared.read());
    });
    // 预期: <50ns/iter
}

#[bench]
fn bench_syscall_clock_gettime(b: &mut Bencher) {
    b.iter(|| {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        black_box(ts);
    });
    // 对比: ~1000ns/iter (取决于系统)
}
```

### 集成测试

```bash
# 测试Ethereum测试网
cd examples/ethereum-testnet
time shadow --config fast_time config.yaml

# 对比
# 优化前: ~18秒, 加速比6.67x
# 优化后: ~3秒, 加速比38x
```

---

## 📚 参考资料

### 相关技术

1. **Linux vDSO**
   - Kernel提供的用户态快速syscall
   - `clock_gettime`等时间函数可无syscall调用
   - 原理：共享只读内存页

2. **Seqlock**
   - 适用于读多写少场景的同步原语
   - 使用sequence number检测并发写入
   - 比mutex快得多

3. **Go Runtime时间查询**
   - `runtime.nanotime()` 内部调用
   - Timer/Ticker机制
   - 调度器的时间切片

### Shadow相关Issue

- 可搜索Shadow GitHub Issues关于`clock_gettime`性能的讨论
- 类似优化可能已有讨论但未实现

---

## 💬 讨论问题

1. **时间更新策略**：
   - 固定间隔 vs 自适应间隔？
   - 是否需要per-process的更新策略？

2. **准确性保证**：
   - Seqlock是否必要？
   - 如何平衡性能与准确性？

3. **API设计**：
   - 是否需要用户可见的配置选项？
   - 如何处理特殊时钟类型（如CLOCK_THREAD_CPUTIME_ID）？

---

## 📝 总结

这个优化提案针对Shadow在模拟Go程序时的关键性能瓶颈，通过引入共享内存时间缓存机制，预期可将加速比从6x提升至**30-50x**。

**关键优势**：
- ✅ 巨大的性能提升（5-8倍）
- ✅ 实现相对简单
- ✅ 向后兼容
- ✅ 准确性影响可控

**实施建议**：
建议Shadow社区采纳此方案，将大幅提升Shadow在模拟现代应用（特别是Go/Rust编写的高性能网络应用）时的性能。

---

**作者**：基于Ethereum测试网模拟的实际性能分析  
**日期**：2025-09-30  
**Shadow版本**：3.2.0+
