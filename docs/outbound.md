# 出站代理

[返回首页](../README.md)

Kit 把 Codex 的业务请求、ChatGPT 登录和模型价格同步都经当前出站线路发出。线路绑定在账号上：每个账号记住自己的出站设置，切换账号时一起切换。

## 手动代理

选择「手动代理」，填写完整 URL，失焦后自动保存。例如：

```text
http://127.0.0.1:7890
socks5h://127.0.0.1:1080
socks5h://user:password@proxy.example.com:1080
```

支持 HTTP、HTTPS 和 SOCKS；`socks5://` 在实际连接时会转换为 `socks5h://`，由代理解析域名。账号和密码中的特殊字符需进行 URL 编码。

需要让住宅代理分配会话出口时，把 session 段写成 `{session}`：

```text
socks5://xmtt1126849-region-DE-sid-{session}-t-120:password@us.arxlabs.io:3010
```

Kit 会把 `{session}` 换成随机值：上游 WebSocket 连接共用同一个 session，直到切换账号或线路；回退到 HTTP 时每次请求生成新的 session。日志和状态只显示 session，不会回显密码。

### 经系统代理连接

Clash Verge 等软件只开系统代理、没开 TUN 时，浏览器能出网，但 Kit 直接连国外的代理服务器可能不通。「经系统代理连接手动代理」默认开启：连接不在本机的手动代理时，Kit 先检测系统代理，存在则经它（HTTP CONNECT 或 SOCKS5）连到手动代理，否则直连。系统代理开关变化对下一条连接生效，无需重启 Kit。

## 订阅节点

选择「订阅节点」，填写 Clash / Mihomo 订阅 URL、本地文件路径或分享链接正文。Kit 用内置 Mihomo 内核加载订阅，可在节点分组中切换节点和测速；未指定节点时使用订阅中的第一个。内核来源与校验值见[来源记录](../src-tauri/resources/mihomo/PROVENANCE.md)。

分享链接正文支持 `ss`、`vmess`、`vless`、`trojan`、`hysteria2`/`hy2`、`anytls` 和 `tuic`，也支持裸 Base64、`base64://` 与 `base64,` 前缀。需要完整协议覆盖时请使用包含 `proxies` 节点的 Clash / Mihomo YAML；只有 `proxy-providers` 的配置需要先由订阅客户端展开，URI 形式的 SSR、Hysteria v1、HTTP 和 SOCKS 分享链接暂不解析。

## 失败处理

### 延迟检测

当前节点旁的测速只检测该节点，目标为 `https://www.gstatic.com/generate_204`，单节点超时 4 秒。节点选择窗口里的「测全部节点」最多并发 4 个，避免逐个串行等待。

「Codex 链路检测」单独通过当前订阅出口请求配置的 Codex 上游，保留 8 秒超时。节点测速包含连接和 HTTPS 握手耗时，不是 ICMP ping；链路检测只表示 HTTP 可达性与响应耗时，不检查登录可用性或模型首字速度。

线路不可用时业务请求返回 502，不会自动退回直连。

## 上游 WebSocket

原生 WebSocket 客户端的业务请求和官方 Codex 客户端一样走上游 WebSocket，不需要设置。HTTP 客户端继续走 HTTP SSE，以确保 usage 事件完整并能计费：

- **预热**：Kit 在后台预先连好一条连接，请求到达时直接发送，省去握手时间；账号、设备或线路变化后立即按新的身份重新预热。
- **保活与换新**：空闲连接每 25 秒 ping 一次；连接用到约 50 分钟（上游约一小时断开）时在空闲期提前换新。
- **并发**：一条连接一次只跑一轮，并发请求各用一条，最多 8 条；带 `previous_response_id` 的续跑回到原来那条连接。多出来的空闲连接闲置 10 分钟后关闭。
- **自动重连**：连接在空闲时被上游断开，下一次请求会自动重连并重发，不会报错。
- **HTTP 客户端**：不经过 HTTP→WebSocket 桥接，直接使用 HTTP SSE；这样上游关闭连接时不会丢失 `response.completed` 中的 usage。
