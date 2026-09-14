# old-main → new main 对比与捞回审计

基准：
- **新 main 源**：`feat/appmsg-url`（0.5.0 线）
- **old-main**：原 `botiverse/main` tip `6424a21`（0.1.11 线）
- 分叉点：约 `f0dcd4e`

## 结论摘要

| 类别 | 判定 |
|------|------|
| 产品功能（attachment/SNS/biz/appmsg/favorites url） | **两边都有**；feat 侧经 `d5492ea` 恢复/重做，无缺文件 |
| 0.5 独有（doctor/key/timeline/watch/media/online SQLCipher/shard-meta/FTS） | **仅新 main**，不必从 old-main 回捞 |
| 工程硬化（SUDO_USER home、PidFile JSON、stop_daemon、short-page read_exact、new_messages state） | **新 main 已具备** |
| Windows 页保护扫描加宽（#54） | **old-main 更强** → **已捞回** |

## old-main 独有 commit 逐条

| Commit | 主题 | 是否捞回 |
|--------|------|----------|
| e8939f3 / 2b5d872 / c7e2775 | SNS 命令与 media/DOM | 已在新 main，跳过 |
| d750ef6 | sudo home + stop_daemon + ReloadConfig 预留 | 新 main 已有完整实现（含真实 ReloadConfig），跳过 |
| 35a8f0e / b043135 / 1b00d04 / c284b4a | 群昵称 / 引用 / appmsg url / type49 | 已在新 main，跳过 |
| 9d5a78a | macOS TCC 文档 | 文档向；新 README 已改默认「不重签」，跳过 |
| dab3217 / f0f3d3c | biz-articles / favorites url | 已在新 main，跳过 |
| d4587b1 | contacts private 过滤 / search JoinSet / new_messages state | 新 main query 已含等价逻辑，跳过 |
| 70aa3a4 | daemon lifecycle / PidFile / short read / **Windows page protect** | 前三项已有；**page protect 已捞回** |
| 5c001b1 | 0.1.11 bump | 版本已是 0.5.0，跳过 |
| 14fdfde…ff96f95 / 7feacc6 | attachment 全套 + extract 去掉 payload `ok` | 模块在新 main；extract 无重复 `ok`，跳过 |
| b032b8b / e9f65ba / 6424a21 | WAL 增量 cache | 新 main 有 WAL + online + per-key 锁，跳过 |

## 已实现捞回

1. `scanner::is_writable_readable_page`（WinNT 常量 + 单测）
2. `scanner/windows.rs` 扫描条件改回宽匹配（WRITECOPY / EXECUTE_*WRITE* + modifier strip）

## 明确不捞

- 默认 ad-hoc 重签 / 依赖关 SIP 的文档路径（与 0.5 产品方向冲突）
- 把 0.1.11 版本号或 npm 包元数据倒退
- 机械 cherry-pick 整段 query/cache（会覆盖 online-open 重构）
