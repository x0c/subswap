# subswap 开源就绪度审视

> 审视日期：2026-08-31。范围是 GitHub 首屏、README、安装与发布、贡献入口和架构可维护性；不是功能路线图。

## 执行裁定（2026-08-31）

用户已授权直接完成本审视中的对外优化并推送 GitHub。实现范围固定为：统一仓库元数据与公开文案；重组四种语言 README 的首屏、安装路径和支持矩阵；修正贡献指南；新增安全、问题与 PR 入口；让发布说明、平台资产与 Windows 安装验收反映当前版本。不会在本轮引入跨工具 context、插件系统或改变账号切换策略。

## 裁定

当前不应为架构重构而重构。核心策略、Provider 抽象和各原生客户端适配的分层已经适合继续扩展；近期最能提高外部信任和关注转化的工作，是让 GitHub 元数据、README、贡献说明和 Release 说明一致地表达已具备的能力与安全边界。

直接竞品 AISW 的调研结论：可借鉴“真实使用情境 + 最短可执行上手路径 + 终端演示”的首屏组织；不照搬其跨工具 profile/context 模型。subswap 的差异化价值在于按各客户端真实凭证形态、额度窗口和并发安全边界分别处理，强行统一成 profile 会掩盖这些边界并提高误切换风险。

参考：<https://github.com/burakdede/aisw>（调研时浅克隆至 `/tmp/ref-aisw-jDUN92/aisw` 并阅读 README 与项目结构）。

## 已确认的优势

- 五个 Provider 通过同一核心策略和注册表协调；文件型 OAuth 客户端复用共享引擎，Claude 与 Cursor 因钥匙串/API 和 SQLite/桌面生命周期差异保留专用实现。
- 自动切换策略是无 IO 的纯决策单元，覆盖手动优先、未知额度降级、刚切换宽限、最早恢复和 `manual_only` 等边界，已有针对这些边界的测试。
- 三平台 CI、原生 Release 资产、Homebrew 与 Windows 一键安装链路已存在；Windows 的安装脚本会校验 SHA-256。

这些是应在首屏对外表达的可信度资产，不是需要隐藏在内部文档里的实现细节。

## 优先级清单

### P0：先消除不一致，才能建立信任

1. **同步 GitHub 仓库简介和 topics。** 当前公开简介只提 Claude、Codex、ChatGPT；topics 也缺 Kimi、Cursor、OpenCode、Moonshot。README 已宣称支持五类客户端，二者必须同步。建议简介：

   > Safely switch and monitor multiple Claude Code, Codex, Cursor, Kimi and OpenCode accounts—with local snapshots and optional quota-aware auto-swap.

2. **重写 `CONTRIBUTING.md` 的过期事实。** 它仍称凭证只放系统钥匙串、Provider 只有 Claude/Codex，和默认使用 owner-only 文件凭证库、现有五个 Provider 的事实冲突。贡献指南中的注册位置、验证命令和安全规则必须以当前 `AGENTS.md` 与领域文档为准。

3. **让每个 Release 有人能读懂的变更说明。** 当前 v1.6.1 的正文仅为 `Automated release for v1.6.1.`。发布流程应生成或要求填写用户可感知的新增、修复、已知限制和升级提示；自动化可以继续创建草稿，但不应把这句作为正式说明发布。

4. **把“全平台支持”拆成可判断的支持矩阵。** 现有表把五个客户端都列为支持，但隔离运行不支持 Cursor、后台 daemon 不支持 Windows、macOS 的 daemon 又要显式开启。对用户而言这是三种不同能力，不能只用一个“支持”概括。矩阵至少列出：导入/切换、额度查询、自动切换、隔离运行、后台 daemon、各操作系统安装包；缺一项就明确写“不可用”和原因。

5. **修正凭证保护的跨平台措辞。** 代码只在 Unix 断言并设定 `0600`；README 的“owner-only (`0600`) file”却放在三平台共同承诺中。对外文案应准确为“macOS/Linux 强制 `0600`；Windows 使用用户应用数据目录，具体访问控制由系统账户权限决定”，不要把 Unix 权限位表述成 Windows 保证。

6. **发布必须满足公开承诺的全部安装目标。** Release 工作流把 macOS 两个目标和 Linux ARM 标为允许失败，但发布条件只要求整体构建成功；因此将来可能出现已发布版本少了某个 README 承诺平台的资产。两种做法二选一：要么将这些目标转正并阻止不完整发布，要么在 README 和 Release 标为 preview，不能继续写成完整支持。

7. **让 Windows 安装验证覆盖当前候选版本。** CI 现在固定下载安装历史 v1.3.0，所以只能证明旧安装器仍可用，不能证明刚打出的 Release 能下载、校验和启动。保留这条回归检查，同时在 Release 资产上传后以当前 tag 跑一次安装和版本验证。

### P1：降低首次试用与贡献摩擦

1. README 开头改为产品名 + 一句价值主张，不把六个品牌堆进主标题；把“本地安全切换、额度可见、可选自动切换”放在一句话里。
2. 在首屏放一张真实终端状态截图或短 GIF：展示账号列表、额度、手动切换和自动切换结果。演示应使用虚构账号，不露出 token、邮箱或真实用量。
3. 把 Quick start 改成两条清晰路径：已经在原生客户端登录的用户如何导入；需要新增账号的用户如何登录/导入。第一屏只放最常用步骤，其余命令下沉到 CLI 文档。
4. 在 Quick start 前明确三项行为边界：Cursor 切换会协调桌面应用重启；账号隔离不支持 Cursor；macOS 后台自动切换需要显式开启。避免用户在安装后才发现影响。
5. 增加 `SECURITY.md`（私密漏洞报告方式、凭证文件权限与不应提交的信息）以及问题模板（诊断信息、脱敏要求、平台与客户端版本）。是否采用完整行为准则可在有稳定贡献流量后再决定。

6. 将英文 README 定为唯一事实源，并为三种翻译加机械同步检查：标题层级、支持矩阵、命令块、FAQ 问题、Provider 名单必须一致。当前日文/韩文在架构、常见用途、隔离运行和比较段落仍遗漏 OpenCode；人工翻译不是问题，缺少“改能力时一并核对”的机制才是问题。

7. 为外部贡献者补一页短的英文贡献地图。保留内部中文知识库，但不要要求陌生贡献者先读大量中文机制文档才能判断从哪里开始；这页只需解释可改范围、测试方式、敏感数据禁令和如何报告兼容性问题。

8. README 增加“适用边界”：仅管理你本人拥有或获授权使用的本地账号；不用于共享凭证、绕过上游用量限制或规避服务条款。不要把它写成法律结论，而是明确产品不承诺这些用途。OpenAI 当前条款明确禁止共享账号凭证或让他人使用账号，也要求遵守其使用政策；这是把安全边界讲清而非指控任何用户违规的必要原因。<https://openai.com/policies/terms-of-use/>

### P2：有真实需求再投入

- 增设 Linux/macOS 的独立安装脚本和 shell completion；当前 Homebrew、Cargo 与 Windows 已覆盖核心安装面，不应抢在 P0/P1 前投入。
- 建独立文档站；README 优化和公开的贡献/安全入口先行即可。
- 做跨工具“工作/个人/客户”组合切换。只有出现明确用户需求时再设计，且必须先证明不会绕过各 Provider 的刷新、桌面客户端和隔离运行边界。
- 立即上代码签名、供应链证明或全面依赖治理。现阶段先保证所有安装包都有校验和、Release 不缺资产、安装验收覆盖当前版本；有稳定下载量或外部贡献后，再投入 provenance、依赖更新自动化与 Actions SHA 固定。

## 更完整的产品与沟通审视

### 产品定位：强调“安全协调”，不要只说“多账号”

“账号切换器”已是拥挤词：同类项目分别主打 profile、每项目上下文、桌面面板、用量仪表盘或环境变量隔离。subswap 不应与它们比“支持多少账号”，而应明确下列主张：

> A local-first multi-account switcher for AI coding tools that respects each client's native state and quota boundaries.

中文可译为“尊重各客户端原生登录状态与额度边界的本地多账号切换工具”。首屏避免使用 `unified multi-provider subscription swapper` 这类不自然、也不贴合用户搜索习惯的说法；保留 `account switcher`、`quota-aware`、`Claude Code`、`Codex`、`Cursor` 等用户真实会搜索的词。

对照项目表明，外部用户最容易理解的叙事顺序是“我遇到的场景 → 我能在一两步内做到什么 → 具体限制 → 深入能力”。AISW 把工作/个人/客户场景、Quick start 和终端演示放在前面；aiwitch 用“每个 profile 独立环境”解释心智模型；一些 Codex 专用工具则用截图建立直观感受。subswap 应借鉴这个组织顺序，不能照搬其功能模型。

### README 的建议结构

1. 品牌名、单句价值主张、CI/Release/许可证 badge。
2. 一张脱敏终端截图或 10–15 秒 GIF；截图必须证明“查看 → 手动切换 → 自动切换结果”中的至少两步。
3. 三个真实场景：工作与个人账号分离、额度耗尽时明确切换、并行终端不干扰全局账号。
4. 按用户当前状态分流的 Quick start：已登录导入 / 新账号登录；说明第一次运行的后台副作用。
5. 支持矩阵与安全/兼容边界。
6. 深入功能、FAQ、安装替代路径、贡献与安全入口。

不要在首页保留“已完成里程碑”表。它适合项目内管理，却不帮助新用户决定是否试用；换成简短的“当前稳定能力”或移入变更记录即可。

### 安装、发布与更新可信度

- Windows 脚本下载当前 Release 并校验 SHA-256，是正确基础；README 应同时给出 Release 下载页，避免把管道执行当成唯一可见安装路径。
- Homebrew formula 的自动生成描述仍漏掉 OpenCode，属于又一处对外信息源不同步；将它纳入“新增 Provider 的发布核对项”。
- `cargo install --git` 安装的是仓库指定分支的源码，不等同于已验证的 Release。README 应把它明确标为开发者/尝鲜路径；普通用户优先 Homebrew、Windows 安装器或已校验的 Release 附件。
- 版本发布十分密集，而近期开源 Release 大多使用同一句自动说明。将用户可读 Release notes 视为发布门槛，而不是宣传加分项；GitHub 原生支持生成并分类 Release notes。<https://docs.github.com/en/repositories/releasing-projects-on-github/automatically-generated-release-notes>

### 质量、安全与社区入口

- 现有 CI 已有格式、静态检查和 macOS/Linux/Windows 测试，这是可信度优势，应在 README badge 后用一句“已在三平台 CI 验证”说明，但不要扩大为所有功能同等可用。
- 仓库社区健康度为 57%，当前缺少问题模板、PR 模板和行为准则。先补问题模板与 `SECURITY.md`；行为准则可随社区增长补上，避免为了分数建立无人维护的流程。
- 公开仓库应在 GitHub 设置中启用私密漏洞报告，并在 `SECURITY.md` 中说明响应目标和不该公开的内容。GitHub 的私密报告入口与 `SECURITY.md` 是互补关系，不是一者替代另一者。<https://docs.github.com/en/code-security/how-tos/report-and-fix-vulnerabilities/configure-vulnerability-reporting/configure-for-a-repository>
- 问题模板应索取 `subswap doctor` 的脱敏输出、操作系统、原生客户端版本、复现步骤与是否启用后台模式；明确禁止贴 token、刷新凭证、完整登录文件、真实邮箱和账单截图。

### 架构与长期维护：保持边界，不扩张抽象

当前的共享切换引擎 + 专用 Provider 组合是正确的中期形态。建议保留以下规则：

- 核心层只做账号、额度、候选选择与回滚等不依赖具体客户端的规则；各客户端的文件、钥匙串、SQLite、进程生命周期和官方刷新协调必须留在自己的适配层。
- 新增 Provider 前先判定它是“单一凭证文件”还是“需要客户端生命周期协调”；前者接共享引擎，后者独立实现。不要为了统一目录结构牺牲安全边界。
- 保持自动切换策略为无副作用、可用测试矩阵描述的决策单元；当前覆盖的未知额度、刚手动切换、已耗尽、最早恢复和 `manual_only` 是未来改动最容易回归的部分。
- 暂不引入插件系统、动态 Provider 发现或跨工具 context。五个内置客户端时，显式注册更直观、更容易审查，也更不易让凭证访问范围失控。

## 每次公开发布前的机械核对

当 Provider、平台、安装渠道或安全边界变化时，同一变更必须逐项核对：

1. README 英文源、三种翻译、GitHub 简介、topics、Homebrew 描述是否都列出同一支持范围。
2. 支持矩阵是否反映切换、额度、自动切换、隔离运行、daemon 和安装包的实际差异。
3. 本次 tag 的每一个公开承诺平台是否真的有可下载资产、校验和，并按安装方式跑过版本检查。
4. Release notes 是否包含用户可见的新能力、修复、限制和升级动作；没有实际变化时也写明“维护发布，无用户行为变化”。
5. 是否新增了需要同步进入贡献说明、问题模板或安全政策的敏感数据/原生客户端边界。

## README 维护约定

新增或移除 Provider、平台能力、安装渠道或用户可见限制时，同一次变更同步核对以下位置：README 四种语言、GitHub 仓库简介、topics、支持矩阵、Quick start、FAQ、`CONTRIBUTING.md` 和 Release notes。README 是对外权威入口；内部 `docs/` 可保留实现细节，但不能替代对外边界说明。

## 本次证据

- GitHub 公开页与 API（2026-08-31）：2 stars、1 fork、0 issues；仓库健康度 57%，缺 issue/PR 模板和行为准则，且简介与 topics 未含新增客户端。
- 最新公开 Release v1.6.1（2026-08-16）包含 macOS、Linux、Windows 资产，但正文没有用户可读变更说明。
- 代码图谱索引（2555 nodes / 10262 edges，无解析缺口）及关键链路审视确认 Provider 组成、自动决策与专用适配边界如上；该结论只支持“现阶段无需架构重构”，不替代每次行为修改后的实跑验证。

## 实施结果（2026-08-31）

本次已落实 P0/P1 中不需要产品方向拍板的事项：GitHub 简介和 topics 已补齐五个支持客户端；英文与中文 README 重写为面向首次使用者的入口，日文与韩文同步修正 OpenCode、平台能力与安装边界；补充贡献、安全、问题和合并请求入口；启用私密漏洞报告与 Dependabot 安全更新。

发布流程已改为自动生成用户可读 Release notes，并把全部三平台资产和 Windows 安装器的当前版本验收设为公开发布前置条件。Homebrew 描述也已同步 OpenCode。

本机以真实默认入口完成 1.6.2 覆盖安装、版本核验和 daemon 启动；完整 workspace 格式、静态检查、全部自动测试、开发构建和 release 构建均已通过。`run kimi` 隔离测试还修正为不受开发机是否安装原生 Kimi CLI 影响，避免其意外进入交互流程而卡住测试。
