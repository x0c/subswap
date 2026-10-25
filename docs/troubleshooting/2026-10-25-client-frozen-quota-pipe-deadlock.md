# Subswap Apple 客户端额度停留在旧数据（`status --json` 管道死锁）

## 症状

- `subswap`（默认入口）与 `subswap status --json` 在终端里数据正确、`generated_at` 与 `cached_at` 都是当前时间。
- 已安装的 Mac 客户端、它的菜单栏面板、小组件和 iOS 端全部停在同一天的旧额度上，重启客户端也不刷新。
- 系统里能查到一条长期不退出的 `subswap status --json` 子进程（`ppid` 是 `Subswap`），CPU 接近 0。
- 客户端自身没有报错：首轮刷新就卡住，`isRefreshing` 永远为 true，后续 30 秒轮询不再发起。

## 根因

客户端旧的 `SubswapCLI.run` 把子进程 stdout 接到管道，先 `waitUntilExit()`，退出后才 `readDataToEndOfFile()`。

macOS 管道容量 64 KiB。v1.16.x 的 `status --json` 输出已超过 160 KiB（其中诊断用的 `request_metrics` 占约 87 KiB），于是：

1. 子进程写满管道后阻塞在 `write()`；
2. 父进程等子进程退出，始终不读管道；
3. 双方永久互等，刷新任务永不返回。

终端里不触发，是因为 shell 边写边读，0.06 秒即完成。输出体积随 60 分钟桶与账号维度增长，因此这个缺陷在输出超过 64 KiB 后才必现。

## 确认证据

```bash
pgrep -af 'subswap status --json'      # 找到长期存活的子进程
sample <pid> 1                         # 主线程停在 write()
lsof -p <pid> | grep PIPE              # stdin/stdout PIPE SIZE=65536（已满）
```

同一 spawn 模式的对照实验：输出很小的 `--version` 正常退出；`status --json` 先等待再读必然挂起；并发排空 stdout 则正常退出。

## 修复

- 客户端（Subswap Apple 0.1.6）：子进程 stdout/stderr 重定向到临时文件，退出后读取；增加 20 秒硬超时与终止；优先请求 `status --json --no-metrics`，旧版 CLI 不认识该开关时回退到完整输出。
- CLI（v1.16.5）：新增 `status --json --no-metrics`，只省略 `request_metrics`，其余字段与只读语义不变，供秒级轮询的 GUI / 小组件使用。

任何把 CLI 输出接入管道的消费者都应并发读取或重定向到文件，不要「先等退出再读」。

## 相关

- [只读状态快照](../CLI.md)
- [TROUBLESHOOTING_INDEX](TROUBLESHOOTING_INDEX.md)
