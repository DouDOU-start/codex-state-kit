# 配置与自动接入

[返回首页](../README.md)

## 工作目录

「Codex 工作目录」指存放 `config.toml` 和 `auth.json` 的配置目录，不是代码仓库目录，在「Codex 接入」页的「转发设置」中修改。默认值为用户目录下的 `.codex`；使用自定义目录时，请与 Codex 客户端保持一致。

修改后，输入框失焦或按 Enter 即保存。应用取消进行中的登录、恢复旧目录路由，重新读取新目录账号并加载配置；出站代理设置保留。新目录必须存在，已有 `config.toml` 必须可以解析。

## 自动接入与恢复

本机监听就绪且已有 ChatGPT 登录时，应用自动接入。只有 API Key 的配置不满足自动接入条件。

接入时 Kit 备份官方 `auth.json`，用当前账号的登录覆盖它（见[账号与登录](accounts.md#登录文件)），并记录原值、修改 `config.toml` 中的顶层字段：

```toml
openai_base_url = "http://127.0.0.1:8787"
```

应用保留其他配置及注释，不新建自定义模型提供方。使用已有自定义提供方时，该顶层字段不代表其请求一定经过本应用，应结合请求日志确认。
接入时会清理本地模型、reasoning 和自定义目录覆盖，让 Codex 使用 Kit 映射的官方模型目录；已有 `service_tier`（例如 `priority`）会原样保留。Fast/普通模式仍由 Codex 客户端选择，请求字段原样转发，退出 Kit 也保留配置中的最新选择。由旧版接入流程删除的 `service_tier` 会在升级后从路由恢复记录中自动补回。

| 场景 | 处理 |
| --- | --- |
| 正常退出 | 恢复接入前的路由和 `auth.json`；原字段不存在时删除该字段 |
| 切换工作目录 | 恢复旧目录，按新目录的登录状态决定是否接入 |
| 上次异常退出 | 下次成功启动监听后，尝试恢复遗留路由，再自动接入 |
| 接入新目录失败 | 尝试恢复旧目录的接入状态 |

路由变化后建议重新启动 Codex 客户端。已保存的账号不会因退出或恢复而删除。

兼容旧版本时，应用会清理自身遗留的提供方配置。如果存在旧的 `config.toml.codex-state-kit.bak`，恢复时会一次性使用该文件并删除它；当前接入流程不再生成整份配置备份。

## 文件位置

以下 `~` 表示用户目录，Windows 通常为 `%USERPROFILE%`；实现优先读取 `HOME`，其次读取 `USERPROFILE`。

| 内容 | 正式版 | 开发版 |
| --- | --- | --- |
| 应用设置 | `~/.codex-state-kit.json` | `~/.codex-state-kit-dev.json` |
| 路由恢复记录 | `~/.codex-state-kit.backup.json` | `~/.codex-state-kit-dev.backup.json` |
| 使用记录 | `~/.codex-state-kit-billing.sqlite3` | `~/.codex-state-kit-dev-billing.sqlite3` |
| 模型价格缓存 | `~/.codex-state-kit-pricing.json` | `~/.codex-state-kit-dev-pricing.json` |
| 虚拟设备 | `~/.codex-state-kit-vm.json` | `~/.codex-state-kit-dev-vm.json` |
| 虚拟设备 installation ID | `~/.codex-state-kit-installation_id` | `~/.codex-state-kit-dev-installation_id` |
| 本机代理端口 | `8787` | `8788` |

账号相关文件保存在所选工作目录中，见[账号与登录](accounts.md#登录文件)。订阅内核的数据位于系统应用数据目录下的 `mihomo/`（开发版为 `dev/mihomo/`）。旧版的 Turn-State 缓存文件 `~/.codex-state-kit-token.json` 和内置 WARP 的数据目录（应用数据目录下的 `warp/`）已不再使用，启动时会自动删除。

虚拟设备配置文件由界面维护。字段 `enabled` 控制是否模拟设备和模型环境，默认值为 `true`；关闭后请求中的客户端设备与环境信息原样透传。旧文件没有该字段时仍按 `true` 读取。多窗口请求按来源 session/window 映射到稳定的虚拟作用域；没有来源标识的 HTTP 请求按请求隔离，WebSocket 按连接隔离。持久化的 `installation_id` 同时写入独立的 sidecar 文件，并在写入时加锁和同步，避免并发启动时生成多个设备身份。

同一版本只能运行一个 Kit：再次启动时会直接切到已打开的窗口。开发版与正式版的应用数据分开保存，但默认 Codex 工作目录相同，不要让两者同时管理同一个工作目录。

## 高级设置

日常使用通过界面配置即可。需要手动编辑应用设置时，先退出应用，再修改 JSON 文件：

| 字段 | 用途 |
| --- | --- |
| `proxy_listen` | 本机监听地址，默认 `127.0.0.1:8787`；开发版为 `8788` |
| `lan_access_enabled` | 是否把同一端口扩展到局域网接口，默认 `false`；开启前需要先生成 API Key |
| `lan_api_key_hash` | 局域网 API Key 的 SHA-256 哈希；只保存哈希，不要手动填写明文 Key |
| `upstream` | 上游地址，默认 `https://chatgpt.com/backend-api/codex` |
| `codex_home` | Codex 配置目录的完整路径 |
| `outbound_mode` | `manual`（默认）或 `mihomo`；旧值 `warp` 按 `manual` 读取 |
| `outbound_proxy` | 手动代理 URL；切换模式时保留。可把出口写成 `{session}`，见[出站代理](outbound.md) |
| `mihomo_subscription` | 订阅 URL、本地文件路径或分享链接正文 |
| `mihomo_node` | 固定使用的节点名；留空使用订阅中的第一个 |
| `chain_system_proxy` | 是否经系统代理连接手动代理，默认 `true` |
| `forced_model` | 强制绑定的上游模型 ID，例如 `gpt-6-astra`；填写后下游无论请求什么模型都会改成该值再转发。留空保持下游原模型 |

## 局域网访问

在“转发设置”中开启“允许局域网访问”并生成 API Key 后，Kit 会继续使用当前端口，但把监听地址扩展到局域网接口。界面会直接显示当前主路由对应的局域网 IPv4 地址；自动注入到本机 Codex 的地址仍然是 `http://127.0.0.1:<端口>`，本机 Codex 的账号注入流程不变。

远端客户端使用 Kit 主机的局域网地址和生成的 Key：

```text
Base URL: http://<Kit主机IP>:8787
Authorization: Bearer <Kit API Key>
```

也兼容 `x-api-key: <Kit API Key>` 和 `api-key: <Kit API Key>`。

下游 Base URL 可以使用 `http://<Kit主机IP>:8787` 或 `http://<Kit主机IP>:8787/v1`：`/models` 与 `/v1/models`、`/responses` 与 `/v1/responses` 会转发到相同的上游路径，查询参数保留。配置的上游前缀保持不变，例如默认上游使用 `/backend-api/codex/responses`，上游 Base URL 已含 `/v1` 时也不会重复拼接。

使用默认 ChatGPT 上游时，下游只需采用标准的 OpenAI Responses 协议；路径别名不会把 Chat Completions 的请求和响应转换成 Responses。Kit 会在转发前补齐上游要求的 `store: false`，移除 ChatGPT Codex 不接受的 `max_output_tokens`，并在 HTTP 请求中移除无法由上游解析的 `previous_response_id`；请求 `/models` 时也会补上虚拟 Codex 客户端版本。这样下游不需要伪装成 Codex CLI，Kit 会统一以虚拟设备身份向上游请求。`/responses` 返回 `400` 时，仍需根据上游返回的错误检查其他请求字段，不能仅靠增删 `/v1` 解决。

开发版端口默认为 `8788`。Key 只在生成或重新生成时显示一次；重新生成会立即使旧 Key 失效。启用后同一端口上的请求需要有效的 Kit API Key 或当前本机 Codex 的 ChatGPT 凭据，建议只在可信局域网使用，并通过系统防火墙限制端口来源。不要把端口直接暴露到公网。

旧版的 `upstream_proxy` 会迁移到 `outbound_proxy`；Turn-State 相关的旧字段（`models`、`state_miss_policy`、`token_reuse_policy` 等）和 `ws_upstream_enabled`（上游现在固定走 WebSocket）读取时忽略，下次保存时移除。

应用设置可能含代理密码，账号文件也包含凭据；提交问题报告时不要附上这些文件的原文。
