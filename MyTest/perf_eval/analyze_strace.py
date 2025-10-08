#!/usr/bin/env python3
import re
from collections import Counter
import sys

def parse_strace_line(line):
    """解析strace行，提取syscall名称"""
    # 匹配格式: 00:00:21.000000000 [tid 1000] syscall_name(...) = 0
    match = re.match(r'^[\d:.]+\s+\[tid\s+\d+\]\s+(\w+)\([^)]*\)\s*=\s*', line)
    return match.group(1) if match else None

def analyze_strace_file(filename):
    """分析strace文件，统计syscall频率"""
    syscalls = Counter()

    with open(filename, 'r') as f:
        for line in f:
            syscall = parse_strace_line(line.strip())
            if syscall:
                syscalls[syscall] += 1

    return syscalls

def main():
    if len(sys.argv) != 2:
        print("用法: python3 analyze_strace.py <strace文件>")
        sys.exit(1)

    filename = sys.argv[1]
    print(f"分析文件: {filename}")

    syscalls = analyze_strace_file(filename)
    total = sum(syscalls.values())

    print(f"\n总系统调用数: {total:,}")
    print("\n最频繁的系统调用 (前20):")
    print("=" * 50)

    for name, count in syscalls.most_common(20):
        pct = count / total * 100
        print(f"{name:<20} {count:>10,} ({pct:>6.2f}%)")

    # 分析瓶颈类型
    print("\n瓶颈分析:")
    print("=" * 50)

    # 时间相关syscall
    time_syscalls = ['clock_gettime', 'gettimeofday', 'time', 'clock_nanosleep', 'nanosleep']
    time_count = sum(syscalls.get(s, 0) for s in time_syscalls)
    time_pct = time_count / total * 100
    print(f"时间相关syscall: {time_count:,} ({time_pct:.2f}%)")

    # 锁相关syscall
    lock_syscalls = ['futex']
    lock_count = sum(syscalls.get(s, 0) for s in lock_syscalls)
    lock_pct = lock_count / total * 100
    print(f"锁相关syscall: {lock_count:,} ({lock_pct:.2f}%)")

    # I/O相关syscall
    io_syscalls = ['read', 'write', 'pread64', 'pwrite64', 'recvfrom', 'sendto']
    io_count = sum(syscalls.get(s, 0) for s in io_syscalls)
    io_pct = io_count / total * 100
    print(f"I/O相关syscall: {io_count:,} ({io_pct:.2f}%)")

    # 网络相关syscall
    net_syscalls = ['socket', 'bind', 'listen', 'accept', 'connect', 'send', 'recv', 'poll', 'epoll_wait', 'epoll_ctl']
    net_count = sum(syscalls.get(s, 0) for s in net_syscalls)
    net_pct = net_count / total * 100
    print(f"网络相关syscall: {net_count:,} ({net_pct:.2f}%)")

if __name__ == "__main__":
    main()
