# TASKBOARD — 多 Agent 并行协作看板

> 规则见 agentsync 全局 docs/AGENT_TASKBOARD_GUIDE.md。只编辑自己的条目；完成后删除。

| 任务 | 状态 | 影响范围 | 开始 | 最近更新 | 备注 |
|---|---|---|---|---|---|
| Recover affected Claude login after native-query failure | 恢复未完成 | Native credential preservation; Claude troubleshooting record | 10:06 | 2026-10-09 22:43 | Original credentials preserved; restoration failed real model request; daemon stopped; awaiting evidence of another authenticated device |
| Fix Linux glibc floor + rm never refuses | 验证中 | release.yml Linux zigbuild/glibc guard; rm.rs; Claude disconnect API teardown; cli_surface rm test; CLI/README/OPERATIONS/troubleshooting | 11:13 | 2026-10-09 11:23 | Product: rm never blocked; soft official disconnect; 11:23 cli_surface deepseek rm ok |
| Add standalone Volcengine Ark Coding Plan provider | 验证中 | New volcengine provider/registration/CLI/docs/tests; local install; isolated Claude smoke | 16:26 | 2026-11-07 17:10 | Independent volcengine provider wired (login/swap/rm/run/env); Claude guard prevents dual-active; workspace tests pass; release installed and isolated smoke ok |
| Add interactive Claude/Codex custom API channels | 验证中 | add-api CLI wizard; Codex config/auth helper; common/core locked activation; CLI/provider tests; docs; local install | 11:46 | 2026-11-12 14:21 | Real Codex global + isolated Responses smoke passed; final workspace gates/release install; Pi worker owns separate integration changes |
