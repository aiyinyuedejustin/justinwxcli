# justinwxcli

个人维护的微信本地数据工具，Rust 可执行文件名为 `wx`。本仓库是代码、解密、查询、导出和维护说明的唯一来源。上游起点为 [jackwener/wx-cli-again](https://github.com/jackwener/wx-cli-again)，个人远端为 [aiyinyuedejustin/justinwxcli](https://github.com/aiyinyuedejustin/justinwxcli)。

在本目录与 agent 对话，说明日期、会话范围和想找的信息即可。agent 根据任务组合查询，直接回答或按要求保存结果。工具本身只读取本地微信数据，模型和联网操作由当前对话决定。

## 安装与本机配置

日常入口是 `./bin/wx`。构建需要 Rust 和系统 C 编译器；运行已构建的 `wx` 使用内置 SQLCipher/OpenSSL/Zstd，无 Python 虚拟环境、Node.js 或模型密钥依赖。

```sh
cargo build --release --locked
mkdir -p bin
cp target/release/wx bin/wx.new
mv -f bin/wx.new bin/wx
./bin/wx --version
```

本机状态统一放在 `~/.wx-cli/`：

- `config.json`：微信 `db_storage` 路径、密钥文件位置。
- `all_keys.json`：按数据库保存的 AES 密钥，权限 `600`，目录权限 `700`。
- `cache/`：部分旧查询命令生成的可重建解密缓存。
- `daemon.log`：后台查询进程日志；启动时超过 5 MiB 会轮转一份。

`config.example.json` 是配置格式示例。配置查找顺序为当前目录、可执行文件目录、`~/.wx-cli/config.json`；本机统一使用最后一种位置。

macOS 首次提取密钥时，保持微信已登录，在本机 Terminal 执行：

```sh
sudo ./bin/wx init
./bin/wx doctor --json
```

升级微信、新增账号或出现缺失分片时：

```sh
sudo ./bin/wx key extract --hook-seconds 90
./bin/wx daemon stop
./bin/wx doctor --json
```

提取先扫描进程内存中的明文和 Config.Cipher XOR 混淆候选，以实际数据库首页校验密钥。缺密钥时 LLDB hook 等待数据库加载，等待期间打开相关聊天。官方Hardened Runtime微信还受目标调试授权约束；本机4.2.0的重新取钥实测返回 `task_for_pid kr=5`。已有有效密钥可直接使用。现有实现保持微信签名和 SIP 状态；具体结果与处理入口见 [macOS 权限指南](docs/macos-permission-guide.md)。

日常查询使用已有密钥，微信退出后也可读取已落到本机的数据。覆盖范围是当前账号在这台 Mac 上保存的历史；手机端尚未同步的内容通过微信自身同步。

## 日常查询

```sh
./bin/wx sessions -n 200 --json
./bin/wx history '群名或username' --since 2026-11-02 --until 2026-11-07 -n 100 --json
./bin/wx search 'computer use' --since 2026-11-02 --until 2026-11-07 -n 200 --json
./bin/wx search 'huggingface.co' --in '群名或username' --since 2026-11-02 --json
./bin/wx members '群名或username' --json
./bin/wx contacts --query '名字' --json
```

`history`、`search` 的日期使用本机时区，`--until YYYY-MM-DD` 包含该日。`export` 使用下节的显式时区与排他上界。

浏览命令有默认条数限制。`history` 返回 `truncated` 和 `next_offset`，用 `--offset` 接着读取；`--after` / `--before` 是严格秒级时间条件。同一秒多条消息的完整读取使用 `export`，或者固定时间窗按 offset 分页。`search` 的 `truncated=true` 表示仍有命中结果，缩小时间窗或使用完整导出。搜索逐行解压后匹配真实消息正文，长历史优先给时间范围。

`meta.unknown_shards` 列出缺密钥的消息库；读取错误以命令失败报告。`raw_content`保留浏览格式化之前的完整解码正文；`message_ref`包含分片、表、会话和本地消息号。

## 完整结构化导出

`export`直接只读打开原始数据库，默认全量、逐条输出 JSONL。它保留文本、引用、转发、链接、文件元数据以及图片/语音等消息的原始字段。媒体内容仍在微信原位置，按任务另行提取。

```sh
# 指定群或单聊；名称要求精确且唯一，重名时使用 sessions 返回的 username
./bin/wx export '群名或username' --since 2026-11-02 --until 2026-11-08

# 多个指定会话
./bin/wx export '群A' --in '群B' --since 2026-11-02 --until 2026-11-08

# 全部群；按关键词和链接域名筛选
./bin/wx export --all-groups --since 2026-11-02 --until 2026-11-08 --keyword CUA --keyword 'computer use'
./bin/wx export --all-groups --since 2026-11-02 --until 2026-11-08 --domain huggingface.co

# 全部本地会话，包括群、单聊和本地消息表中的公众号记录
./bin/wx export --all-chats --since 2026-11-02 --until 2026-11-08

# 明确需要文件时保存；目标文件须尚未存在
mkdir -p exports
./bin/wx export '群名或username' --since 2026-11-02 --until 2026-11-08 --output exports/messages.jsonl
```

日期默认 `+08:00`。`--since`包含起点，`--until`排除终点；导出 11 月 7 日整天，填写 `--since 2026-11-07 --until 2026-11-08`。可以传 `--timezone-offset +09:00`，或直接使用带偏移的 RFC3339 时间。

多个 `--keyword` 为任一匹配，多个 `--domain` 为任一域名匹配；关键词组与域名组同时指定时取交集。域名包含子域名。主题相关性由 agent 阅读上下文判断，可先按多组候选词取消息，再按会话和时间补上下文。

JSONL 第一行 `record=metadata`，中间每行 `record=message`，最后一行 `record=summary`。成功退出且得到 summary 后，检查 `scope_complete`、`truncated` 和 `unresolved_tables`。stdout消费中途停止或命令报错时，已经接收的行属于部分结果。显式输出文件在普通执行错误时自动删除；强制杀进程产生的残留文件由使用者删除。

每条消息的主要字段：

| 字段 | 含义 |
|---|---|
| `id` | 当前账号＋分片＋消息表＋local_id；重复导出保持一致，账号库重建后应视为新来源 |
| `server_id` | 微信消息号，以字符串保留大整数；引用的 `structured.quote.server_id` 可据此关联 |
| `chat_username` / `chat_name` / `chat_type` | 会话身份、显示名、类型 |
| `sender_username` / `sender_display` | 原始发言者身份与群名片/联系人显示名；无法解析的作者为 null |
| `timestamp` / `time` | Unix 秒与带时区时间 |
| `content` | 完整解压正文，XML保持完整 |
| `structured` | 解码后的URL、XML叶字段、明确引用和转发内嵌内容；解析异常写入 `parse_issue` |
| `row` | 原始SQL全部列；BLOB编码为base64，大整数/非有限浮点使用显式编码 |

默认按分片、消息表、时间、排序号输出，跨群时间排序由下游按 `timestamp` 完成。每个数据库持有只读事务。`--limit N`显式限量并报告截断；`--format json`把结果收集为一个文档，适合较小范围。

需要无损SQL快照和递归补齐时间窗外的明确引用时，用高级入口 `raw-export`，参数见 `./bin/wx raw-export --help`。该入口要求精确username或消息表，以及配置中的数据库/密钥路径；输出 `raw.jsonl` 和 `manifest.json`。`manifest.json`记录条数验证、找不到的引用与解析问题。

## 附件与其他能力

```sh
./bin/wx attachments '群名或username' --json
./bin/wx extract 'attachment_id' --output exports/image.png --json
./bin/wx favorites --json
./bin/wx timeline --since 2026-11-07 -n 100 --json
./bin/wx --help
```

图片解密、语音导出、朋友圈与公众号命令沿用仓库的对应模块。文本与文件卡片的元数据在完整导出中保留；正文读取或媒体识别由下游按需执行。`timeline` 是有限会话窗口的浏览工具，全部会话的完整窗口使用 `export --all-chats`。

## 清理与磁盘

日常自动回收由工具自身完成：

| 产物 | 自动处理方式 |
| --- | --- |
| 解密过程中的 `.db.tmp` | 成功安装缓存或执行报错后立即回收；进程中断的残留在后台下次启动时回收 |
| 取钥用 LLDB 临时目录 | 取钥函数结束时回收，覆盖正常返回和错误返回 |
| `~/.wx-cli/cache/` 解密缓存 | 后台启动时及运行中每分钟检查；7天未使用的缓存删除，总量超过512 MiB时按最近使用时间从旧到新回收 |
| `~/.wx-cli/daemon.log` | 启动时及运行中每分钟检查，超过5 MiB清空后继续记录；旧的 `daemon.log.1` 同时回收 |
| 用户指定的导出文件、附件 | 按用户指定路径保留，使用者决定删除时间 |
| Rust `target/` 编译产物 | 开发时生成；安装并验证 `bin/wx` 后执行 `cargo clean` |

缓存回收会等待正在进行的后台查询结束，查询期间可以暂时超过512 MiB。缓存的最近使用时间随访问保存，后台重启后继续计算。回收范围限定为工具生成的缓存文件名；配置、密钥和微信原始数据保持原位。后台停止期间，下次启动会继续执行维护。

Agent另行编写的分析脚本使用 `tempfile.TemporaryDirectory()`，或使用带退出清理的临时目录，任务结束时回收中间材料。显式要求交付的结果文件按约定路径保留。

```sh
# 查看可重建缓存和日志
./bin/wx clean

# 停止daemon，删除缓存和日志，保留配置与密钥
./bin/wx clean --apply

# 已安装 bin/wx 后，释放本仓库的 Rust 编译产物
cargo clean
```

普通 `export` 标准输出模式在本仓库内零落盘。`--output`生成的文件由用户选择保留时间，用完删除指定文件即可。原始微信数据库和附件归微信管理。`clean`只删除 `~/.wx-cli/` 中明确列出的可重建状态。

## 开发和验证

日常在 `main` 开发。`origin` 指向个人仓库，`upstream`用于只读参考。运行 `cargo check`、相关测试；跨平台改动另外检查 Linux 与 Windows target，交叉构建需对应 C/OpenSSL 工具链。

上游参考：Config.Cipher混淆 [PR #23](https://github.com/huohuoer/wechat-cli/pull/23)、[PR #29](https://github.com/huohuoer/wechat-cli/pull/29)。本项目候选扫描测试覆盖全部32个mask相位及重叠锚点，密钥仍通过本地数据库验证。当前Mac的17份已有数据库密钥使用4096字节页、HMAC-SHA512。

## 0.7.0 本机验收（2026-11-07）

微信 4.2.0（270345），已有17份数据库密钥全部通过首页HMAC-SHA512校验。对北京时间2026-11-02至2026-11-08的本地窗口，独立SQLCipher查询与完整导出对照：34,044条消息、135个非空会话，逐会话条数与最早/最晚时间全部一致；其中群聊31,424条、单聊2,602条、公众号18条。验证过程中微信继续运行，后续重新查询会包含新到消息。

167项单元测试与1项真实CLI合成数据库测试通过。CLI测试覆盖603条消息、WAL更新、压缩文本、同秒消息、单聊双方身份、原始BLOB与大整数、域名筛选和限量截断。macOS cargo check与release构建通过，Linux交叉check使用cargo-zigbuild通过。Windows目标已尝试普通check和cargo-xwin，均停在OpenSSL的MSVC/Perl构建环境，Windows代码检查结果待原生Windows环境验证。

当前官方签名微信重新取钥返回task_for_pid kr=5，进程读取边界见权限指南。已有密钥导出已完成上述实测。XOR候选扫描覆盖全部32个相位的合成测试，当前官方签名进程中的候选扫描等待可读取的运行环境。

同日XML修复回归：固定时间窗的34,182条消息在修复前后，正文、原始SQL字段、作者、会话、消息标识和时间的序列化SHA256完全一致；6条纯文本XML误报与1条长转发的内嵌声明解析错误全部消除，成功展开的合并转发从27条增加到28条。解析副本处理声明，导出的 `content` 与 `row` 保留原值。默认完整导出格式继续保留，阅读层精简待单独决定。

2026-11-08自动回收回归：173项单元测试与1项CLI导出测试通过，覆盖闲置/容量回收、查询期间保护、重启后保留使用时间、错误临时副本回收及日志句柄继续写入。本机release安装后，会话查询、3条消息导出、启动回收和一分钟定时回收实测通过；配置与密钥文件的校验值保持一致。本轮macOS check及release通过，普通Linux交叉check缺少 `x86_64-linux-gnu-gcc`，Windows交叉check缺少MSVC标准C头文件。安装验证后已执行 `cargo clean`，回收2.2 GiB编译产物。
