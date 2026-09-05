# 2026-09-05 — Cursor 全员 1st 见底时切到全空号，放过仍有 API 余量的号

## 症状

默认入口类似：

```text
cursor
  ! auto: swapped to kimberly…@hotmail.com
  *  4 kimberly…  1st [  0% left …]  API [  0% left …]
     5 terry…     1st [  0% left …]  API [  0% left …]
     6 hillard…   1st [  0% left …]  API [  0% left …]
     7 kochis…    1st [  0% left …]  API [ 10% left …]
```

用户期望：应切到 **7 号**（还有 API 余量），不该切到全空的 kimberly。

## 根因

旧策略把 Cursor 的 `API` **排除**出自动换号判定，只看 `1st`（及后来的 Credits）。
全员 `1st` 都耗尽时，退化成「按重置时间挑最早恢复的号」——kimberly 重置更近，即使
`1st`/`API` 都是 0% 也会被选中。

## 正确口径（【裁定 · 2026-09-05】）

Cursor 的 `1st`、**Credits**、**API** 是**并行可用池**：

- 任一池仍有余量（`Ok` / `Warn`）→ 该号可用，不必切走 / 可作为候选；
- 仅当参与判定的池**全部**耗尽 → 才触发切走，且不得优先于仍有任一池余量的号；
- **不要**在「全员 1st 见底、某号 API 仍有余量」时，只按重置时间挑全空号。

仍成立的旧约束：某号 `1st` 还有余量时，**不要**只因 `API` 耗尽就切走它
（见 [2026-08-21](2026-08-21-cursor-auto-swap-to-zero-over-remaining.md)）。

## 相关

- [AUTO_SWAP_DESIGN.md](../design/AUTO_SWAP_DESIGN.md) §1.1
- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「自动切换」
