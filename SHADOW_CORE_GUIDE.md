# Shadow 核心实现指南

本文档从 `run_shadow` 函数开始，逐步深入解释 Shadow 的核心实现，重点关注：
1. 事件调度算法
2. 多 host 的超轻量虚拟化实现（共用代码段）

---

## 第一部分：整体架构

### 1.1 入口函数 `run_shadow`

```rust
pub fn run_shadow(args: Vec<&OsStr>) -> anyhow::Result<()>
```

**主要职责：**
- 初始化系统环境（共享内存、信号处理、资源限制）
- 解析配置文件和命令行参数
- 创建 `Controller` 并启动模拟

**关键步骤：**
1. 设置共享内存分配器（用于 Shadow 主进程和模拟进程之间的通信）
2. 创建 `SimConfig`（包含所有 host 的配置信息）
3. 创建 `Controller`（模拟的主控制器）
4. 调用 `controller.run()` 启动模拟循环

---

## 第二部分：事件调度算法

### 2.1 核心概念

Shadow 使用**离散事件模拟（Discrete Event Simulation）**：
- 每个事件都有一个**模拟时间（EmulatedTime）**
- 事件按时间顺序处理
- 模拟时间可以"跳跃"（跳过空闲时间）

### 2.2 事件类型

```rust
pub enum EventData {
    Packet(PacketEventData),  // 网络数据包到达事件
    Local(LocalEventData),     // 本地事件（定时器、任务等）
}
```

**事件排序规则：**
- 首先按时间排序
- 时间相同时，Packet 事件优先于 Local 事件
- 这确保了网络事件的确定性处理顺序

### 2.3 事件队列

每个 Host 都有自己的事件队列：

```rust
pub struct EventQueue {
    queue: BinaryHeap<Reverse<PanickingOrd<Event>>>,  // 最小堆，按时间排序
    last_popped_event_time: EmulatedTime,
}
```

**关键特性：**
- 使用 `BinaryHeap`（最小堆）实现优先队列
- 保证时间单调递增（不能回到过去）
- O(log n) 的插入和删除操作

### 2.4 Runahead 算法（核心调度策略）

这是 Shadow 实现高效并行的关键！

**基本思想：**
- 每个 host 可以独立执行到某个时间点（`window_end`）
- 只要不与其他 host 交互，就可以并行执行
- 通过 `runahead` 参数控制并行度

**调度循环（Manager::run）：**

```rust
// 1. 定义时间窗口
let mut window = Some((window_start, window_end));

while let Some((window_start, window_end)) = window {
    // 2. 并行执行所有 host
    scheduler.scope(|s| {
        s.run_with_data(&thread_next_event_times, |_, hosts, next_event_time| {
            for_each_host(hosts, |host| {
                host.execute(window_end);  // 执行到 window_end
            });
        });
    });
    
    // 3. 找到所有 host 的下一个事件时间
    let min_next_event_time = /* 所有 host 的最小下一个事件时间 */;
    
    // 4. 计算新的时间窗口
    window = controller.manager_finished_current_round(min_next_event_time);
}
```

**Runahead 计算：**

```rust
fn manager_finished_current_round(
    &self,
    min_next_event_time: EmulatedTime,
) -> Option<(EmulatedTime, EmulatedTime)> {
    let runahead = worker::WORKER_SHARED.runahead.get();
    let new_start = min_next_event_time;
    let new_end = new_start + runahead;  // 新的窗口结束时间
    Some((new_start, new_end))
}
```

**为什么这样可以并行？**
- 如果两个 host 的下一个事件时间都在 `window_end` 之后，它们可以独立执行
- 网络延迟保证了 host 之间的交互不会立即发生
- 只要 `runahead` 小于最小网络延迟，就不会有冲突

### 2.5 Host 的事件执行

```rust
pub fn execute(&self, until: EmulatedTime) {
    loop {
        // 1. 从事件队列取出下一个事件
        let event = event_queue.pop();
        
        // 2. 检查是否超过时间窗口
        if event.time() >= until {
            break;
        }
        
        // 3. 检查 CPU 延迟（模拟 CPU 处理时间）
        if cpu_delay > SimulationTime::ZERO {
            // 重新调度事件
            event.set_time(event.time() + cpu_delay);
            self.push_local_event(event);
            continue;
        }
        
        // 4. 执行事件
        Worker::set_current_time(event.time());
        match event.data() {
            EventData::Packet(data) => {
                // 处理网络数据包
                self.route_incoming_packet(data.into());
            }
            EventData::Local(data) => {
                // 执行本地任务
                TaskRef::from(data).execute(self);
            }
        }
    }
}
```

---

## 第三部分：多 Host 的超轻量虚拟化

### 3.1 核心思想

Shadow 不创建真正的虚拟机，而是：
1. **直接执行真实进程**（使用 `fork` + `exec`）
2. **拦截系统调用**（通过 `LD_PRELOAD` 注入 shim 库）
3. **共享代码段**（所有进程共享相同的可执行文件）

### 3.2 进程创建流程

**1. Shadow 主进程创建 Host：**

```rust
// src/main/core/manager.rs
let host = Host::new(host_id, host_info, ...);
```

**2. Host 创建进程（Process）：**

```rust
// src/main/host/process.rs
pub fn spawn(...) -> RootedRc<Host, RootedRefCell<Host, Process>> {
    // 1. 创建共享内存（用于 Shadow 和进程通信）
    let shim_shared_mem = ProcessShmem::new(...);
    
    // 2. 使用 fork + exec 启动真实进程
    let mthread = ManagedThread::spawn(
        plugin_path,  // 可执行文件路径
        argv,
        envv,
        preload_paths,  // 包含 shim 库的路径
    )?;
    
    // 3. 包装为 Process 对象
    Process::wrap_mthread(...)
}
```

**3. ManagedThread::spawn（实际创建进程）：**

```rust
// src/main/host/managed_thread.rs
pub fn spawn(...) -> Result<Self> {
    // fork 创建子进程
    let pid = unsafe { libc::fork() };
    
    if pid == 0 {
        // 子进程：设置环境变量，加载 shim 库
        std::env::set_var("LD_PRELOAD", shim_library_path);
        execve(program_path, argv, envv);
    } else {
        // 父进程：返回 ManagedThread
        ManagedThread { native_pid: pid, ... }
    }
}
```

### 3.3 代码段共享

**为什么可以共享代码段？**

1. **只读代码段**：可执行文件的代码段是只读的，多个进程可以共享同一份内存映射
2. **写时复制（COW）**：数据段和堆栈是独立的，每个进程有自己的副本
3. **Linux 内核优化**：内核自动处理代码段的共享

**实际效果：**
- 1000 个相同的进程只需要 1 份代码段内存
- 每个进程只有独立的数据段和堆栈
- 大幅减少内存占用

### 3.4 系统调用拦截（Shim）

**Shim 库的作用：**
- 拦截进程的系统调用
- 将系统调用转发给 Shadow 主进程
- 实现虚拟化的网络、时间、文件系统等

**共享内存通信：**

```rust
// src/lib/shadow-shim-helper-rs/src/shim_shmem.rs
pub struct HostShmem {
    host_id: HostId,
    sim_time: AtomicEmulatedTime,  // 当前模拟时间
    protected: SelfContainedMutex<HostShmemProtected>,
}

pub struct ProcessShmem {
    host_id: HostId,
    host_shmem: ShMemBlockSerialized,  // 指向 HostShmem
    protected: RootedRefCell<ProcessShmemProtected>,
}
```

**系统调用拦截示例（clock_gettime）：**

```c
// shim 库中
int clock_gettime(clockid_t clockid, struct timespec *tp) {
    // 1. 从共享内存读取当前模拟时间
    HostShmem* host_shmem = get_host_shmem();
    EmulatedTime sim_time = atomic_load(&host_shmem->sim_time);
    
    // 2. 转换为 timespec
    *tp = emulated_time_to_timespec(sim_time);
    
    return 0;
}
```

### 3.5 内存管理（MemoryMapper）

Shadow 使用 `MemoryMapper` 来管理进程的内存映射：

```rust
// src/main/host/memory_manager/memory_mapper.rs
impl MemoryMapper {
    pub fn new(memory_manager: &mut MemoryManager, ctx: &ThreadContext) -> MemoryMapper {
        // 1. 创建共享内存文件（memfd）
        let shm_file = rustix::fs::memfd_create(&shm_name, MemfdFlags::CLOEXEC)?;
        
        // 2. 获取进程的内存区域（代码段、数据段、堆栈等）
        let regions = get_regions(memory_manager.pid);
        
        // 3. 合并相邻区域
        let regions = coalesce_regions(regions);
        
        // 4. 映射堆和栈
        map_stack(memory_manager, ctx, &mut shm_file, &mut regions);
        
        MemoryMapper { shm_file, regions, ... }
    }
}
```

**关键点：**
- 使用 `memfd_create` 创建匿名共享内存
- 通过 `/proc/PID/fd/FD` 让子进程访问
- 实现 Shadow 主进程和模拟进程之间的内存共享

---

## 第四部分：Rust 实现细节

### 4.1 所有权和生命周期

Shadow 大量使用 Rust 的所有权系统来保证内存安全：

**RootedRc 和 RootedRefCell：**
```rust
// 用于在多个 host 之间安全共享对象
pub struct RootedRc<Root, T> { ... }
pub struct RootedRefCell<Root, T> { ... }
```

**为什么需要这些？**
- Shadow 需要在多个线程之间共享 Host 和 Process
- 使用 `Root` 类型参数来跟踪对象的生命周期
- 防止悬垂指针和内存泄漏

### 4.2 线程安全

**关键同步原语：**
- `Arc<Mutex<EventQueue>>`：事件队列的线程安全访问
- `AtomicEmulatedTime`：原子操作的模拟时间
- `SelfContainedMutex`：自包含的互斥锁（用于共享内存）

### 4.3 错误处理

Shadow 使用 `anyhow::Result` 进行错误处理：
- 提供详细的错误上下文
- 使用 `context()` 添加错误信息
- 自动生成错误链

---

## 第五部分：性能优化

### 5.1 事件队列优化

- 使用 `BinaryHeap` 实现 O(log n) 的插入和删除
- 批量处理事件，减少锁竞争
- 使用 `runahead` 减少同步点

### 5.2 并行执行

- 每个 host 独立的事件队列
- 使用线程池并行执行多个 host
- 最小化线程间的同步开销

### 5.3 内存优化

- 代码段共享（多个进程共享同一份代码）
- 使用共享内存进行进程间通信
- 延迟分配内存映射

---

## 总结

Shadow 的核心设计：

1. **事件驱动**：使用离散事件模拟，按时间顺序处理事件
2. **Runahead 并行**：通过时间窗口实现高效的并行执行
3. **轻量虚拟化**：直接执行真实进程，通过 shim 拦截系统调用
4. **代码段共享**：多个进程共享代码段，大幅减少内存占用

这些设计使得 Shadow 能够：
- 模拟数千个进程
- 保持高精度的时间模拟
- 实现高效的并行执行
- 使用较少的内存资源

