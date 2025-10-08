#!/bin/bash
# Shadow性能分析脚本

set -e

MYTEST_DIR="/home/ins0/Repos/shadow-gen/MyTest"
cd "$MYTEST_DIR"

# 清理旧数据
rm -rf shadow.data
rm -f perf_*.log perf.data*

echo "=== 启动性能监测 ==="
echo "1. 使用perf记录CPU事件"
echo "2. 使用strace记录syscall"
echo "3. 运行2分钟模拟以快速获取数据"
echo ""

# 创建临时配置文件（2分钟模拟）
cat > shadow_perf.yaml << 'EOF'
general:
  stop_time: 2m
  model_unblocked_syscall_latency: true
  log_level: warning
  parallelism: 16
  progress: true
network:
  graph:
    type: gml
    inline: |
      graph [
        directed 0
        node [
          id 0
          host_bandwidth_up "10 Gbit"
          host_bandwidth_down "10 Gbit"
        ]
        node [
          id 1
          host_bandwidth_up "10 Gbit"
          host_bandwidth_down "10 Gbit"
        ]
        edge [
          source 0
          target 0
          latency "10 ms"
          packet_loss 0.0
        ]
        edge [
          source 0
          target 1
          latency "10 ms"
          packet_loss 0.0
        ]
        edge [
          source 1
          target 1
          latency "10 ms"
          packet_loss 0.0
        ]
      ]
hosts:
  prysm-genesis:
    network_node_id: 0
    processes:
    - path: /home/ins0/Learning/Testnet/LocalTry/ethereum-pos-testnet/dependencies/prysm/bazel-bin/cmd/prysmctl/prysmctl_/prysmctl
      args: testnet generate-genesis --fork=deneb --num-validators=10 --chain-config-file=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/config.yml --geth-genesis-json-in=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/genesis.json --output-ssz=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/genesis.ssz --geth-genesis-json-out=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/genesis.json
      start_time: 1
  create-network-db:
    network_node_id: 0
    processes:
    - path: /home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/scripts/create-network-db.sh
      start_time: 5
  geth-bootnode:
    network_node_id: 0
    processes:
    - path: /home/ins0/Learning/Testnet/LocalTry/ethereum-pos-testnet/dependencies/go-ethereum/build/bin/bootnode
      args: -genkey /home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/bootnode/nodekey
      start_time: 8
    - path: /home/ins0/Learning/Testnet/LocalTry/ethereum-pos-testnet/dependencies/go-ethereum/build/bin/bootnode
      args: -nodekey /home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/bootnode/nodekey -addr=:30301 -verbosity=5
      start_time: 10
  geth-account:
    network_node_id: 0
    processes:
    - path: /home/ins0/Learning/Testnet/LocalTry/ethereum-pos-testnet/dependencies/go-ethereum/build/bin/geth
      args: account new --datadir /home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/execution --password /home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/geth_password.txt
      start_time: 12
  geth-genesis:
    network_node_id: 0
    processes:
    - path: /home/ins0/Learning/Testnet/LocalTry/ethereum-pos-testnet/dependencies/go-ethereum/build/bin/geth
      args: init --datadir=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/execution /home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/execution/genesis.json
      start_time: 14
  geth-node:
    network_node_id: 0
    ip_addr: 11.0.0.10
    processes:
    - path: /home/ins0/Learning/Testnet/LocalTry/ethereum-pos-testnet/dependencies/go-ethereum/build/bin/geth
      args: --networkid=32382 --http --http.api=eth,net,web3 --http.addr=0.0.0.0 --http.corsdomain="*" --http.port=8000 --port=8400 --metrics.port=8300 --ws --ws.api=eth,net,web3 --ws.addr=0.0.0.0 --ws.origins="*" --ws.port=8100 --authrpc.vhosts="*" --authrpc.addr=0.0.0.0 --authrpc.jwtsecret=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/execution/jwtsecret --authrpc.port=8200 --datadir=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/execution --password=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/geth_password.txt --identity=node-0 --maxpendpeers=0 --verbosity=2 --syncmode=full --ipcdisable --nodiscover --maxpeers=0 --nat=none
      start_time: 16
  prysm-beacon-1:
    network_node_id: 0
    ip_addr: 11.0.0.20
    processes:
    - path: /home/ins0/Learning/Testnet/LocalTry/ethereum-pos-testnet/dependencies/prysm/bazel-bin/cmd/beacon-chain/beacon-chain_/beacon-chain
      args: --datadir=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/consensus/beacondata --min-sync-peers=0 --p2p-max-peers=5 --genesis-state=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/consensus/genesis.ssz --interop-eth1data-votes --chain-config-file=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/consensus/config.yml --contract-deployment-block=0 --chain-id=32382 --rpc-host=11.0.0.20 --rpc-port=4000 --grpc-gateway-host=11.0.0.20 --grpc-gateway-port=4100 --execution-endpoint=http://11.0.0.10:8200 --accept-terms-of-use --jwt-secret=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/execution/jwtsecret --suggested-fee-recipient=0x123463a4b065722e99115d6c222f267d9cabb524 --minimum-peers-per-subnet=0 --p2p-tcp-port=4200 --p2p-udp-port=4300 --p2p-host-ip=11.0.0.20 --monitoring-port=4400 --verbosity=warn
      start_time: 21
  prysm-validator-1:
    network_node_id: 0
    ip_addr: 11.0.0.30
    processes:
    - path: /home/ins0/Learning/Testnet/LocalTry/ethereum-pos-testnet/dependencies/prysm/bazel-bin/cmd/validator/validator_/validator
      args: --beacon-rpc-provider=11.0.0.20:4000 --datadir=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/consensus/validatordata --accept-terms-of-use --interop-num-validators=5 --interop-start-index=0 --rpc-port=7000 --grpc-gateway-port=7100 --monitoring-port=7200 --graffiti="node-0" --chain-config-file=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/consensus/config.yml
      start_time: 26
  prysm-beacon-2:
    network_node_id: 1
    ip_addr: 11.0.0.21
    processes:
    - path: /home/ins0/Learning/Testnet/LocalTry/ethereum-pos-testnet/dependencies/prysm/bazel-bin/cmd/beacon-chain/beacon-chain_/beacon-chain
      args: --datadir=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/node-2/consensus/beacondata --min-sync-peers=0 --p2p-max-peers=5 --genesis-state=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/node-2/consensus/genesis.ssz --interop-eth1data-votes --chain-config-file=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/node-2/consensus/config.yml --contract-deployment-block=0 --chain-id=32382 --rpc-host=11.0.0.21 --rpc-port=4001 --grpc-gateway-host=11.0.0.21 --grpc-gateway-port=4101 --execution-endpoint=http://11.0.0.10:8200 --accept-terms-of-use --jwt-secret=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/the-node/execution/jwtsecret --suggested-fee-recipient=0x123463a4b065722e99115d6c222f267d9cabb524 --minimum-peers-per-subnet=0 --p2p-tcp-port=4201 --p2p-udp-port=4301 --p2p-host-ip=11.0.0.21 --monitoring-port=4401 --verbosity=warn --peer=/ip4/11.0.0.20/tcp/4200/p2p/16Uiu2HAkwKVPzETrAsYwUxUZ33vW8BekgXwHsu7ZnxdR9ZTuHYTS
      start_time: 21
  prysm-validator-2:
    network_node_id: 1
    ip_addr: 11.0.0.31
    processes:
    - path: /home/ins0/Learning/Testnet/LocalTry/ethereum-pos-testnet/dependencies/prysm/bazel-bin/cmd/validator/validator_/validator
      args: --beacon-rpc-provider=11.0.0.21:4001 --datadir=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/node-2/consensus/validatordata --accept-terms-of-use --interop-num-validators=5 --interop-start-index=5 --rpc-port=7000 --grpc-gateway-port=7100 --monitoring-port=7200 --graffiti="node-1" --chain-config-file=/home/ins0/Learning/Shadow-Test/Test-Alpha/05-testnet-full/network/node-2/consensus/config.yml
      start_time: 26
EOF

echo "=== 使用perf record进行性能采样 ==="
# 使用perf record捕获CPU事件，频率1000Hz
perf record -F 1000 -g --call-graph dwarf -o perf.data -- shadow --strace-logging-mode=standard shadow_perf.yaml > perf_run.log 2>&1 || true

echo ""
echo "=== 生成perf报告 ==="
# 生成详细的perf报告
perf report --stdio -g graph,0.5,caller --sort comm,dso,symbol -i perf.data > perf_report.txt 2>&1 || true

# 生成按函数统计的报告
perf report --stdio --sort comm,symbol -i perf.data | head -200 > perf_functions.txt 2>&1 || true

echo ""
echo "=== 分析syscall频率 ==="
# 统计所有进程的syscall调用
if [ -d "shadow.data/hosts" ]; then
    echo ">>> Syscall统计分析 <<<" > syscall_analysis.txt
    for host_dir in shadow.data/hosts/*/; do
        host=$(basename "$host_dir")
        echo "" >> syscall_analysis.txt
        echo "=== Host: $host ===" >> syscall_analysis.txt
        
        # 统计strace文件中的syscall
        if ls "$host_dir"/*.strace 2>/dev/null | head -1 >/dev/null; then
            cat "$host_dir"/*.strace 2>/dev/null | \
                grep -oP '^\w+' | \
                sort | uniq -c | sort -rn | head -30 >> syscall_analysis.txt || true
        else
            echo "No strace files found" >> syscall_analysis.txt
        fi
    done
    
    # 全局syscall统计
    echo "" >> syscall_analysis.txt
    echo "=== 全局Syscall频率TOP 50 ===" >> syscall_analysis.txt
    find shadow.data/hosts -name "*.strace" -exec cat {} \; 2>/dev/null | \
        grep -oP '^\w+' | \
        sort | uniq -c | sort -rn | head -50 >> syscall_analysis.txt || true
fi

echo ""
echo "=== 性能分析完成 ==="
echo "生成的文件："
echo "  - perf.data: perf原始数据"
echo "  - perf_report.txt: 详细性能报告"
echo "  - perf_functions.txt: 函数级性能统计"
echo "  - syscall_analysis.txt: Syscall频率分析"
echo "  - perf_run.log: Shadow运行日志"
echo ""
echo "查看热点函数："
echo "  head -100 perf_functions.txt"
echo ""
echo "查看syscall统计："
echo "  cat syscall_analysis.txt"

