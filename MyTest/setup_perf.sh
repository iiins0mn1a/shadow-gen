#!/bin/bash

# 在WSL2环境中配置perf工具

echo "=========================================="
echo "配置perf性能分析工具"
echo "=========================================="
echo ""

# 检查是否在WSL环境
if ! grep -qi microsoft /proc/version; then
    echo "⚠️  警告：似乎不在WSL环境中"
    echo ""
fi

# 获取内核版本
KERNEL_VERSION=$(uname -r)
echo "当前内核版本: $KERNEL_VERSION"
echo ""

# 方案1：尝试安装标准perf工具
echo "[方案1] 尝试安装linux-tools..."
echo "----------------------------------------"

sudo apt-get update
sudo apt-get install -y linux-tools-common linux-tools-generic 2>&1 | tee /tmp/perf_install.log

# 检查是否成功
if command -v perf &> /dev/null; then
    echo "✓ perf安装成功！"
    perf --version
    echo ""
else
    echo "✗ 标准安装失败，尝试替代方案..."
    echo ""
    
    # 方案2：使用linux-tools-generic
    echo "[方案2] 安装通用版本..."
    echo "----------------------------------------"
    
    # 查找可用的linux-tools版本
    echo "可用的linux-tools版本："
    apt-cache search linux-tools | grep "^linux-tools-[0-9]" | head -10
    echo ""
    
    # 尝试安装最新的通用版本
    LATEST_TOOLS=$(apt-cache search linux-tools | grep "^linux-tools-[0-9]" | sort -V | tail -1 | awk '{print $1}')
    if [ ! -z "$LATEST_TOOLS" ]; then
        echo "安装: $LATEST_TOOLS"
        sudo apt-get install -y "$LATEST_TOOLS"
        echo ""
    fi
fi

# 方案3：WSL2特定的perf配置
echo "[方案3] WSL2特定配置..."
echo "----------------------------------------"

# 创建perf符号链接（如果需要）
if [ -d "/usr/lib/linux-tools" ]; then
    PERF_BIN=$(find /usr/lib/linux-tools -name "perf" -type f 2>/dev/null | head -1)
    if [ ! -z "$PERF_BIN" ]; then
        echo "找到perf二进制文件: $PERF_BIN"
        sudo ln -sf "$PERF_BIN" /usr/local/bin/perf
        echo "✓ 创建符号链接到 /usr/local/bin/perf"
    fi
fi

# 检查perf功能
echo ""
echo "[检查] 验证perf功能..."
echo "----------------------------------------"

if command -v perf &> /dev/null; then
    echo "✓ perf命令可用"
    perf --version
    echo ""
    
    # 测试基本功能
    echo "测试perf stat..."
    perf stat -e cycles,instructions sleep 0.1 2>&1 | head -10
    echo ""
    
    # 检查权限
    echo "检查性能事件权限..."
    if [ -f /proc/sys/kernel/perf_event_paranoid ]; then
        PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid)
        echo "perf_event_paranoid = $PARANOID"
        
        if [ "$PARANOID" -gt 1 ]; then
            echo "⚠️  权限受限，尝试调整..."
            echo "运行: echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid"
            echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid
        fi
    else
        echo "⚠️  无法访问 /proc/sys/kernel/perf_event_paranoid"
        echo "这在WSL中是正常的，某些perf功能可能受限"
    fi
    echo ""
else
    echo "✗ perf命令不可用"
    echo ""
fi

# 方案4：使用替代工具
echo "[方案4] 安装替代性能分析工具..."
echo "----------------------------------------"

# 安装其他有用的性能工具
echo "安装其他性能工具..."
sudo apt-get install -y \
    strace \
    ltrace \
    sysstat \
    iotop \
    htop \
    time

echo "✓ 替代工具安装完成"
echo ""

# 生成perf测试脚本
cat > /tmp/test_perf.sh << 'PERF_TEST'
#!/bin/bash
echo "=== 测试perf功能 ==="

if ! command -v perf &> /dev/null; then
    echo "✗ perf不可用"
    exit 1
fi

echo "1. 基本统计 (perf stat)"
perf stat sleep 1 2>&1 | head -20
echo ""

echo "2. 尝试记录 (perf record)"
perf record -F 99 -g -- sleep 1 2>&1 | head -10
if [ -f perf.data ]; then
    echo "✓ perf record成功"
    perf report --stdio 2>&1 | head -20
    rm -f perf.data
else
    echo "✗ perf record失败（这在WSL中是常见的）"
fi
echo ""

echo "3. 可用事件"
perf list 2>&1 | head -30
PERF_TEST

chmod +x /tmp/test_perf.sh

echo "=========================================="
echo "配置完成！"
echo "=========================================="
echo ""

if command -v perf &> /dev/null; then
    echo "✓ perf已安装并可用"
    echo ""
    echo "运行以下命令测试perf："
    echo "  /tmp/test_perf.sh"
    echo ""
    echo "使用perf分析Shadow："
    echo "  cd /home/ins0/Repos/shadow-gen/MyTest"
    echo "  perf stat /home/ins0/Repos/all-shadows/shadow-time/build/src/main/shadow shadow.yaml"
    echo ""
else
    echo "⚠️  perf安装可能失败"
    echo ""
    echo "WSL2中的perf限制："
    echo "  - 某些硬件性能计数器不可用"
    echo "  - perf record可能无法工作"
    echo "  - perf stat通常可以使用"
    echo ""
    echo "替代方案："
    echo "  1. 使用strace分析syscall（已经在用）"
    echo "  2. 使用/usr/bin/time -v获取资源使用统计"
    echo "  3. 在原生Linux环境中运行perf"
    echo ""
fi

echo "有用的性能分析命令："
echo "  strace -c <cmd>              # 统计syscall"
echo "  /usr/bin/time -v <cmd>       # 详细资源使用"
echo "  perf stat <cmd>              # CPU性能统计"
echo "  perf record -g <cmd>         # 记录调用栈（可能不可用）"
echo ""

