#!/bin/bash

# Shadow时间调用性能实验
# 对比原生Linux vs Shadow环境下的clock_gettime开销

set -e

SHADOW_BIN="/home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow"
RESULTS_DIR="experiment_results"

mkdir -p "$RESULTS_DIR"

echo "=========================================="
echo "Shadow时间调用性能实验"
echo "=========================================="
echo ""

# 编译测试程序
echo "[步骤1] 编译测试程序..."
make clean
make
echo "✓ 编译完成"
echo ""

# 测试1：原生Linux性能
echo "[步骤2] 原生Linux性能测试..."
echo "----------------------------------------"
./time_benchmark 100000 | tee "$RESULTS_DIR/native_test.log"
echo ""

# 测试2：Shadow模拟（无strace）
echo "[步骤3] Shadow模拟性能测试..."
echo "----------------------------------------"
rm -rf shadow.data
time "$SHADOW_BIN" shadow_time_test.yaml 2>&1 | tee "$RESULTS_DIR/shadow_test.log"
echo ""

# 提取输出
if [ -f "shadow.data/hosts/time-test-frequent/time_benchmark.1000.stdout" ]; then
    echo "[步骤4] Shadow模拟结果:"
    echo "----------------------------------------"
    cat shadow.data/hosts/time-test-frequent/time_benchmark.1000.stdout | tee "$RESULTS_DIR/shadow_output.log"
    echo ""
fi

# 测试3：Shadow + strace分析
echo "[步骤5] Shadow + Strace分析..."
echo "----------------------------------------"
rm -rf shadow.data
"$SHADOW_BIN" --strace-logging-mode=standard shadow_time_test.yaml 2>&1 > /dev/null

if [ -f "shadow.data/hosts/time-test-frequent/time_benchmark.1000.strace" ]; then
    echo "分析syscall频率..."
    python3 ../analyze_strace.py shadow.data/hosts/time-test-frequent/time_benchmark.1000.strace | tee "$RESULTS_DIR/strace_analysis.log"
    echo ""
fi

# 生成对比报告
echo "[步骤6] 生成对比报告..."
echo "=========================================="
cat > "$RESULTS_DIR/comparison_report.txt" << 'EOF'
时间调用性能对比报告
========================================

1. 原生Linux性能：
EOF

grep "测试1" "$RESULTS_DIR/native_test.log" -A2 >> "$RESULTS_DIR/comparison_report.txt"

cat >> "$RESULTS_DIR/comparison_report.txt" << 'EOF'

2. Shadow模拟性能：
EOF

grep "测试1" "$RESULTS_DIR/shadow_output.log" -A2 >> "$RESULTS_DIR/comparison_report.txt" 2>/dev/null || echo "数据不可用" >> "$RESULTS_DIR/comparison_report.txt"

cat >> "$RESULTS_DIR/comparison_report.txt" << 'EOF'

3. Syscall统计：
EOF

grep "clock_gettime" "$RESULTS_DIR/strace_analysis.log" >> "$RESULTS_DIR/comparison_report.txt" 2>/dev/null || echo "数据不可用" >> "$RESULTS_DIR/comparison_report.txt"

echo ""
echo "=========================================="
echo "实验完成！结果已保存到: $RESULTS_DIR/"
echo "=========================================="
echo ""
echo "关键文件："
echo "  - $RESULTS_DIR/native_test.log       (原生Linux测试)"
echo "  - $RESULTS_DIR/shadow_output.log     (Shadow模拟输出)"
echo "  - $RESULTS_DIR/strace_analysis.log   (Syscall分析)"
echo "  - $RESULTS_DIR/comparison_report.txt (对比报告)"
echo ""

cat "$RESULTS_DIR/comparison_report.txt"

