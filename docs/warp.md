# 出站代理与 WARP

[返回首页](../README.md)

默认「同网获取并发送」时，WARP 或手动代理同时用于获取 Turn-State 和转发 Codex 业务请求。切到「Token 与业务分路」后，WARP / 手动代理只负责打票，业务仍走独立的「上游转发代理」。

## 内置 WARP

Windows x64 安装包包含 usque v4.2.1 用户态内核，无需另装 WARP、WireGuard 或网络驱动。隧道运行无需管理员权限。usque 是第三方开源实现，并非 Cloudflare 官方客户端。

应用默认使用内置 WARP，启动或切换到该模式后自动连接。就目前观察，WARP 出口相对固定，多数情况下较难稳定拿到完整能力的 Token；需要不降智票据时，请改用下方手动代理，并参阅首页[出站代理与 Token](../README.md#出站代理与-token)。

连接步骤：

1. 首次连接注册设备身份，后续复用。
2. 后台验证出口，界面显示准备中、已就绪或重试状态。
3. QUIC 不可用时自动尝试 TCP。
4. 切回手动代理或退出应用时停止内核。

连接期间约每 20 秒检查状态，启动失败后约 60 秒重试。WARP 不可用时，Turn-State 获取不会自动改用直连或旧的手动代理。本地 SOCKS 监听使用动态端口和随机认证信息。

使用 WARP 受 [Cloudflare 服务条款](https://www.cloudflare.com/application/terms/)约束，界面也提供条款入口。

## 手动代理

选择「手动代理」，填写完整 URL，失焦后自动保存。例如：

```text
http://127.0.0.1:7890
socks5h://127.0.0.1:1080
socks5h://user:password@proxy.example.com:1080
```

建议使用 HTTP 或 SOCKS5；获取 Turn-State 时，`socks5://` 会转换为 `socks5h://`，由代理解析域名。账号和密码中的特殊字符需进行 URL 编码。

需要轮换住宅代理出口时，把 session 段写成 `{session}`：

```text
socks5://xmtt1126849-region-DE-sid-{session}-t-120:password@us.arxlabs.io:3010
```

Kit 每次打票都会生成新的 session。拿到稳定 292 后，把该 session 绑在票据上，同网策略下的业务请求继续走同一条出口，直到票据过期、312 失效或你改了代理地址。日志和状态只显示 session，不会回显密码。

切换到 WARP 会保留手动地址。旧配置已设置代理时继续使用原模式；没有代理且未保存模式的旧配置迁移为 WARP。

## 上游转发代理

仅在网络策略为「Token 与业务分路」时显示。填写独立的业务转发代理，例如 Clash 的 HTTP / mixed 端口：

```text
http://127.0.0.1:7897
```

保持 Clash 运行，填写实际监听端口，无需开启 TUN。支持 HTTP、HTTPS 和 SOCKS；`socks5://` 使用代理端 DNS。认证信息可写入 URL，特殊字符需要 URL 编码。

失焦或 Enter 保存后，新业务请求立即使用新代理，在途流式响应继续完成。此设置不影响登录、Token 获取或 WARP，也不清空 Token 缓存。留空恢复原有默认网络行为（可能受进程代理环境变量影响），并非强制直连。本功能不增加操作系统代理自动识别。同网策略下不会使用该字段。

代理不可用时业务请求返回 502，不会自动退回直连。WebSocket 仍返回 426，由客户端回退到 HTTP SSE。

## 数据与分发

正式版数据目录为 `%LOCALAPPDATA%/io.codexstatekit.desktop/warp/`；开发版为同一应用目录下的 `dev/warp/`。

| 文件 | 用途 |
| --- | --- |
| `config.json` | 设备身份和私钥 |
| `registration.json` | 注册期间的临时文件 |
| `warp.log` | 最近一次连接的内核日志 |

分发独立主程序时必须同时携带安装目录中的 `warp/` 资源，推荐使用 MSI 或 EXE 安装包。内核来源与校验值见[来源记录](../src-tauri/resources/warp/PROVENANCE.md)。

WARP 不提供按请求轮换 IP 或指定国家的保证。重连可能保持原出口，连通性与 Token 获取结果取决于网络和上游服务。浏览器预览不会建立真实隧道。
