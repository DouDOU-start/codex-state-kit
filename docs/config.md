# 配置与自动接入

[返回首页](../README.md)

## 工作目录

「Codex 工作目录」指存放 `config.toml` 和 `auth.json` 的配置目录，不是代码仓库目录。默认值为用户目录下的 `.codex`；使用自定义目录时，请与 Codex 客户端保持一致。

修改后，输入框失焦或按 Enter 即保存。应用取消进行中的登录、恢复旧目录路由，重新读取新目录账号并加载配置；出站代理设置保留。新目录必须存在，已有 `config.toml` 必须可以解析。

## 自动接入与恢复

本机监听就绪且工作目录中已有 ChatGPT 登录时，应用自动接入。只有 API Key 的配置不满足自动接入条件。

接入会记录原值，并修改 `config.toml` 中的顶层字段：

```toml
openai_base_url = "http://127.0.0.1:8787"
```

应用保留其他配置及注释，不新建自定义模型提供方。使用已有自定义提供方时，该顶层字段不代表其请求一定经过本应用，应结合请求日志确认。
接入时会清理本地模型、reasoning 和自定义目录覆盖，让 Codex 使用 Kit 映射的官方模型目录；已有 `service_tier`（例如 `priority`）会原样保留。Fast/普通模式仍由 Codex 客户端选择，请求字段原样转发，退出 Kit 也保留配置中的最新选择。由旧版接入流程删除的 `service_tier` 会在升级后从路由恢复记录中自动补回。

| 场景 | 处理 |
| --- | --- |
| 正常退出 | 恢复接入前的路由；原字段不存在时删除该字段 |
| 切换工作目录 | 恢复旧目录，按新目录的登录状态决定是否接入 |
| 上次异常退出 | 下次成功启动监听后，尝试恢复遗留路由，再自动接入 |
| 接入新目录失败 | 尝试恢复旧目录的接入状态 |

路由变化后建议重新启动 Codex 客户端。恢复路由不会删除或回滚 `auth.json`。

兼容旧版本时，应用会清理自身遗留的提供方配置。如果存在旧的 `config.toml.codex-state-kit.bak`，恢复时会一次性使用该文件并删除它；当前接入流程不再生成整份配置备份。

## 文件位置

以下 `~` 表示用户目录，Windows 通常为 `%USERPROFILE%`；实现优先读取 `HOME`，其次读取 `USERPROFILE`。

| 内容 | 正式版 | 开发版 |
| --- | --- | --- |
| 应用设置 | `~/.codex-state-kit.json` | `~/.codex-state-kit-dev.json` |
| 路由恢复记录 | `~/.codex-state-kit.backup.json` | `~/.codex-state-kit-dev.backup.json` |
| Turn-State 缓存 | `~/.codex-state-kit-token.json` | `~/.codex-state-kit-dev-token.json` |
| 本机代理端口 | `8787` | `8788` |

账号文件保存在所选工作目录的 `auth.json`。WARP 数据位置见[出站代理与 WARP](warp.md)。

开发版与正式版的应用数据分开保存，但默认 Codex 工作目录相同。不要让两个实例同时管理同一个工作目录。

## 高级设置

日常使用通过界面配置即可。需要手动编辑应用设置时，先退出应用，再修改 JSON 文件：

| 字段 | 用途 |
| --- | --- |
| `proxy_listen` | 本机监听地址，默认 `127.0.0.1:8787`；开发版为 `8788` |
| `upstream` | 上游地址，默认 `https://chatgpt.com/backend-api/codex` |
| `codex_home` | Codex 配置目录的完整路径 |
| `outbound_mode` | `warp` 或 `manual`，新配置默认 `warp` |
| `outbound_proxy` | 手动代理 URL；切换模式时保留 |
| `upstream_proxy` | 独立的上游业务转发代理，默认空；例如 `http://127.0.0.1:7897`，保存后对新请求生效 |
| `warp_http2` | 是否优先使用 TCP，默认 `false`；应用支持自动回退 |
| `token_reuse_policy` | `shared_292`（默认，同账号跨模型复用精确 292 字节票据）或 `per_model`（旧策略，按模型独立）；旧配置未填写该字段时也使用新默认 |

应用设置可能含代理密码，账号和 Token 文件也包含凭据；提交问题报告时不要附上这些文件的原文。
