# 2026-09-05 — Cursor 全员 1st 见底时切到全空号，放过仍有 API 余量的号

## 症状

```text
cursor
  ! auto: swapped to kimberly…@hotmail.com
  *  4 kimberly…  1st [  0% left …]  API [  0% left …]
     7 kochis…    1st [  0% left …]  API [ 10% left …]
```

应切到 **7**（有 API 余量），不该切全空的 kimberly。

**排障提醒**：列表若暂无 Credits 列，先按「旁边号是否还有 API / 1st 余量」判换号对错，勿先归因 Credits 显示（用户 2026-09-05 纠正）。

## 根因

旧策略把 Cursor `API` **排除**出自动换号，只看 `1st`（及后来的 Credits）。全员 `1st` 耗尽时退化成「按重置时间挑最早恢复」——kimberly 重置更近，即使 `1st`/`API` 都是 0% 也会被选中。

## 正确口径（【裁定 · 2026-09-05】）

Cursor `1st`、**Credits**、**API** 是**并行可用池**：

- 任一池仍有余量（`Ok` / `Warn`）→ 该号可用 / 可作候选；
- 仅当参与判定的池**全部**耗尽 → 才切走，且不得优先于仍有任一池余量的号；
- **不要**在「全员 1st 见底、某号 API 仍有余量」时只按重置时间挑全空号。

仍成立：某号 `1st` 还有余量时，**不要**只因 `API` 耗尽就切走（见 [2026-08-21](2026-08-21-cursor-auto-swap-to-zero-over-remaining.md)）。

## 相关

- [AUTO_SWAP_DESIGN.md](../design/AUTO_SWAP_DESIGN.md) §1.1
- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「自动切换」

<!-- 该文档整理/压缩于 2026-09-05 -->
