# 计费与模型价格

[返回首页](../README.md)

Kit 为每次经它转发的 Codex 请求记录用量，并按模型价格计算**估算成本**，按实际使用的 ChatGPT 账号汇总。这里的成本只是按公开价格折算，不代表 ChatGPT 订阅的实际账单。没有价格或上游没有返回完整用量时显示「未定价」，不会当作 0。界面上的展示见[使用记录与降智识别](usage-records.md)。

## 模型价格

价格与 [sub2api](https://github.com/Wei-Shaw/sub2api) 保持一致，来源是它使用的 [model-price-repo](https://github.com/Wei-Shaw/model-price-repo)（LiteLLM 格式），只保留 OpenAI 的对话 / Responses 模型：

- 应用内置一份价格表，离线时也能计价。
- 运行期间每 10 分钟经当前出站线路比对远程的 sha256，有变化就下载、校验并替换，同时缓存到本机；「模型价格」页也可「立即同步」。
- 同步失败时继续使用现有价格表，并弹出提醒。

请求模型按 sub2api 的顺序匹配价格：规范名称、原名，去掉推理强度后缀（如 `-high`）或日期后缀后的名称，基础版本（`gpt-5.2-codex` → `gpt-5.2`），其他 `gpt-*` 名称最后回退到 `gpt-5.4`。计价优先用实际发往上游的模型（强制绑定后的模型），响应里的模型名只作后备。

## 计算方式

与 sub2api 的 `computeTokenBreakdown` 相同：

```text
非缓存输入 = 输入 - 缓存读 - 缓存写
成本 = 非缓存输入 × 输入价 + 缓存读 × 缓存读价 + 缓存写 × 缓存写价 + 输出 × 输出价
```

- 推理 Tokens 已包含在输出中，按输出计价。
- **服务档位**：Priority 使用价格表中的 Priority 价，缺省时为标准价的 2 倍；Flex 为标准价的一半。ChatGPT 账号的响应在快速模式下也会报告 `default`，因此以请求时选择的档位为准，响应只能把档位往低调。
- **长上下文**：输入超过模型的长上下文阈值时，输入、输出按价格表中的倍率加价。
- **缓存写**：gpt-5.6 / gpt-6 没有单独的缓存写价格时，按输入价的 1.25 倍计。

金额用整数纳美元（十亿分之一美元）保存，避免浮点累加误差；每条记录同时保存输入、缓存读、缓存写、输出各项成本，以及计价用的模型、档位和是否长上下文。价格表更新不会改写已结算的记录。

也可以通过 `set_billing_pricing` 命令写入手动价格规则（按模型精确匹配、带生效时间），命中时优先于价格表。

## 记录与结算

1. 请求确定实际使用的凭据和最终发送的模型后，先写入一条 `pending` 记录，再转发上游。记录的账号以 Kit 选用的凭据为准，不采信客户端自己填写的 `ChatGPT-Account-ID`。
2. 响应旁路解析，只采信上游返回的 `usage`。流结束后结算：用量完整记为 `measured` 并计价；正常结束但缺用量记为 `missing_usage`；客户端取消、超时、断流或上游错误记为 `interrupted`，已读到的用量保留，金额为空。
3. 同时记录首字耗时、传输方式、服务档位和[降智识别](usage-records.md#降智识别)结果。
4. 进程退出或崩溃时还未结算的请求，下次启动时转为 `interrupted`，不伪造 0 用量或 0 成本。

汇总只在一个账号的所有记录都有完整用量且都能计价时显示总成本；混入未知用量或未定价的记录时显示「未定价」，避免把不完整的数字当成完整账单。

旧版后台取票请求曾以 `token_fetch`、`reverify` 来源记账；取票机制已移除，这些历史记录保留在数据库中，仍计入账号总计，但不计价。

## 数据存储

使用本机 SQLite：用户目录下的 `.codex-state-kit-billing.sqlite3`（开发版为 `.codex-state-kit-dev-billing.sqlite3`）。位置不跟随 Codex 工作目录，切换目录不会切断历史记录。数据库启用 WAL、外键和同步提交，启动时自动迁移表结构。

同一 `provider + 账号 ID` 在不同工作目录或重新登录后仍归为同一账号，邮箱只作展示。数据库不保存 access token、refresh token、提示词、响应正文或代理密码。备份时请用 SQLite 备份 API 或先停止应用，不要只复制主文件而漏掉 WAL。

## 参考

sub2api 将用量日志与账号统计费用分开保存。本项目借鉴其逐笔用量、账号归属、价格快照和计费口径，不引入它的下游用户、API Key、钱包或订阅体系：

- [sub2api UsageLog](https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/service/usage_log.go)
- [sub2api 账号统计定价](https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/service/account_stats_pricing.go)
- [sub2api 账号统计口径](https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/pkg/usagestats/account_stats.go)
