#!/usr/bin/env python3
"""
Shadow性能瓶颈分析工具
分析syscall频率、热点函数、事件队列等
"""

import os
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

def analyze_strace_files(shadow_data_dir):
    """分析strace文件，统计syscall频率"""
    print("=" * 80)
    print("📊 SYSCALL 频率分析")
    print("=" * 80)
    
    hosts_dir = Path(shadow_data_dir) / "hosts"
    if not hosts_dir.exists():
        print("❌ shadow.data/hosts 目录不存在")
        return
    
    # 全局统计
    global_syscalls = Counter()
    host_syscalls = defaultdict(Counter)
    
    # 遍历所有host目录
    for host_dir in hosts_dir.iterdir():
        if not host_dir.is_dir():
            continue
        
        host_name = host_dir.name
        
        # 查找strace文件
        strace_files = list(host_dir.glob("*.strace"))
        
        for strace_file in strace_files:
            try:
                with open(strace_file, 'r', errors='ignore') as f:
                    for line in f:
                        # 提取syscall名称（行首的单词）
                        match = re.match(r'^(\w+)', line.strip())
                        if match:
                            syscall = match.group(1)
                            global_syscalls[syscall] += 1
                            host_syscalls[host_name][syscall] += 1
            except Exception as e:
                print(f"⚠️  读取 {strace_file} 失败: {e}")
    
    # 打印全局统计
    print("\n🌍 全局 Syscall TOP 30:")
    print(f"{'排名':<6} {'Syscall':<25} {'调用次数':<15} {'占比':<10}")
    print("-" * 80)
    
    total_calls = sum(global_syscalls.values())
    for i, (syscall, count) in enumerate(global_syscalls.most_common(30), 1):
        percentage = (count / total_calls * 100) if total_calls > 0 else 0
        print(f"{i:<6} {syscall:<25} {count:<15,} {percentage:>6.2f}%")
    
    print(f"\n总计: {total_calls:,} 次syscall调用")
    
    # 找出高频syscall（占比>5%）
    print("\n🔥 高频 Syscall (>5%):")
    high_freq = [(s, c, c/total_calls*100) for s, c in global_syscalls.most_common() 
                 if c/total_calls > 0.05]
    for syscall, count, pct in high_freq:
        print(f"  • {syscall}: {count:,} 次 ({pct:.1f}%)")
    
    # 按host分组统计
    print("\n📦 各Host的Top Syscall:")
    for host_name in sorted(host_syscalls.keys()):
        syscalls = host_syscalls[host_name]
        host_total = sum(syscalls.values())
        top3 = syscalls.most_common(3)
        print(f"\n  {host_name} (总计: {host_total:,}):")
        for syscall, count in top3:
            pct = count / host_total * 100
            print(f"    - {syscall}: {count:,} ({pct:.1f}%)")
    
    return global_syscalls, host_syscalls

def analyze_perf_report(perf_file):
    """分析perf报告，找出热点函数"""
    print("\n" + "=" * 80)
    print("🔥 热点函数分析 (基于perf)")
    print("=" * 80)
    
    if not Path(perf_file).exists():
        print(f"❌ {perf_file} 不存在")
        return
    
    print(f"\n从 {perf_file} 读取...")
    
    # 解析perf report输出
    functions = []
    with open(perf_file, 'r', errors='ignore') as f:
        in_data = False
        for line in f:
            # 跳过头部
            if re.match(r'^\s*#', line) or not line.strip():
                continue
            if re.match(r'^\s*\d+\.\d+%', line):
                in_data = True
            
            if in_data:
                # 提取百分比和函数名
                match = re.match(r'^\s*([\d.]+)%\s+(\S+)\s+(.+)', line)
                if match:
                    percentage = float(match.group(1))
                    comm = match.group(2)
                    symbol = match.group(3).strip()
                    if percentage > 0.5:  # 只关注>0.5%的
                        functions.append((percentage, comm, symbol))
    
    if not functions:
        print("⚠️  未找到性能数据，可能perf report格式不符")
        return
    
    # 按百分比排序
    functions.sort(reverse=True)
    
    print(f"\n🎯 热点函数 TOP 30 (>0.5% CPU):")
    print(f"{'排名':<6} {'CPU%':<10} {'进程':<20} {'函数':<50}")
    print("-" * 100)
    
    for i, (pct, comm, symbol) in enumerate(functions[:30], 1):
        print(f"{i:<6} {pct:>6.2f}%   {comm:<20} {symbol[:50]}")
    
    # 分类统计
    shadow_cpu = sum(pct for pct, comm, _ in functions if 'shadow' in comm.lower())
    geth_cpu = sum(pct for pct, comm, _ in functions if 'geth' in comm.lower())
    beacon_cpu = sum(pct for pct, comm, _ in functions if 'beacon' in comm.lower())
    validator_cpu = sum(pct for pct, comm, _ in functions if 'validator' in comm.lower())
    
    print(f"\n📊 CPU分布:")
    print(f"  • Shadow:     {shadow_cpu:>6.2f}%")
    print(f"  • Geth:       {geth_cpu:>6.2f}%")
    print(f"  • Beacon:     {beacon_cpu:>6.2f}%")
    print(f"  • Validator:  {validator_cpu:>6.2f}%")

def find_optimization_opportunities(global_syscalls):
    """基于syscall分析找出优化机会"""
    print("\n" + "=" * 80)
    print("💡 优化建议")
    print("=" * 80)
    
    total = sum(global_syscalls.values())
    
    # 检查常见的性能杀手
    issues = []
    
    # 检查时间相关syscall
    time_syscalls = ['clock_gettime', 'gettimeofday', 'time', 'clock_nanosleep']
    time_count = sum(global_syscalls.get(s, 0) for s in time_syscalls)
    if time_count / total > 0.1:
        issues.append(f"⚠️  时间相关syscall占比过高 ({time_count/total*100:.1f}%)")
        issues.append(f"   建议: 考虑批量处理时间请求，减少频繁的时间查询")
    
    # 检查futex（锁竞争）
    if global_syscalls.get('futex', 0) / total > 0.05:
        issues.append(f"⚠️  Futex占比高 ({global_syscalls['futex']/total*100:.1f}%) - 可能存在锁竞争")
        issues.append(f"   建议: 检查是否可以减少锁的使用，优化并发策略")
    
    # 检查poll/epoll（可能的忙等待）
    poll_syscalls = ['poll', 'epoll_wait', 'epoll_ctl', 'select']
    poll_count = sum(global_syscalls.get(s, 0) for s in poll_syscalls)
    if poll_count / total > 0.15:
        issues.append(f"⚠️  Poll/Epoll占比高 ({poll_count/total*100:.1f}%) - 可能存在忙等待")
        issues.append(f"   建议: 检查事件循环效率，是否可以增加超时时间")
    
    # 检查read/write频率
    io_syscalls = ['read', 'write', 'readv', 'writev', 'pread64', 'pwrite64']
    io_count = sum(global_syscalls.get(s, 0) for s in io_syscalls)
    if io_count / total > 0.2:
        issues.append(f"⚠️  I/O syscall占比高 ({io_count/total*100:.1f}%)")
        issues.append(f"   建议: 考虑增加缓冲区大小，减少I/O调用次数")
    
    # 检查mmap/munmap（内存管理）
    mem_syscalls = ['mmap', 'munmap', 'madvise', 'mprotect']
    mem_count = sum(global_syscalls.get(s, 0) for s in mem_syscalls)
    if mem_count / total > 0.05:
        issues.append(f"⚠️  内存管理syscall占比高 ({mem_count/total*100:.1f}%)")
        issues.append(f"   建议: 检查是否有频繁的内存分配/释放，考虑使用内存池")
    
    if issues:
        print("\n发现的潜在问题:")
        for issue in issues:
            print(issue)
    else:
        print("\n✅ 未发现明显的syscall瓶颈")
    
    # 额外的优化建议
    print("\n\n🚀 通用优化方向:")
    print("1. 减少网络延迟配置 (当前10ms) -> 尝试1ms或100us")
    print("2. 检查是否可以合并小的网络包，减少包数量")
    print("3. 考虑使用更高效的序列化格式")
    print("4. 检查是否有不必要的日志写入（即使是warning级别）")
    print("5. 考虑使用tmpfs存储临时数据，减少磁盘I/O")

def main():
    mytest_dir = Path(__file__).parent
    shadow_data = mytest_dir / "shadow.data"
    perf_report = mytest_dir / "perf_functions.txt"
    
    print("🔍 Shadow 性能瓶颈分析工具")
    print(f"📁 工作目录: {mytest_dir}")
    print()
    
    # 分析syscall
    global_syscalls, host_syscalls = analyze_strace_files(shadow_data)
    
    # 分析perf数据
    analyze_perf_report(perf_report)
    
    # 提供优化建议
    if global_syscalls:
        find_optimization_opportunities(global_syscalls)
    
    print("\n" + "=" * 80)
    print("✅ 分析完成")
    print("=" * 80)

if __name__ == "__main__":
    main()

