#!/bin/bash

# Shadow以太坊测试网性能测试脚本

SHADOW_BIN="/home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow"
CONFIG="shadow.yaml"
RESULTS_DIR="performance_results"

mkdir -p "$RESULTS_DIR"

echo "=================================================="
echo "Shadow以太坊测试网性能测试"
echo "=================================================="
echo ""

# 测试1：基线性能（当前配置）
echo "=== 测试1：基线性能测试 ==="
rm -rf shadow.data
/usr/bin/time -v "$SHADOW_BIN" "$CONFIG" 2>&1 | tee "$RESULTS_DIR/baseline.log"
echo ""
sleep 2

# 测试2：禁用syscall延迟建模
echo "=== 测试2：禁用syscall延迟建模 ==="
cp shadow.yaml shadow_no_latency.yaml
sed -i 's/# model_unblocked_syscall_latency: true/model_unblocked_syscall_latency: false/' shadow_no_latency.yaml
rm -rf shadow.data
/usr/bin/time -v "$SHADOW_BIN" shadow_no_latency.yaml 2>&1 | tee "$RESULTS_DIR/no_syscall_latency.log"
echo ""
sleep 2

# 测试3：strace分析（短时间运行）
echo "=== 测试3：Strace分析（60秒超时） ==="
cp shadow.yaml shadow_short.yaml
sed -i 's/stop_time: 3m/stop_time: 30s/' shadow_short.yaml
rm -rf shadow.data
timeout 60 "$SHADOW_BIN" --strace-logging-mode=standard shadow_short.yaml 2>&1 | tee "$RESULTS_DIR/strace_run.log"

# 分析strace结果
if [ -f "shadow.data/hosts/prysm-beacon-1/beacon-chain.1000.strace" ]; then
    echo ""
    echo "=== Strace分析结果 ==="
    python3 analyze_strace.py shadow.data/hosts/prysm-beacon-1/beacon-chain.1000.strace | tee "$RESULTS_DIR/strace_analysis.txt"
fi
echo ""
sleep 2

# 汇总结果
echo "=================================================="
echo "性能测试汇总"
echo "=================================================="
echo ""

echo "测试1 - 基线性能："
grep "Elapsed" "$RESULTS_DIR/baseline.log" || echo "未找到时间数据"
grep "User time" "$RESULTS_DIR/baseline.log" || echo ""
grep "System time" "$RESULTS_DIR/baseline.log" || echo ""
echo ""

echo "测试2 - 禁用syscall延迟："
grep "Elapsed" "$RESULTS_DIR/no_syscall_latency.log" || echo "未找到时间数据"
grep "User time" "$RESULTS_DIR/no_syscall_latency.log" || echo ""
grep "System time" "$RESULTS_DIR/no_syscall_latency.log" || echo ""
echo ""

echo "测试3 - Strace分析："
if [ -f "$RESULTS_DIR/strace_analysis.txt" ]; then
    grep "clock_gettime" "$RESULTS_DIR/strace_analysis.txt" || echo "未找到clock_gettime数据"
fi
echo ""

echo "=================================================="
echo "所有结果已保存到: $RESULTS_DIR/"
echo "=================================================="

