# subswap 图标设计

> 现行图标的决策、来源与约束。图标探索方法论与历次评审结论在 `apple-app-icon-pack` 技能的 playbook（Subswap D–L 轮）。

## 现行图标（2026-11-02 机主采纳）

| 用途 | 文件 |
|---|---|
| 方形满幅母版（1024×1024，不透明 RGB） | `design/icon/subswap-icon-1024.png` |
| README 展示图（256 px，已裁圆角、透明角） | `docs/images/app-icon.png` |

**画面**：侧身的扁平渐变变色龙，尾巴卷成螺旋，整体轮廓是字母 S；身体从头部青绿 `#19B3A6` 渐变到尾尖天蓝 `#3C8CF0`；纯白底，主体约占画布 62%，四周等距留白。

**为什么是它**：变色龙最出名的行为是换颜色，对应 subswap 的核心任务——换身份、换账号；尾巴的 S 把名字首字母藏进轮廓；它是有神态的角色，不是冷冰冰的公司标志。机主评价：「这次的变色龙的设计上非常聪明 有隐喻在里面」，并要求不出变体、原样采用。

## 约束

- 原样使用模型出图：不重新居中、不压平成矢量、不改色、不换底。任何修改都要机主明确指示，且一次只改一处。
- README 只引用圆角展示图，不直接贴方形母版；四种语言 README 同步放在标题上方。
- 不要再用这些已被否定的方向：谐音梗（潜艇、地铁、三明治，sub 在这里指订阅）、只沾边的概念（沙漏、电池、SIM 卡、小人）、会被认成别的应用的物件（钥匙→密码管理器、工牌头像→通讯录、叠卡→复制）、哑光玩具场景、带“换”的装饰字母 S。

## 已知风险

- **对比度门禁未通过**（`validate_icon_contrast.py`，48 px 与 24 px）：头部青绿对白底仅 1.95:1，身体与尾巴约 3.2:1。目视检查 180/48/24 px 在浅色与深色背景上均可辨认。目前只用于 README，不阻断；将来若打包成 macOS/iOS 应用图标，需先由机主决定是否加深头部青绿。
- 白底图标放在 GitHub 浅色主题页面上时，圆角外形主要靠主体识别，边界不明显。

## 来源与复现

- 出图：`gpt-image`（gpt-image-2，后端回报质量 low），L 轮第 4 格；探索稿保存在本地 `design/icon/explorations/`（体积大，不入库）。
- 完整提示词：

```text
Use case: logo-brand. Asset: one square app icon artwork, full-bleed opaque 1:1 canvas, the ground colour runs edge to edge; no rounded corners, no border, no frame, no text, no letters except where stated.
Subject: a stylised chameleon emblem seen from the side, standing on nothing, its tail curling into a clear spiral below the body so the whole silhouette suggests a letter S; one round eye; the body colour flows smoothly from turquoise #19B3A6 at the head to sky blue #3C8CF0 at the tail tip.
Ground: pure white #FFFFFF.
Composition: bold centred mark about 62 percent of the canvas, even breathing room.
Charming detail: the colour shift along the body, as if it is changing colour right now.
Rendering: a modern app-icon mark, one emblem with one continuous calm silhouette, rendered as flat with a soft tonal gradient, simple geometric shapes; one soft light from the upper left; not a photograph, not a 3D product render. The eye should read the whole picture in one glance: only the parts named here exist, nothing else is added.
Remove: scales, spikes, crest, branch, leaves, legs beyond two simple ones, patterns, outlines; only turquoise to sky blue, a dark eye pupil and white. One clear focal subject that reads instantly at 48 pixels.
```
