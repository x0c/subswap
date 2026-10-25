# TASKBOARD — 多 Agent 并行协作看板

> 规则见 agentsync 全局 docs/AGENT_TASKBOARD_GUIDE.md。只编辑自己的条目；完成后删除。

| 任务 | 状态 | 影响范围 | 开始 | 最近更新 | 备注 |
|---|---|---|---|---|---|
| Recover affected Claude login after native-query failure | 恢复未完成 | Native credential preservation; Claude troubleshooting record | 10:06 | 2026-10-09 22:43 | Original credentials preserved; restoration failed real model request; daemon stopped; awaiting evidence of another authenticated device |
| Fix Linux glibc floor + rm never refuses | 验证中 | release.yml Linux zigbuild/glibc guard; rm.rs; Claude disconnect API teardown; cli_surface rm test; CLI/README/OPERATIONS/troubleshooting | 11:13 | 2026-10-09 11:23 | Product: rm never blocked; soft official disconnect; 11:23 cli_surface deepseek rm ok |
| Generalize quota window ordering to real durations | 进行中 | core/model.rs QuotaWindow::Timed + serde tokens; cli/render.rs ordering/labels; codex/kimi window mapping; quota_cache TTL; CLI/PROVIDER docs | 20:55 | 2026-10-25 20:55 | Owner: unknown lengths (e.g. 20d) must sort by duration, not name lists |
