# subswap 开源就绪度审视

> 审视日期：2026-08-31。范围是 GitHub 首屏、README、安装与发布、贡献入口和架构可维护性；不是功能路线图。

## 裁定

- 不应为架构重构而重构；核心策略、Provider 抽象与原生客户端适配分层适合继续扩展。近期优先：GitHub 元数据、README、贡献说明和 Release 说明一致表达已具备能力与安全边界。
- 竞品 AISW（<https://github.com/burakdede/aisw>）：可借鉴「真实情境 + 最短上手 + 终端演示」首屏组织；不照搬跨工具 profile/context。subswap 差异化在按各客户端真实凭证形态、额度窗口和并发安全边界分别处理；强行统一成 profile 会掩盖边界并提高误切换风险。
- 用户已授权直接完成本审视中的对外优化并推送 GitHub（2026-08-31）；不引入跨工具 context、插件系统或改变账号切换策略。

## 已确认的优势（对外应表达）

- 五 Provider 同一核心策略与注册表；文件型 OAuth 复用共享引擎；Claude / Cursor 因钥匙串·API / SQLite·桌面生命周期保留专用实现。
- AutoSwap 无 IO 纯决策：手动优先、未知额度降级、刚切换宽限、最早恢复、`manual_only` 等边界有测试。
- 三平台 CI、原生 Release、Homebrew、Windows 一键安装（SHA-256）已存在。

## P0 / P1（2026-08-31 已关闭）

| 项 | 状态 |
|---|---|
| GitHub 简介/topics 含五客户端 | 已关闭 |
| `CONTRIBUTING.md` 对齐五 Provider + owner-only 文件凭证库 | 已关闭 |
| Release notes 用户可读（禁止仅 `Automated release for vX.Y.Z.`） | 已关闭（流程门槛） |
| 支持矩阵拆分：导入/切换、额度、自动切换、隔离运行、daemon、各 OS 安装包 | 已关闭 |
| 凭证保护措辞：macOS/Linux 强制 `0600`；Windows 靠用户应用数据目录账户权限 | 已关闭 |
| 公开承诺平台构建全成功才可发布（含 macOS 双目标、Linux ARM） | 已关闭 |
| Windows 安装验证覆盖当前 draft tag（保留历史版回归） | 已关闭 |
| README 首屏价值主张、脱敏终端演示、Quick start 分流、三项行为边界 | 已关闭 |
| `SECURITY.md`、问题/PR 模板、私密漏洞报告、Dependabot | 已关闭 |
| 英文 README 事实源 + 三语机械同步；贡献地图 | 已关闭 |
| 适用边界文案（本人账号；不用于共享凭证/绕过用量/规避条款） | 已关闭；OpenAI ToU：<https://openai.com/policies/terms-of-use/> |

## P2：有真实需求再投入（待处理）

- 增设 Linux/macOS 的独立安装脚本和 shell completion；当前 Homebrew、Cargo 与 Windows 已覆盖核心安装面，不应抢在 P0/P1 前投入。
- 建独立文档站；README 优化和公开的贡献/安全入口先行即可。
- 做跨工具「工作/个人/客户」组合切换。只有出现明确用户需求时再设计，且必须先证明不会绕过各 Provider 的刷新、桌面客户端和隔离运行边界。
- 立即上代码签名、供应链证明或全面依赖治理。现阶段先保证所有安装包都有校验和、Release 不缺资产、安装验收覆盖当前版本；有稳定下载量或外部贡献后，再投入 provenance、依赖更新自动化与 Actions SHA 固定。

## 仍成立的对外与架构约束

**定位**：本地优先、尊重各客户端原生状态与额度边界的多账号切换；首屏叙事「场景 → 一两步能做什么 → 限制 → 深入」；不照搬 AISW/aiwitch 的 profile 模型。价值主张参考：`A local-first multi-account switcher for AI coding tools that respects each client's native state and quota boundaries.` 避免 `unified multi-provider subscription swapper`；保留可搜索词 `account switcher` / `quota-aware` / 各客户端名。

**安装可信度**：Windows 脚本校验 SHA-256；README 同时给 Release 下载页；Homebrew 描述随 Provider 核对；`cargo install --git` 标为开发者/尝鲜路径；用户可读 Release notes 是发布门槛（GitHub 自动生成：<https://docs.github.com/en/repositories/releasing-projects-on-github/automatically-generated-release-notes>）。

**社区**：问题模板索取 `subswap doctor` 脱敏输出、OS、原生客户端版本、复现步骤、是否启用后台；禁止贴 token/刷新凭证/完整登录文件/真实邮箱/账单截图。私密漏洞报告与 `SECURITY.md` 互补（<https://docs.github.com/en/code-security/how-tos/report-and-fix-vulnerabilities/configure-vulnerability-reporting/configure-for-a-repository>）。

**架构**：核心层只做账号/额度/候选/回滚；文件·钥匙串·SQLite·进程生命周期留在适配层。新增 Provider 先判「单一凭证文件」vs「需客户端生命周期协调」。AutoSwap 保持无副作用可测决策单元。暂不引入插件系统、动态 Provider 发现或跨工具 context。

## 每次公开发布前的机械核对

1. README 英文源、三种翻译、GitHub 简介、topics、Homebrew 描述是否都列出同一支持范围。
2. 支持矩阵是否反映切换、额度、自动切换、隔离运行、daemon 和安装包的实际差异。
3. 本次 tag 的每一个公开承诺平台是否真的有可下载资产、校验和，并按安装方式跑过版本检查。
4. Release notes 是否包含用户可见的新能力、修复、限制和升级动作；没有实际变化时也写明「维护发布，无用户行为变化」。
5. 是否新增了需要同步进入贡献说明、问题模板或安全政策的敏感数据/原生客户端边界。

## README 维护约定

新增或移除 Provider、平台能力、安装渠道或用户可见限制时，同一次变更同步核对：README 四种语言、GitHub 仓库简介、topics、支持矩阵、Quick start、FAQ、`CONTRIBUTING.md` 和 Release notes。README 是对外权威入口；内部 `docs/` 可保留实现细节，但不能替代对外边界说明。

## 实施结果（2026-08-31）

P0/P1 已落实并推送；发布流程改为用户可读 notes + 全平台资产 + Windows 当前 tag 安装验收门槛；Homebrew 描述已含 OpenCode。本机 1.6.2 覆盖安装 / 版本 / daemon 冒烟与全 workspace 检查、测试、release 构建通过。`run kimi` 隔离测试改为不依赖本机是否安装原生 Kimi CLI。主分支 Dependabot 告警 `quinn-proto` GHSA-4w2j-m93h-cj5j：已升至 `0.11.15`。

审视时证据摘要：2 stars / 1 fork / 0 issues；社区健康度 57%（模板与准则当时缺失，现已补）；v1.6.1 Release 资产齐全但正文当时无用户可读说明。

<!-- 该文档整理/压缩于 2026-09-05 -->
