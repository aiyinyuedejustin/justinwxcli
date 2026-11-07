# macOS 微信取钥与读取权限

数据库读取与进程取钥是两项能力。`wx export`直接使用 `~/.wx-cli/all_keys.json`打开本地数据库；`wx key extract`从正在运行的微信读取新密钥。

## 当前实机结果

2026-11-07，macOS本机微信 4.2.0（270345）：

- 17份已有密钥通过数据库首页HMAC-SHA512校验，页面4096字节，保留区80字节。
- 完整导出和独立SQLCipher计数验证通过。
- 微信为官方Hardened Runtime签名，entitlements中缺少`com.apple.security.get-task-allow`。
- Developer mode已启用，本机终端sudo执行内存扫描返回`task_for_pid kr=5`。

Apple文档规定，调试目标的授权也参与task port访问判定：[Debugging tool entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.debugger)。因此开启开发者工具与sudo并不能保证对官方发行版微信的进程读取成功。

## 操作入口

```sh
./bin/wx doctor --json
sudo ./bin/wx key extract --hook-seconds 0
```

上述取钥只运行内存扫描。允许读取的微信进程中，扫描器同时寻找传统明文key/salt和新版Config.Cipher XOR混淆候选，再用真实数据库首页验证。缺少冷分片密钥时可显式使用`--hook-seconds 90`，在等待期间打开相关聊天。

遇到`task_for_pid kr=5`时，保留有效密钥，检查目标微信的签名和调试授权。LLDB同样受目标进程保护约束。更改微信签名、添加调试授权、重启微信会改变用户安装的应用，需要用户明确决定并准备恢复方式；本工具不会自动执行这些操作。

上游[PR #27](https://github.com/huohuoer/wechat-cli/pull/27)的成功实机案例使用的是已ad-hoc签名、带get-task-allow的微信。该案例支持混淆密钥扫描算法，对官方原始签名的进程访问仍须按实际系统结果判断。

密钥属于本机微信数据库，已验证密钥可以在本工具自己的状态目录持续使用。更换微信账号、清空本地数据或出现新加密分片后，使用`doctor`检查覆盖范围，再处理具体缺失。
