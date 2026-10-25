# TASKBOARD — 多 Agent 并行协作看板

> 规则见 agentsync 全局 docs/AGENT_TASKBOARD_GUIDE.md。只编辑自己的条目；完成后删除。

| 任务 | 状态 | 影响范围 | 开始 | 最近更新 | 备注 |
|---|---|---|---|---|---|
| Bound background quota collection and Apple snapshot traffic | 发布中 | Core cache/coordinator; adaptive scheduler; provider request telemetry; read-only status and isolated regressions; docs/release | 10-24 20:18 | 2026-10-25 12:55 | v1.16.1/v1.16.2 CI 暴露 Linux 锁回收、Windows 时序与 rustfmt 格式问题；v1.16.3 发布后审计又发现空成功读数按 90 秒地板重采；已修复并验证 469 测试，准备 v1.16.4 提交/安装/Release |
| Recover affected Claude login after native-query failure | 恢复未完成 | Native credential preservation; Claude troubleshooting record | 10:06 | 2026-10-09 22:43 | Original credentials preserved; restoration failed real model request; daemon stopped; awaiting evidence of another authenticated device |
| Fix Linux glibc floor + rm never refuses | 验证中 | release.yml Linux zigbuild/glibc guard; rm.rs; Claude disconnect API teardown; cli_surface rm test; CLI/README/OPERATIONS/troubleshooting | 11:13 | 2026-10-09 11:23 | Product: rm never blocked; soft official disconnect; 11:23 cli_surface deepseek rm ok |
