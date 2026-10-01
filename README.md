# promotions

This branch holds `stable.json`: the exact set of releases the **stable** yum
channel serves (https://vagrantin.github.io/xoa-proxy/8.3/x86_64/). The Pages
workflow reads it at the start of every run and verifies each asset's SHA-256.

Only the Jenkins promotion job (`prod/promote`, `prod/demote`) writes here,
after QA and a human approval. Entries with `"basis": "legacy-baseline"` were
already served when the gate was introduced (2026-10-01) and were not approved
by it. Design: jenkins-infra `docs/promotion-gate.md`, Vagrantin/xcp-hl#154.
