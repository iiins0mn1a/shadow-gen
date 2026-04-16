# Checkpoint Network Ethereum Shadow-YAML

This test is a higher-fidelity synthetic checkpoint/restore gate derived from
`/home/ins0/Repos/Event-Driven-Testnet/shadow-ethereum/shadow.yaml`.

It mirrors the target deployment shape more closely than
`checkpoint-network-eth-multiproc`:

- `1` shared execution endpoint (`geth-node`)
- `4` beacon hosts
- `4` validator hosts
- one recorder helper process on each beacon host
- host-IP RPC instead of loopback-only RPC
- beacon peer discovery through a shared `beacon_peers.txt` file

The synthetic app still uses `epoll`, `eventfd`, and `timerfd`, so restore
continues to exercise the async-runtime state that has been the main source of
checkpoint/restore bugs.

The verifier supports:

- `--scenario stable`: full graph established before checkpoint
- `--scenario peer-bootstrap`: earlier checkpoint while the peer file is still converging
