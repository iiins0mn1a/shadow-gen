# Checkpoint Network Ethereum Multiprocess

This test models an Ethereum-like node layout where each Shadow host runs
multiple cooperating processes:

- `execution`: local TCP RPC server
- `beacon`: local TCP RPC client/server plus cross-host P2P TCP/UDP
- `validator`: local TCP RPC client

The workload also uses `epoll`, `eventfd`, and `timerfd` in each process so
that checkpoint/restore covers richer async-runtime descriptor state than the
earlier full-network smoke tests.
