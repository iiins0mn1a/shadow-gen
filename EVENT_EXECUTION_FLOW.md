# Shadow 事件执行流程详解

本文档详细解释 Shadow 如何处理每个事件的运行，包括轻量虚拟化、退出时机和 context 保存。

---

## 一、整体架构概览

Shadow 的事件执行采用**协作式多任务**模型：
- Shadow 主进程（Rust）控制模拟时间
- 真实进程（通过 fork+exec 创建）执行应用代码
- 通过**共享内存 + IPC 通道**进行通信
- 进程在系统调用时**主动让出控制权**给 Shadow

---

## 二、事件执行的主循环

### 2.1 Host::execute() - 事件处理循环

```rust
// src/main/host/host.rs:749
pub fn execute(&self, until: EmulatedTime) {
    loop {
        // 1. 从事件队列取出下一个事件
        let event = event_queue.pop();
        
        // 2. 检查是否超过时间窗口
        if event.time() >= until {
            break;  // 退出循环，返回给调度器
        }
        
        // 3. 检查 CPU 延迟（模拟 CPU 处理时间）
        if cpu_delay > ZERO {
            // 重新调度事件到未来
            event.set_time(event.time() + cpu_delay);
            self.push_local_event(event);
            continue;
        }
        
        // 4. 设置当前模拟时间
        Worker::set_current_time(event.time());
        
        // 5. 执行事件
        match event.data() {
            EventData::Packet(data) => {
                // 处理网络数据包
                self.route_incoming_packet(data.into());
            }
            EventData::Local(data) => {
                // 执行本地任务（通常是恢复进程执行）
                TaskRef::from(data).execute(self);
            }
        }
        
        // 6. 清除当前时间
        Worker::clear_current_time();
    }
}
```

**关键点：**
- `until` 参数是**时间窗口的结束时间**（由 runahead 算法决定）
- 每个 host 独立执行，直到时间窗口结束
- 事件按时间顺序处理，保证确定性

---

## 三、进程恢复执行流程

### 3.1 调用链

```
Host::execute()
  └─> TaskRef::execute(host)
      └─> Host::resume(pid, tid)
          └─> Process::resume(host, tid)
              └─> Thread::resume(ctx)
                  └─> ManagedThread::resume(ctx, syscall_handler)
                      └─> continue_plugin() [IPC 通信]
```

### 3.2 Host::resume() - 恢复进程执行

```rust
// src/main/host/host.rs:446
pub fn resume(&self, pid: ProcessId, tid: ThreadId) {
    // 1. 获取进程引用
    let processrc = self.process_borrow(pid);
    
    // 2. 设置当前活跃进程（保存到 Worker）
    Worker::set_active_process(&processrc);
    
    // 3. 调用进程的 resume 方法
    process.resume(self, tid);
    
    // 4. 清除活跃进程
    Worker::clear_active_process();
    
    // 5. 处理僵尸进程
    if process.is_zombie() {
        // 处理孤儿进程、清理资源等
    }
}
```

### 3.3 Process::resume() - 恢复线程执行

```rust
// src/main/host/process.rs:1215
pub fn resume(&self, host: &Host, tid: ThreadId) {
    // 1. 获取线程引用
    let threadrc = threads.get(&tid).clone();
    
    // 2. 设置当前活跃线程（保存到 Worker）
    Worker::set_active_thread(&threadrc);
    
    // 3. 更新共享内存中的模拟时间
    Process::set_shared_time(host);
    
    // 4. 创建进程上下文
    let ctx = ProcessContext::new(host, self);
    
    // 5. 恢复线程执行
    let res = thread.resume(&ctx);
    
    // 6. 处理执行结果
    match res {
        ResumeResult::Blocked => {
            // 线程被阻塞（等待系统调用完成）
        }
        ResumeResult::ExitedThread(return_code) => {
            // 线程退出
        }
        ResumeResult::ExitedProcess => {
            // 进程退出
        }
    }
    
    // 7. 清除活跃线程
    Worker::clear_active_thread();
}
```

**关键点：**
- `Worker::set_active_thread()` 将线程引用保存到**线程本地存储（TLS）**
- 这样其他代码（如日志系统）可以访问当前执行的线程
- 使用 `RootedRc` 和 `RootedRefCell` 管理生命周期

---

## 四、ManagedThread::resume() - 核心执行逻辑

### 4.1 主循环

```rust
// src/main/host/managed_thread.rs:187
pub fn resume(&self, ctx: &ThreadContext, syscall_handler: &mut SyscallHandler) -> ResumeResult {
    loop {
        // 1. 获取上次的事件（从 shim 发来的）
        let event = self.current_event.borrow();
        
        // 2. 处理事件
        match event {
            ShimEventToShadow::StartReq(_) => {
                // 初始化线程，设置共享内存
                // 发送 StartRes 给 shim
            }
            ShimEventToShadow::Syscall(syscall) => {
                // 处理系统调用
                return self.handle_syscall(ctx, syscall_handler, syscall);
            }
            ShimEventToShadow::ProcessDeath => {
                // 进程已退出
                return ResumeResult::ExitedProcess;
            }
        }
        
        // 3. 等待 shim 的下一个事件（阻塞在这里）
        let next_event = self.wait_for_next_event();
        self.current_event.replace(next_event);
    }
}
```

### 4.2 continue_plugin() - IPC 通信

这是 Shadow 和进程之间的**关键通信点**：

```rust
// src/main/host/managed_thread.rs:435
fn continue_plugin(&self, host: &Host, event: &ShimEventToShim) -> ShimEventToShadow {
    // 1. 更新共享内存状态
    host.shim_shmem_lock_borrow_mut().unwrap().max_runahead_time =
        Worker::max_event_runahead_time(host);
    host.shim_shmem().sim_time
        .store(Worker::current_time().unwrap(), atomic::Ordering::Relaxed);
    
    // 2. 释放共享内存锁（让 shim 可以访问）
    host.unlock_shmem();
    
    // 3. 发送事件给 shim（通过 IPC 通道）
    self.ipc_shmem.to_plugin().send(*event);
    
    // 4. 等待 shim 的响应（阻塞在这里）
    let event = self.ipc_shmem.from_plugin().receive();
    
    // 5. 重新获取共享内存锁
    host.lock_shmem();
    
    // 6. 更新模拟时间（shim 可能在执行过程中更新了时间）
    let shim_time = host.shim_shmem().sim_time.load(atomic::Ordering::Relaxed);
    Worker::set_current_time(shim_time);
    
    event
}
```

**关键点：**
- **共享内存锁**：Shadow 持有锁时，shim 不能访问共享内存
- **IPC 通道**：使用无锁队列进行双向通信
- **时间同步**：shim 可以在执行过程中更新模拟时间（例如处理定时器）

---

## 五、系统调用处理流程

### 5.1 Shim 拦截系统调用

当进程执行系统调用时：

```c
// shim 库中（通过 LD_PRELOAD 注入）
int syscall(...) {
    // 1. 创建系统调用事件
    ShimEventSyscall syscall_event = {
        .syscall_args = {...},
    };
    
    // 2. 发送给 Shadow（阻塞等待）
    ipc_to_shadow.send(ShimEventToShadow::Syscall(syscall_event));
    
    // 3. 等待 Shadow 的响应
    let response = ipc_from_shadow.receive();
    
    // 4. 返回系统调用结果
    return response.retval;
}
```

### 5.2 Shadow 处理系统调用

```rust
// src/main/host/managed_thread.rs:255
ShimEventToShadow::Syscall(syscall) => {
    // 1. 特殊处理 exit 系统调用
    if syscall.syscall_args.number == libc::SYS_exit {
        // 直接退出，不等待
        return ResumeResult::ExitedThread(return_code);
    }
    
    // 2. 调用系统调用处理器
    let scr = syscall_handler.syscall(ctx, &syscall.syscall_args);
    
    // 3. 处理系统调用结果
    match scr {
        SyscallReturn::Block(cond) => {
            // 系统调用被阻塞（例如等待网络数据）
            return ResumeResult::Blocked(cond);
        }
        SyscallReturn::Done(d) => {
            // 系统调用完成，发送结果给 shim
            self.continue_plugin(host, &ShimEventToShim::SyscallComplete {
                retval: d.retval,
            });
        }
        SyscallReturn::Native => {
            // 需要执行真实的系统调用（例如文件 I/O）
            self.continue_plugin(host, &ShimEventToShim::SyscallDoNative);
        }
    }
}
```

---

## 六、Context 保存和恢复

### 6.1 Worker 的 Context 管理

Worker 使用**线程本地存储（TLS）**保存当前执行的上下文：

```rust
// src/main/core/worker.rs:57
pub struct Worker {
    active_host: RefCell<Option<Box<Host>>>,
    active_process: RefCell<Option<RootedRc<RootedRefCell<Process>>>>,
    active_thread: RefCell<Option<RootedRc<RootedRefCell<Thread>>>>,
    clock: RefCell<Clock>,  // 当前模拟时间
}

// 设置当前活跃线程
pub fn set_active_thread(thread: &RootedRc<RootedRefCell<Thread>>) {
    WORKER.with(|w| {
        w.active_thread.replace(Some(thread.clone()));
    });
}

// 清除当前活跃线程
pub fn clear_active_thread() {
    WORKER.with(|w| {
        w.active_thread.replace(None);
    });
}
```

### 6.2 进程 Context 的保存

**不需要显式保存进程的 CPU 状态**，因为：
1. 进程是**真实进程**，CPU 状态由内核保存
2. 进程在系统调用时**主动让出控制权**
3. 系统调用返回时，内核自动恢复 CPU 状态

**需要保存的是 Shadow 的状态：**
- 当前模拟时间（保存在共享内存中）
- 文件描述符状态（Shadow 管理的虚拟文件系统）
- 网络连接状态（Shadow 管理的虚拟网络）
- 系统调用条件（等待条件）

### 6.3 共享内存中的 Context

```rust
// src/lib/shadow-shim-helper-rs/src/shim_shmem.rs
pub struct HostShmem {
    sim_time: AtomicEmulatedTime,  // 当前模拟时间
    protected: SelfContainedMutex<HostShmemProtected>,
}

pub struct HostShmemProtected {
    max_runahead_time: EmulatedTime,  // 最大运行时间
    unapplied_cpu_latency: SimulationTime,  // 未应用的 CPU 延迟
}

pub struct ProcessShmem {
    host_shmem: ShMemBlockSerialized,  // 指向 HostShmem
    protected: RootedRefCell<ProcessShmemProtected>,
}
```

---

## 七、退出时机

### 7.1 何时退出 execute() 循环

```rust
// Host::execute() 在以下情况退出：
1. 事件队列为空
2. 下一个事件的时间 >= until（时间窗口结束）
3. 所有事件都已处理
```

### 7.2 进程何时退出

```rust
// Process::resume() 返回以下结果：
1. ResumeResult::Blocked
   - 进程被阻塞（等待系统调用完成）
   - 会创建一个 SyscallCondition，等待条件满足后重新调度

2. ResumeResult::ExitedThread(return_code)
   - 线程退出
   - 从线程列表中移除
   - 如果是最后一个线程，进程也退出

3. ResumeResult::ExitedProcess
   - 进程退出
   - 清理所有资源
   - 处理子进程（变为孤儿或重新分配父进程）
```

### 7.3 系统调用阻塞

当系统调用需要等待时（例如 `recv` 等待网络数据）：

```rust
// 1. 系统调用处理器返回 Block
SyscallReturn::Block(cond) => {
    // 2. 创建 SyscallCondition（等待条件）
    let cond = SyscallCondition::new(...);
    
    // 3. 返回 Blocked
    return ResumeResult::Blocked(cond);
}

// 4. 当条件满足时（例如网络数据到达）
//    会创建一个新的 TaskRef，调用 Host::resume()
//    重新执行进程
```

---

## 八、轻量虚拟化的实现

### 8.1 为什么是"轻量"的？

1. **不创建虚拟机**：直接使用真实进程
2. **不虚拟化 CPU**：使用真实 CPU 执行代码
3. **不虚拟化内存**：使用真实内存（代码段共享）
4. **只虚拟化系统调用**：拦截并模拟系统调用

### 8.2 代码段共享

```rust
// 多个进程共享同一可执行文件的代码段
// Linux 内核自动处理：
1. 代码段是只读的，可以安全共享
2. 数据段和堆栈是独立的（写时复制）
3. 1000 个相同进程 ≈ 1 份代码段内存
```

### 8.3 系统调用虚拟化

```rust
// 通过 LD_PRELOAD 注入 shim 库
// shim 库拦截所有系统调用：
1. 时间相关：clock_gettime, gettimeofday → 返回模拟时间
2. 网络相关：socket, send, recv → Shadow 管理的虚拟网络
3. 文件系统：open, read, write → Shadow 管理的虚拟文件系统
4. 进程管理：fork, clone, exec → Shadow 管理的虚拟进程
```

---

## 九、关键数据结构

### 9.1 TaskRef

```rust
// src/main/core/work/task.rs
pub struct TaskRef {
    inner: Arc<dyn Fn(&Host) + Send + Sync>,
}

// 执行任务
pub fn execute(&self, host: &Host) {
    (self.inner)(host)  // 通常是调用 Host::resume()
}
```

### 9.2 ProcessContext 和 ThreadContext

```rust
// src/main/host/context.rs
pub struct ProcessContext {
    host: *const Host,
    process: RootedRc<RootedRefCell<Process>>,
}

pub struct ThreadContext {
    host: *const Host,
    process: RootedRc<RootedRefCell<Process>>,
    thread: RootedRc<RootedRefCell<Thread>>,
}
```

---

## 十、总结

### 10.1 执行流程

1. **调度器**：为每个 host 分配时间窗口
2. **Host::execute()**：处理事件队列中的事件
3. **TaskRef::execute()**：执行任务（通常是恢复进程）
4. **Process::resume()**：恢复线程执行
5. **ManagedThread::resume()**：通过 IPC 与 shim 通信
6. **系统调用处理**：Shadow 模拟系统调用，shim 等待结果
7. **退出**：进程阻塞、退出或时间窗口结束

### 10.2 关键特性

- **协作式多任务**：进程在系统调用时主动让出控制权
- **事件驱动**：按时间顺序处理事件
- **并行执行**：多个 host 可以并行执行（runahead 算法）
- **轻量虚拟化**：只虚拟化系统调用，不虚拟化 CPU/内存

### 10.3 Context 管理

- **Worker TLS**：保存当前执行的 host/process/thread
- **共享内存**：Shadow 和进程之间的通信
- **IPC 通道**：事件驱动的双向通信
- **不需要保存 CPU 状态**：由内核自动管理

