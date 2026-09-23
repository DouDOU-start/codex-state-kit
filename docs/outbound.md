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

Kit 会把 `{session}` 换成随机值：上游 WebSocket 连接在存活期内沿用同一个 session，HTTP 请求每次生成新的 session。日志和状态只显示 session，不会回显密码。

### 经系统代理连接

Clash Verge 等软件只开系统代理、没开 TUN 时，浏览器能出网，但 Kit 直接连国外的代理服务器可能不通。「经系统代理连接手动代理」默认开启：连接不在本机的手动代理时，Kit 先检测系统代理，存在则经它（HTTP CONNECT 或 SOCKS5）连到手动代理，否则直连。系统代理开关变化对下一条连接生效，无需重启 Kit。

## 订阅节点

选择「订阅节点」，填写 Clash / Mihomo 订阅 URL、本地文件路径或分享链接正文。Kit 用内置 Mihomo 内核加载订阅，可在节点分组中切换节点和测速；未指定节点时使用订阅中的第一个。内核来源与校验值见[来源记录](../src-tauri/resources/mihomo/PROVENANCE.md)。

## 失败处理

线路不可用时业务请求返回 502，不会自动退回直连。业务请求优先走上游 WebSocket，握手失败时回退 HTTP SSE。
