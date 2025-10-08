#!/usr/bin/env python3
"""
Shadow Syscall分析工具 - 深度性能瓶颈分析
"""

import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

def parse_strace_line(line):
    """解析strace行，提取syscall名称"""
    # 格式: 时间戳 [tid xxx] syscall_name(...) = result
    match = re.match(r'^[\d:.]+\s+\[tid\s+\d+\]\s+(\w+)\(', line)
    if match:
        return match.group(1)
    return None

def analyze_syscall_frequency(shadow_data_dir):
    """分析syscall频率"""
    print("=" * 100)
    print("📊 SYSCALL 频率深度分析")
    print("=" * 100)
    
    hosts_dir = Path(shadow_data_dir) / "hosts"
    if not hosts_dir.exists():
        print("❌ shadow.data/hosts 目录不存在")
        return None, None
    
    global_syscalls = Counter()
    host_syscalls = defaultdict(Counter)
    host_total_lines = defaultdict(int)
    
    # 遍历所有host目录
    for host_dir in sorted(hosts_dir.iterdir()):
        if not host_dir.is_dir():
            continue
        
        host_name = host_dir.name
        strace_files = list(host_dir.glob("*.strace"))
        
        for strace_file in strace_files:
            try:
                with open(strace_file, 'r', errors='ignore') as f:
                    for line in f:
                        host_total_lines[host_name] += 1
                        syscall = parse_strace_line(line)
                        if syscall:
                            global_syscalls[syscall] += 1
                            host_syscalls[host_name][syscall] += 1
            except Exception as e:
                print(f"⚠️  读取 {strace_file} 失败: {e}")
    
    total_syscalls = sum(global_syscalls.values())
    total_lines = sum(host_total_lines.values())
    
    print(f"\n📈 统计概览:")
    print(f"  • 总syscall调用: {total_syscalls:,}")
    print(f"  • 总strace行数: {total_lines:,}")
    print(f"  • 解析成功率: {total_syscalls/total_lines*100:.1f}%")
    print(f"  • 不同syscall种类: {len(global_syscalls)}")
    
    # 全局TOP syscalls
    print(f"\n🌍 全局 Syscall TOP 50:")
    print(f"{'排名':<6} {'Syscall':<25} {'调用次数':<18} {'占比':<10} {'每秒调用':<12}")
    print("-" * 100)
    
    sim_time = 120  # 2分钟模拟
    for i, (syscall, count) in enumerate(global_syscalls.most_common(50), 1):
        percentage = (count / total_syscalls * 100) if total_syscalls > 0 else 0
        per_second = count / sim_time
        print(f"{i:<6} {syscall:<25} {count:<18,} {percentage:>6.2f}%    {per_second:>10,.0f}/s")
    
    # 高频syscall分析
    print(f"\n🔥 高频 Syscall (>1% 调用):")
    high_freq = [(s, c, c/total_syscalls*100) for s, c in global_syscalls.most_common() 
                 if c/total_syscalls > 0.01]
    
    for syscall, count, pct in high_freq:
        print(f"  • {syscall:<20}: {count:>12,} 次 ({pct:>5.1f}%)")
    
    # 按类别分组统计
    print(f"\n📦 Syscall 类别统计:")
    categories = {
        '时间相关': ['clock_gettime', 'gettimeofday', 'time', 'clock_nanosleep', 'nanosleep'],
        '内存管理': ['mmap', 'munmap', 'madvise', 'mprotect', 'brk'],
        '文件I/O': ['read', 'write', 'readv', 'writev', 'pread64', 'pwrite64', 'lseek', 'fsync'],
        '文件操作': ['open', 'openat', 'close', 'newfstatat', 'fstat', 'stat', 'access', 'faccessat'],
        '网络I/O': ['socket', 'bind', 'listen', 'accept', 'accept4', 'connect', 'send', 'sendto', 
                  'recv', 'recvfrom', 'sendmsg', 'recvmsg', 'setsockopt', 'getsockopt', 'getpeername', 'getsockname'],
        '事件轮询': ['poll', 'epoll_wait', 'epoll_ctl', 'epoll_create', 'select', 'ppoll'],
        '进程/线程': ['clone', 'fork', 'vfork', 'execve', 'wait4', 'waitid', 'futex', 'sched_yield'],
        '信号处理': ['rt_sigaction', 'rt_sigprocmask', 'rt_sigreturn', 'sigaltstack'],
    }
    
    category_stats = {}
    for cat_name, syscalls in categories.items():
        cat_count = sum(global_syscalls.get(s, 0) for s in syscalls)
        category_stats[cat_name] = cat_count
    
    # 排序并显示
    for cat_name, count in sorted(category_stats.items(), key=lambda x: x[1], reverse=True):
        pct = count / total_syscalls * 100 if total_syscalls > 0 else 0
        if count > 0:
            print(f"  {cat_name:<15}: {count:>12,} 次 ({pct:>5.1f}%)")
    
    # 按host统计
    print(f"\n🖥️  各Host的Syscall统计:")
    print(f"{'Host':<25} {'总调用':<15} {'Top-3 Syscalls':<60}")
    print("-" * 100)
    
    for host_name in sorted(host_syscalls.keys()):
        syscalls = host_syscalls[host_name]
        host_total = sum(syscalls.values())
        top3 = syscalls.most_common(3)
        top3_str = ', '.join([f"{s}({c:,})" for s, c in top3])
        print(f"{host_name:<25} {host_total:<15,} {top3_str:<60}")
    
    return global_syscalls, host_syscalls, category_stats

def identify_bottlenecks(global_syscalls, category_stats, total_syscalls):
    """识别性能瓶颈"""
    print("\n" + "=" * 100)
    print("🔍 性能瓶颈分析")
    print("=" * 100)
    
    issues = []
    recommendations = []
    
    # 1. 检查时间syscall
    time_pct = category_stats.get('时间相关', 0) / total_syscalls * 100
    if time_pct > 10:
        issues.append(f"⚠️  时间相关syscall占比过高: {time_pct:.1f}%")
        recommendations.append("• 考虑批量处理时间查询，减少频繁调用")
        recommendations.append("• Shadow的event-driven模型下，过多时间查询会降低加速比")
    
    # 2. 检查futex（锁竞争）
    futex_count = global_syscalls.get('futex', 0)
    futex_pct = futex_count / total_syscalls * 100
    if futex_pct > 5:
        issues.append(f"⚠️  Futex (锁)占比高: {futex_pct:.1f}% ({futex_count:,}次)")
        recommendations.append("• 存在严重的锁竞争，考虑优化并发策略")
        recommendations.append("• 检查是否可以使用无锁数据结构")
    
    # 3. 检查事件轮询
    poll_pct = category_stats.get('事件轮询', 0) / total_syscalls * 100
    if poll_pct > 15:
        issues.append(f"⚠️  事件轮询syscall占比高: {poll_pct:.1f}%")
        recommendations.append("• 可能存在忙等待或轮询过于频繁")
        recommendations.append("• 考虑增加poll/epoll的超时时间")
    
    # 4. 检查I/O频率
    io_pct = category_stats.get('文件I/O', 0) / total_syscalls * 100
    if io_pct > 20:
        issues.append(f"⚠️  文件I/O syscall占比高: {io_pct:.1f}%")
        recommendations.append("• 考虑增加缓冲区大小，减少I/O次数")
        recommendations.append("• 使用tmpfs存储临时数据可能提升性能")
    
    # 5. 检查内存管理
    mem_pct = category_stats.get('内存管理', 0) / total_syscalls * 100
    if mem_pct > 10:
        issues.append(f"⚠️  内存管理syscall占比高: {mem_pct:.1f}%")
        recommendations.append("• 频繁的内存分配/释放，考虑使用内存池")
        recommendations.append("• 检查是否有内存泄漏或不必要的重分配")
    
    # 打印发现的问题
    if issues:
        print("\n❌ 发现的性能问题:")
        for issue in issues:
            print(f"  {issue}")
    else:
        print("\n✅ 未发现明显的syscall层面瓶颈")
    
    # 打印建议
    if recommendations:
        print("\n💡 优化建议:")
        for rec in recommendations:
            print(f"  {rec}")
    
    # 计算理论加速比上限
    print("\n" + "=" * 100)
    print("📊 Shadow 加速比分析")
    print("=" * 100)
    
    sim_time = 120  # 秒
    total_events = total_syscalls  # 简化：每个syscall作为一个事件
    events_per_second = total_events / sim_time
    
    print(f"\n当前状态:")
    print(f"  • 模拟时间: {sim_time}秒")
    print(f"  • 总事件数: {total_events:,}")
    print(f"  • 平均事件率: {events_per_second:,.0f} 事件/秒")
    print(f"  • 当前加速比: ~6.5x")
    
    # 理论分析
    print(f"\n理论分析:")
    print(f"  • 如果slot时间从12s降到2s，区块数量将增加6倍")
    print(f"  • 但事件数量不会线性增长（很多是周期性的）")
    print(f"  • 主要瓶颈可能在于:")
    print(f"    1. Shadow的事件调度开销")
    print(f"    2. 网络包处理（每个包都是事件）")
    print(f"    3. 应用层的内部逻辑（时间查询、I/O等）")
    
    print(f"\n🎯 达到更高加速比的路径:")
    print(f"  1. 减少网络包数量（合并小包）")
    print(f"  2. 优化应用层时间查询频率")
    print(f"  3. 使用Shadow的批处理特性")
    print(f"  4. 减少不必要的syscall（如过度的stat/fstat）")

def main():
    mytest_dir = Path(__file__).parent
    shadow_data = mytest_dir / "shadow.data"
    
    print("🔍 Shadow 深度性能分析工具 v2.0")
    print(f"📁 工作目录: {mytest_dir}\n")
    
    # 分析syscall
    result = analyze_syscall_frequency(shadow_data)
    if result[0] is None:
        return 1
    
    global_syscalls, host_syscalls, category_stats = result
    total_syscalls = sum(global_syscalls.values())
    
    # 识别瓶颈
    identify_bottlenecks(global_syscalls, category_stats, total_syscalls)
    
    print("\n" + "=" * 100)
    print("✅ 分析完成!")
    print("=" * 100)
    
    return 0

if __name__ == "__main__":
    sys.exit(main())

