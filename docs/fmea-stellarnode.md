# Failure Mode and Effects Analysis (FMEA) — StellarNode

Structured view of how a managed Stellar node can fail under Kubernetes and network stress, aligned with Chaos Mesh scenarios in [`tests/chaos/`](../tests/chaos/).

| ID | Failure mode | Cause / driver | Local effect | System effect | Detection | Mitigation (operator / ops) |
|----|----------------|----------------|-------------|-------------|-----------|------------------------------|
| F-01 | Operator pod killed | PodChaos, node loss | Reconcile pauses | Drift until new leader | Pod restarts, logs | kube-controller-manager; PDB if configured |
| F-02 | API server partition | NetworkChaos to apiserver | List/watch fail | No progress | API errors in logs | Automatic reconnect; widen timeouts |
| F-03 | API latency | NetworkChaos delay | Slow applies | Sync lag | Controller metrics | Tune client QPS; scale API |
| F-04 | Validator network split | NetworkChaos partition | Peers drop / stuck | Possible halt / catch-up | Core logs, Ready=false | Heal partition; optional quorum tooling |
| F-05 | Stellar Core OOM / crash | Resource limits, disk full | Pod CrashLoop | Not Ready | K8s events | Raise limits; PVC expansion; forensic snapshot |
| F-06 | Bad seed / Vault outage | Secret missing, injector down | Init fails | Pod pending | Pod status | Fix Vault / ESO / local Secret |
| F-07 | PVC loss | AZ failure, bad SC | Data loss risk | Full resync from archives | Volume events | RetentionPolicy; DR config |

Runbooks:

1. **Partition** — [`tests/chaos/run-partition-verify.sh`](../tests/chaos/run-partition-verify.sh)
2. **Forensic** — [`docs/forensic-snapshot.md`](forensic-snapshot.md)
3. **Vault** — [`docs/vault-stellar-tutorial.md`](vault-stellar-tutorial.md)
