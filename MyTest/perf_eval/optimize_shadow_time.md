# Shadow-time优化实施指南

## 🎯 目标

将加速比从当前的6.38x提升到30-50x，通过优化`clock_gettime`系统调用。

---

## 📋 实施步骤

### 步骤1：检查shadow-time当前优化状态

```bash
cd /home/ins0/Repos/all-shadows/shadow-time
git log --oneline --grep="clock" --grep="time" --all -20
git diff main HEAD -- src/lib/shim/
git status
```

查看你已经实施的时间优化内容。

### 步骤2：共享内存时间缓存实现（如果尚未实现）

#### 2.1 在Shadow主进程中创建共享内存

**文件**：`src/main/host/process.rs`

```rust
use std::sync::atomic::{AtomicU64, Ordering};

// 定义共享时间结构
#[repr(C)]
pub struct SharedVirtualTime {
    pub time_ns: AtomicU64,
}

impl Process {
    pub fn create_shared_time_mapping(&mut self) -> Result<()> {
        // 创建共享内存区域
        let size = std::mem::size_of::<SharedVirtualTime>();
        
        // 使用mmap创建共享内存
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        
        if ptr == libc::MAP_FAILED {
            return Err("Failed to create shared memory".into());
        }
        
        // 初始化共享时间
        let shared_time = unsafe { &mut *(ptr as *mut SharedVirtualTime) };
        shared_time.time_ns.store(0, Ordering::Release);
        
        // 保存指针供后续使用
        self.shared_time_ptr = Some(ptr as usize);
        
        Ok(())
    }
    
    pub fn update_shared_virtual_time(&mut self, vtime: EmulatedTime) {
        if let Some(ptr) = self.shared_time_ptr {
            let shared_time = unsafe { &*(ptr as *const SharedVirtualTime) };
            shared_time.time_ns.store(
                vtime.as_nanos() as u64,
                Ordering::Release
            );
        }
    }
}
```

#### 2.2 在Worker中批量更新虚拟时间

**文件**：`src/main/core/worker.rs`

```rust
impl Worker {
    pub fn process_event_batch(&mut self) {
        let mut events_processed = 0;
        
        while let Some(event) = self.get_next_event() {
            self.process_event(event);
            events_processed += 1;
            
            // 每100个事件更新一次共享时间
            // 这个阈值可以调整以平衡性能和准确性
            if events_processed % 100 == 0 {
                self.update_all_shared_times();
            }
        }
    }
    
    fn update_all_shared_times(&mut self) {
        let current_time = self.current_virtual_time();
        for host in self.active_hosts_mut() {
            for process in host.processes_mut() {
                process.update_shared_virtual_time(current_time);
            }
        }
    }
}
```

#### 2.3 在Shim中拦截clock_gettime

**文件**：`src/lib/shim/shim_syscall.c` 或 `src/lib/shim/shim.rs`

```c
// C实现示例
static volatile uint64_t* g_shared_time_ns = NULL;

void shim_init_shared_time(void* ptr) {
    g_shared_time_ns = (volatile uint64_t*)ptr;
}

long shim_clock_gettime(clockid_t clk_id, struct timespec *tp) {
    // 快速路径：从共享内存读取
    if (g_shared_time_ns && 
        (clk_id == CLOCK_REALTIME || clk_id == CLOCK_MONOTONIC)) {
        
        uint64_t ns = __atomic_load_n(g_shared_time_ns, __ATOMIC_ACQUIRE);
        tp->tv_sec = ns / 1000000000ULL;
        tp->tv_nsec = ns % 1000000000ULL;
        return 0;
    }
    
    // 慢速路径：正常系统调用
    return syscall(SYS_clock_gettime, clk_id, tp);
}
```

或者Rust实现：

```rust
// Rust实现示例
use std::sync::atomic::{AtomicU64, Ordering};

static SHARED_TIME_NS: AtomicU64 = AtomicU64::new(0);

#[no_mangle]
pub extern "C" fn shim_clock_gettime(
    clk_id: libc::clockid_t,
    tp: *mut libc::timespec
) -> libc::c_int {
    // 快速路径
    if clk_id == libc::CLOCK_REALTIME || clk_id == libc::CLOCK_MONOTONIC {
        let ns = SHARED_TIME_NS.load(Ordering::Acquire);
        
        if ns > 0 {
            unsafe {
                (*tp).tv_sec = (ns / 1_000_000_000) as i64;
                (*tp).tv_nsec = (ns % 1_000_000_000) as i64;
            }
            return 0;
        }
    }
    
    // 慢速路径：走Shadow的syscall处理
    unsafe {
        libc::syscall(libc::SYS_clock_gettime, clk_id, tp) as libc::c_int
    }
}
```

### 步骤3：编译和测试

```bash
cd /home/ins0/Repos/all-shadows/shadow-time/build
cmake --build . -j16

# 测试优化效果
cd /home/ins0/Repos/shadow-gen/MyTest
echo "=== 优化后测试 ===" 
/usr/bin/time -v /home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow shadow.yaml 2>&1 | tee optimized_result.log

# 提取关键指标
echo ""
echo "=== 性能对比 ==="
echo "优化前："
grep "Elapsed" optimized_baseline.log
echo "优化后："
grep "Elapsed" optimized_result.log
```

### 步骤4：验证效果

运行strace分析验证clock_gettime占比下降：

```bash
# 带strace运行优化版
rm -rf shadow.data
timeout 60 /home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow --strace-logging-mode=standard shadow.yaml

# 分析结果
python3 analyze_strace.py shadow.data/hosts/prysm-beacon-1/beacon-chain.1000.strace

# 预期：clock_gettime从96.53% → <10%
```

---

## 🔧 调优参数

### 共享时间更新频率

```rust
// 在worker.rs中调整
if events_processed % UPDATE_INTERVAL == 0 {
    self.update_all_shared_times();
}
```

**参数建议**：
- `UPDATE_INTERVAL = 100`：平衡性能和准确性（推荐）
- `UPDATE_INTERVAL = 10`：更准确但性能略低
- `UPDATE_INTERVAL = 1000`：更快但可能出现时间滞后

### 时间精度权衡

对于以太坊测试网：
- Slot时间：12秒
- 可接受误差：<100ms
- 建议更新间隔：100-1000个事件

---

## 📊 预期性能提升

| 指标 | 优化前 | 优化后（预期） |
|------|--------|--------------|
| 运行时间 | 28.23秒 | 3-6秒 |
| 加速比 | 6.38x | 30-50x |
| clock_gettime占比 | 96.53% | <10% |
| clock_gettime调用开销 | ~10μs/call | ~50ns/call |

---

## 🐛 可能的问题和解决方案

### 问题1：时间不同步

**症状**：不同进程看到的时间不一致

**解决**：
- 确保所有进程都映射到同一个共享内存
- 使用原子操作保证可见性

### 问题2：时间滞后

**症状**：应用程序超时机制工作异常

**解决**：
- 降低UPDATE_INTERVAL值
- 在关键事件（如网络包到达）后立即更新时间

### 问题3：准确性下降

**症状**：模拟结果与预期不符

**解决**：
- 验证时间语义（单调性、一致性）
- 添加调试日志检查时间更新频率

---

## ✅ 验证检查清单

- [ ] 编译成功，无错误
- [ ] 基本功能测试（模拟能正常运行）
- [ ] 性能测试（加速比>20x）
- [ ] Strace验证（clock_gettime<10%）
- [ ] 准确性验证（区块生成、共识正常）
- [ ] 稳定性测试（多次运行结果一致）

---

## 📚 相关文件

- 性能报告：`PERFORMANCE_REPORT.md`
- 分析脚本：`analyze_strace.py`
- Shadow源码：`/home/ins0/Repos/all-shadows/shadow-time/`
- 测试配置：`shadow.yaml`

---

**创建日期**：2025-10-06  
**状态**：待实施

