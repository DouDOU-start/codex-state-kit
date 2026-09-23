# 按 ChatGPT 账号统计用量与成本：设计草案

## 目标与口径

为经过 Kit 的上游请求记录可追溯的用量和**估算成本**，按实际使用的 ChatGPT 账号汇总。这里的“成本”由 Kit 配置的价格规则计算，不代表 ChatGPT 订阅的实际账单，也不涉及下游用户余额扣减。价格没有配置或上游没有返回可用用量时，显示“未定价”或“用量未知”，不能当作 0 元。

账号切换、Codex 工作目录切换和 Kit 重启后，历史记录及汇总仍可查询。每笔请求的归属在转发前固定；旧账号的流式响应晚于账号切换结束，仍记在旧账号下。

## 当前代码中的接入点

| 现状 | 影响与处理 |
| --- | --- |
| `login::request_credentials` 能取得实际选用凭据的 `account_id`；`proxy::forward_http_tracked` 会将账号写入网络日志。 | 以**选用的凭据**作为账号归属依据，不以客户端可填的 `ChatGPT-Account-ID` 请求头或邮箱作主键。计费开启后，若请求头与凭据不一致，需在发送前纠正或拒绝，避免错账。 |
| `logs::ResponseBodyMetrics` 已旁路解析 SSE/JSON，含 gzip、deflate、zstd 支持，但当前只提取 `output_tokens`。 | 扩充为完整用量快照：`input_tokens`、`output_tokens`、`input_tokens_details.cached_tokens` 等。解析仍只读取上游元数据，不收集响应正文。 |
| `proxy::ResponseLogTracker` 能获知完成、错误、客户端取消；网络日志保存在容量有限的内存队列。 | 在独立的计费服务中落库；不要复用网络日志的自增 ID、内存队列或设置 JSON。 |

旧版的后台取票请求曾以 `token_fetch`、`reverify` 来源记账；取票机制已移除，这些历史记录保留在数据库中，仍计入账号总计。

## 数据存储

使用本机 SQLite。当前实现放在用户目录下的 `.codex-state-kit-billing.sqlite3`（开发版为 `.codex-state-kit-dev-billing.sqlite3`）；位置不跟随 `codex_home`，因此切换工作目录不会把历史账单切断。数据库不保存 access token、refresh token、提示词、响应正文或代理密码。

建议启用 WAL、外键、`busy_timeout`，对计费写入使用同步提交。schema 带版本号并做迁移；启动时先打开和迁移数据库，再启动可计费的代理。定期备份数据库时应使用 SQLite 备份 API 或先做一致性快照，不能只复制主 `.sqlite3` 文件而忽略 WAL。

最小表结构如下；金额用整数 `cost_nanos`（十亿分之一美元）或等价定点数，价格规则用十进制数，避免浮点累加误差。

```sql
CREATE TABLE accounts (
  id INTEGER PRIMARY KEY,
  provider TEXT NOT NULL,                 -- chatgpt
  upstream_account_id TEXT NOT NULL,
  display_email TEXT,                     -- 仅展示，可变
  first_seen_at TEXT NOT NULL,
  last_seen_at TEXT NOT NULL,
  UNIQUE(provider, upstream_account_id)
);

CREATE TABLE usage_records (
  request_id TEXT PRIMARY KEY,            -- 每次上游尝试的 UUID
  account_id INTEGER NOT NULL REFERENCES accounts(id),
  source TEXT NOT NULL,                   -- business/token_fetch/reverify
  started_at TEXT NOT NULL,               -- UTC
  finished_at TEXT,
  state TEXT NOT NULL,                    -- pending/measured/missing_usage/interrupted
  http_status INTEGER,
  requested_model TEXT,
  sent_model TEXT,
  response_model TEXT,
  input_tokens INTEGER,
  cached_input_tokens INTEGER,
  output_tokens INTEGER,
  usage_source TEXT,                      -- provider_response 等
  pricing_rule_id INTEGER,
  cost_nanos INTEGER,                     -- 未定价/未知用量时 NULL
  error_kind TEXT
);

CREATE INDEX usage_by_account_time ON usage_records(account_id, started_at);
CREATE INDEX usage_by_state_time ON usage_records(state, started_at);
```

另设 `pricing_rules` 表，存 provider、模型、计费单位、输入/缓存输入/输出单价、`effective_from` 和币种。一次请求结算时，按**请求开始时间**选规则，将规则 ID、相关单价和计算结果固定到记录；以后改价不重写旧账。初版账号只作为聚合和归属维度，若以后需要账号倍率，再增加账号范围字段或独立倍率表。初版仅支持已在代理中出现的文本 token 字段；新增计费维度前先扩充规则和用量结构。未匹配规则的请求保留 token 用量，金额为 `NULL`。

价格算法示例：`非缓存输入 = max(input_tokens - cached_input_tokens, 0)`；`成本 = 非缓存输入 × 输入单价 + 缓存输入 × 缓存单价 + 输出 × 输出单价`，各单价均按每百万 token 标价。若上游字段语义不同，先按协议归一化；不靠请求字节数或文本长度推算 token。需要账号倍率时作为可配置的价格规则快照处理，默认 1。桌面端通过 `set_billing_pricing` 命令写入或替换规则，价格修改不会回算历史记录；未写入规则的模型显示“未定价”。

## 写入时序与故障处理

1. 请求确定实际凭据、账号和最终发送模型后，生成 `request_id`，在转发前提交 `accounts` 更新与 `usage_records(state='pending')`。计费启用时，若此事务失败，拒绝转发并提示数据库错误，以免产生无记录的新请求。
2. 旁路解析响应，仅采信上游 `response.usage` / `usage`。SSE 的终态事件或非流式完整 JSON 给出用量时，用同一 `request_id` 幂等更新记录，计算价格并提交。更新须在终态数据发给客户端前完成；重复终态不能重复记费。
3. 正常结束但缺用量，记 `missing_usage`；客户端取消、超时、解码错误和中途断线记 `interrupted`，已读到的元数据可留存，但没有可靠终态用量时金额为 `NULL`。HTTP 失败不自动认定为零成本：上游可能已处理部分请求。
4. 下次启动扫描遗留的 `pending`，转为 `interrupted`，保留原账号和时间；不要伪造 0 token 或 0 成本。汇总同时显示“可计价请求数 / 用量未知请求数”，避免低估被误读为完整账单。

当前汇总只有在该账号和来源桶内的每条记录都有完整用量且命中价格规则时才返回成本；混入未知用量或未定价模型时返回 `NULL`，界面显示“未定价”。官方登录模式下，请求账号头若存在必须与本机登录凭据一致；账号头缺失时采用本机凭据归属，显式冲突时不把客户端自填账号头写成计费主键。

数据丢失保证的边界是：**已提交的账号、请求和结算结果重启后仍在；进程或系统在上游返回用量前中断的请求保留为待核对记录。** 无法从未收到的上游用量恢复精确成本。

## 查询与界面

新增独立的 Tauri 查询命令，按账号、时间段、来源、模型分页查询；汇总由数据库聚合，不从 `Status.logs` 或前端状态计算。页面可先做“账号用量”视图：账号选择器、今日/本月/自定义时间、业务与内部请求小计、输入/缓存/输出 token、估算成本、未知用量数和请求明细。邮箱仅作标签；同一 `provider + upstream_account_id` 在不同工作目录或重新登录后仍归为同一账号。

## 实施顺序与验收

1. 建立 SQLite 存储层、迁移、账号归属与请求开始记录；验证切换 A→B 后旧请求结束仍归 A，重启后 A/B 历史仍可查。
2. 扩充 SSE/JSON 用量解析和 `ResponseLogTracker` 结算；覆盖分块/压缩响应、重复终态、缺用量、断流、客户端取消和崩溃恢复。
3. 配置价格规则，验证改价不改历史、未知价不记 0。
4. 增加查询命令和界面，再用真实响应样本核对聚合值与逐笔记录。

## 参考

sub2api 将用量日志与账号统计费用分开保存，并允许账号统计采用独立定价与倍率。本项目只借鉴“逐笔用量、账号归属、价格快照、可追溯汇总”的结构，不引入其下游用户/API Key/钱包/订阅体系：

- [sub2api UsageLog](https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/service/usage_log.go)
- [sub2api 账号统计定价](https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/service/account_stats_pricing.go)
- [sub2api 账号统计口径](https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/pkg/usagestats/account_stats.go)
