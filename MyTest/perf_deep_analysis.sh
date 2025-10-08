#!/bin/bash

PERF="/usr/lib/linux-tools-5.15.0-157/perf"
SHADOW="/home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow"
CONFIG="shadow.yaml"

echo "=== Shadow性能深度分析 ==="
echo ""

# 测试1：基础统计
echo "[测试1] 基础性能统计"
echo "--------------------"
rm -rf shadow.data
$PERF stat -e cycles,instructions,cache-references,cache-misses,branches,branch-misses,context-switches,cpu-migrations,page-faults,minor-faults,major-faults \
  $SHADOW $CONFIG 2>&1 | tee perf_basic_stats.log
echo ""

# 测试2：详细缓存分析
echo "[测试2] 缓存性能分析"
echo "--------------------"
rm -rf shadow.data
$PERF stat -e L1-dcache-loads,L1-dcache-load-misses,L1-icache-loads,L1-icache-load-misses,LLC-loads,LLC-load-misses,dTLB-loads,dTLB-load-misses \
  $SHADOW $CONFIG 2>&1 | tee perf_cache_stats.log
echo ""

# 测试3：分CPU统计
echo "[测试3] 分CPU核心统计"
echo "--------------------"
rm -rf shadow.data
$PERF stat -a -A $SHADOW $CONFIG 2>&1 | tee perf_per_cpu.log
echo ""

# 测试4：尝试采样（可能在WSL中失败）
echo "[测试4] 尝试采样分析 (可能失败)"
echo "--------------------"
rm -rf shadow.data
$PERF record -F 99 -g $SHADOW $CONFIG 2>&1 | tee perf_record.log
if [ -f perf.data ]; then
    echo "✓ 采样成功，生成报告..."
    $PERF report --stdio > perf_report.txt 2>&1
    echo "热点函数 (Top 20):"
    head -50 perf_report.txt
else
    echo "✗ 采样失败（WSL限制）"
fi
echo ""

# 汇总分析
echo "=== 性能汇总 ==="
echo "--------------------"
echo "1. 运行时间统计："
grep "seconds time elapsed" perf_basic_stats.log
grep "seconds user" perf_basic_stats.log  
grep "seconds sys" perf_basic_stats.log

echo ""
echo "2. IPC (Instructions Per Cycle):"
grep "insn per cycle" perf_basic_stats.log

echo ""
echo "3. Cache Miss率:"
grep "cache-misses" perf_basic_stats.log

echo ""
echo "4. Branch Miss率:"
grep "branch-misses" perf_basic_stats.log

echo ""
echo "5. Context Switches:"
grep "context-switches" perf_basic_stats.log

echo ""
echo "所有结果已保存到 perf_*.log 文件"




