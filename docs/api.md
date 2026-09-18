# Codex Proxy RS 接口

本文列出 v3 源码中的公开 HTTP 接口，路由以
`backend/crates/gateway-api/src` 中的 router 为准。配置 Codex 请先看 [客户端配置](../deploy/README.md#客户端配置)；
运行实例是否包含这些功能，应结合其版本和 revision 确认。

## 1. 鉴权与公共约定

### 客户端接口

所有 `/v1/*` 请求都使用管理端创建的 Client Key：

```http
Authorization: Bearer sk_...
```

Codex 原生生图配置还会携带 `X-OpenAI-Actor-Authorization: proxy-managed`。
它仅用于客户端识别服务端托管认证，不能代替 Client Key。网关和 OpenAI Provider 都会过滤该请求头，
上游账号身份只由服务端选中的账号提供；不要把真实账号 token 放进该标记。

Client Key 通过账号分组限定路由范围：未绑定分组时可使用全部账号，绑定一个或多个分组时只能使用
已启用分组成员的并集。分组可以混合 `openai` 与 `xai` 账号；同一请求只会在模型能力明确匹配且满足
重放安全边界时跨 Provider fallback。

运行设置可以分别配置 `minCodexDesktopVersion` 与 `minCodexCliVersion`。两者只接受 SemVer，`null`
表示不限制。API 在 Client Key 鉴权成功后识别官方 Desktop/CLI 请求头；已识别客户端没有合法版本，或版本
低于对应门槛时，所有 `/v1/*` HTTP 请求和新 WebSocket 握手在访问上游前返回 `426 Upgrade Required`。
未知客户端保持兼容，不应用版本门禁。

低版本响应使用 OpenAI 风格错误格式：

```json
{
  "error": {
    "message": "Codex CLI 0.151.0 is below the minimum required version 0.152.0. Upgrade Codex CLI and retry.",
    "type": "invalid_request_error",
    "code": "client_version_too_old",
    "client": "codex_cli",
    "current_version": "0.151.0",
    "min_version": "0.152.0"
  }
}
```

已识别但缺失或携带非法版本时，`code` 为 `client_version_unavailable`，`current_version` 为 `null`。

### 管理接口

除登录、会话状态和登出外，所有 `/api/admin/*` 请求都需要以下任一鉴权方式：

- 浏览器登录后得到的 `cpr_admin_session` Cookie；
- `x-api-key: <admin-api-key>`。

请求无需自带 `x-request-id`；缺失时服务端自动生成 UUID 并在响应头回传同一 request ID。
`api.request_id_header` 可改变注入与回传的 header 名，管理端鉴权不依赖该名字。
管理端响应统一带 `Cache-Control: no-store`。

配置了 CORS 白名单 origin 时，跨域请求以凭据模式放行，仅允许 `GET`/`POST` 方法和
`authorization`、`content-type`、`x-api-key` 与 request ID 四个请求头，不使用通配符。

普通成功响应使用以下信封：

```json
{
  "code": 200,
  "message": "OK",
  "data": {}
}
```

所有 `/api/admin/*` 错误（包括 JSON/Query rejection、未知路由和错误 HTTP method）统一返回
`application/json`：

```json
{
  "code": 40001,
  "message": "请求参数不合法",
  "data": null
}
```

管理端本地产生的 `message` 是可安全展示的中文文案；Store、Serde、Provider 内部 `Display` 和原始上游
body 不进入这个通用信封。稳定业务码如下：

| HTTP | `code` | 含义 |
| ---: | ---: | --- |
| 400 | `40000` | 请求体不是合法 JSON |
| 400 / 405 / 415 / 422 | `40001` | 通用请求、方法、Content-Type 或字段错误；HTTP 状态保留具体语义 |
| 400 | `40002` | 时间范围不合法 |
| 401 | `40101` / `40102` / `40103` | 缺少管理员会话 / 登录凭据错误 / 管理 API Key 错误 |
| 404 | `40401` | 资源或管理接口不存在 |
| 409 | `40901` | 资源状态冲突 |
| 429 | `42901` | 登录尝试过多 |
| 500 | `50001` | 服务内部错误 |
| 502 | `50201` | 上游服务请求失败 |
| 502 | `50202` | 不可逆上游操作的执行结果未知；刷新状态后再决定是否重试 |
| 503 | `50301` | 依赖服务暂不可用 |

未知 `/api/admin/*` 路径使用 `40401`，不会落入 SPA；已存在路径使用错误 method 时返回 `405`、
`40001`，并保留标准 `Allow` header。request ID 继续通过配置的响应 header 返回。

Provider 管理适配使用静态 `public_message` 提供可操作的具体原因，Admin 用例完成安全消息选择后，
API 对 `50201`、`50202` 和 `50301` 也保留该消息，不再用固定错误覆盖；缺少安全消息时仍回退到通用提示。
认证错误和未知内部错误继续使用固定文案，不公开 Provider 内部 message、原始响应或凭据。

手动刷新令牌时，容量／账号租约占用和账号快照冲突仍为 `40901`，但分别提示等待或刷新账号列表。
OpenAI 已收到的刷新失败响应不再统一归为资源冲突：原先落入宽泛 Transport 分类的明确拒绝使用
`50201`，按已解析的错误码区分令牌过期、已使用、已撤销和 `invalid_grant`；刷新接口返回
`token_expired` 时提示“刷新令牌不可用，请重新授权”，不据此断言具体失效原因。无法确认刷新结果时使用
`50202`，提示先核对账号状态、不要立即重复刷新。缺少刷新令牌及原有明确凭据无效分支仍为 `40001`。
上游 `401` 不代表管理员会话失效，也不会触发管理端重新登录。上述变化只修正管理错误的分类和展示，
不改变后台自动刷新、401 恢复退避或账号终态策略；客户端不得仅因状态码从 `409` 改为 `502` 自动重发刷新。
xAI 手动刷新返回无效的新凭据时，从 `40001` 改为 `50202`，因为上游可能已经轮换了旧 RT；
未能完成刷新、但没有明确凭据永久失效证据的 `Rejected` 从 `40001` 改为 `50201`，不再一概提示凭据无效。
Codex PAT 验证服务不可用和身份响应无效分别通过 `50301`、`50201` 保留原有具体提示。

### 管理写入一致性

管理写入不要求客户端提供全局配置版本。会改变路由快照或安全配置的写入由后端在事务内推进
内部 `config_revision`，并用于快照发布与审计。账号更新和分组查询/写入的部分响应会返回
`configRevision` 作为已提交事实，但它不是客户端 mutation 的前置条件。

## 2. 健康检查

| 方法 | 路由 | 鉴权 | 说明 |
| --- | --- | --- | --- |
| `GET` | `/healthz` | 无 | Core、Store 和后台任务健康时返回 `204`，否则返回 `503` |

## 3. OpenAI 数据面与模型目录

除下述 Responses 入站解压保护外，Responses、Images 和 standalone Search HTTP body、
WebSocket message 和 frame 不设置网关私有长度上限；协议可接受性由上游决定。启用 OpenAI 环境与设备
metadata 覆盖时，Responses 和 standalone Search 按下文只修改指定的环境、位置与 metadata 字段；其他字段
仍按原协议处理。

| 方法 | 路由 | 说明 |
| --- | --- | --- |
| `POST` | `/v1/responses` | OpenAI Responses JSON；`stream=true` 返回 SSE，否则返回完整 JSON |
| `GET` | `/v1/responses` | 通过 HTTP Upgrade 建立 Responses WebSocket |
| `POST` | `/v1/alpha/search` | Codex standalone web search；正文覆盖关闭或无目标变化时原样转发，启用时只修改指定位置字段 |
| `POST` | `/v1/images/generations` | 通过 OpenAI Provider 发起图像生成；JSON 请求与响应正文原样转发 |
| `POST` | `/v1/images/edits` | 通过 OpenAI Provider 发起图像编辑；JSON 请求与响应正文原样转发 |
| `GET` | `/v1/models` | 返回当前 Client Key 账号范围内各 Provider 的可用公开模型并集；有两种响应形态，见下 |
| `GET` | `/v1/models/{model_id}` | 返回 OpenAI 兼容的单模型详情 |

Codex 的 review 等子代理请求仍使用 `/v1/responses`，并通过 `x-openai-subagent` 请求头携带子代理类型；
网关不提供独立的子代理请求路径。

`POST /v1/responses` 在鉴权后按 `Content-Encoding` 解压，再解析 JSON；支持单一 `gzip`、
`deflate`（zlib 封装）和 `zstd`，缺省、空值或 `identity` 直接使用原始正文。gzip 多成员与 zstd
多帧连续解码，整体展开结果最多 64 MiB，超限在继续展开前返回 `400 request_too_large`；zstd
回溯窗口同样最多 64 MiB，不能满足该限制的帧按解码失败处理。这个限制保护入站解压资源，不是
模型上下文或 Token 上限，也不新增未压缩正文的长度限制。
不支持的编码、逗号分隔的叠加编码和重复 `Content-Encoding` 头返回
`400 unsupported_content_encoding`；压缩正文损坏、截断或解压后不是合法 JSON 返回
`400 invalid_json`。本地错误不包含原始正文或解压库细节。WebSocket 文本帧不经过这条解压路径。

Responses 不透传下游的逐跳头、反代元数据（如 `cf-*`、`x-forwarded-*`、`forwarded`、`via`、
`cdn-loop`）以及 `Accept-Encoding` / `Content-Encoding`。链路元数据和编解码能力
由各段传输层独立管理；其余业务扩展头继续透传，不使用固定业务头白名单。
此规则同时适用于上游 HTTP 和 WebSocket，不影响上游响应的 `cf-ray` 等诊断信息。

Responses WebSocket 仅接受文本 `response.create`，同一连接串行执行。当前响应期间收到的后续业务帧
留在有界接收队列中，待当前响应完成终结和写出后再逐条校验、准入与执行，不因请求提前到达而断开。
这对齐 Codex 客户端 `stream_request` 持锁至本轮结束的串行行为，不表示支持额外控制消息类型。
接收队列容量为 32 个事件，超载仍关闭连接；Ping/Pong、客户端关闭和服务关闭不等待队列中的请求执行。

客户端使用 HTTP/SSE 时，OpenAI Provider 仍可能选择上游 WebSocket。
客户端配置的 `supports_websockets` 只控制第一段连接，不是服务端传输策略开关。
上游在响应终态前发送 Close 1000 仍属于失败，不能按“正常关闭”计为成功。

已建立模型执行的 Responses、Images 和 Search HTTP 响应使用现有 ID：
`x-gateway-request-id` 为模型执行 ID；`x-request-id` 保留有效上游值，只有上游
`x-oai-request-id` 时复用其值，没有上游 ID 时使用模型执行 ID。`x-oai-request-id` 不是必需字段，
也不要求客户端识别它；OpenAI 与 xAI 路由使用相同规则。失败响应的关联 ID 不采用会话 opening ID，
错误正文读取失败时仍返回已知上游 ID；已采集的 turn state 等允许的会话头继续按原合同交付。
尚未建立执行的入口拒绝继续使用 middleware 的入口关联。

WebSocket 在尚未交付上游业务事件时合成的错误保留已确认的失败状态，以及 Provider 提取的结构化
message/type/code；没有结构化错误时使用稳定安全文案，不把原始 HTML 或截断正文当作 message。
合成错误自身的 `headers` 携带允许下发的响应头：优先保留实际失败的上游 request ID，无上游 ID 时
提供网关关联 ID，并用 `x-gateway-request-id` 独立标识网关请求。已经取得的原始上游错误帧不重写。
客户端可能对特定状态另行统一展示；这不构成网关改写真实状态码的理由。

`GET /v1/models` 默认返回 OpenAI 兼容列表 `{"object": "list", "data": [...]}`；请求携带非空
`client_version` query 参数（Codex 客户端）时改为返回 Codex 专用目录合同 `{"models": [...]}`。

Codex 专用目录中的 `context_window` 与 `max_context_window` 分别表示默认上下文窗口和客户端本地
覆盖的上限。OpenAI Provider 分别传递上游目录中的对应字段，缺失时保留 `null`；网关不通过部署配置
覆盖这些值。Codex 客户端配置 `model_context_window` 后，按该值与非空 `max_context_window` 的较小值
使用窗口；上限为空时保留客户端本地值。xAI 目录只声明一个窗口，其 Provider 继续以该值作为客户端覆盖上限。

环境与设备 metadata 覆盖关闭时，OpenAI 路径保留客户端 Responses wire 语义：解析后的请求结构中未知字段、字段值和顺序保持不变（受控模型
映射除外），HTTP SSE 与 WebSocket 的上游业务事件字节原样转发，response ID 按 opaque 值处理而不
假设 UUID 或固定长度；OpenAI 上游错误 envelope 和允许下发的 opaque header 值也不由 canonical
观测结果重写。

启用环境与设备 metadata 覆盖时，请求只在下文列出的目标字段上例外更新；覆盖关闭或目标字段没有变化时，
Responses 保留解析后的请求结构，但不保证输入 JSON 的空白与转义字节；standalone Search 保留原始正文 bytes。

Images 请求不读取或重建 JSON，也不要求或映射模型字段；它固定使用 OpenAI Provider，
只在原始字节之外完成账号选择、鉴权头替换和端点路由，成功与失败响应正文同样保持原始字节。
`/v1/alpha/search` 使用相同的 OpenAI Provider 原生端点边界：body 中的 `model` 不映射；地域覆盖
关闭时不解析。启用时，standalone Search 只处理 `settings.user_location`；Responses 只处理已声明且受支持的
`web_search` location 对象，字段规则见 [运行设置](#8-运行设置)。`x-codex-turn-metadata` 在移除客户端账号身份并按
当前 lease 重写 installation ID 后转发；上游账号 Authorization、Cookie、account ID、originator 和
User-Agent 均由代理安全重建。
xAI 是 Grok wire 与 Responses wire 之间的协议转换层，转换只在 xAI Provider 内完成。
上游结构化错误的 message/code/type 会透传给客户端，其中内嵌的账号指纹 UUID 已脱敏。模型映射是
全局精确映射，未命中时模型名原样交给候选 Provider；分组只限定账号集合，不参与模型改名。

### Cyber 会话自动屏蔽

启用 cyber 会话自动屏蔽后，Responses 请求按 `Client Key + 语义会话`查询 Redis 屏蔽状态。
会话匹配优先使用请求中经校验的显式会话 ID；缺失时才使用完整历史的精确前缀/续接。会话键不包含
账号、模型或通道，也不保存请求正文。

Provider 先读取顶层 `error.code`；该值缺失或去除首尾空白后为空时，才回退读取 `response.error.code`。
顶层值非空时不读取嵌套值；比较前去除首尾空白并按大小写不敏感匹配，规范化后精确等于
`cyber_policy` 才记录拒绝事实，不扫描 `message` 或正文文本。命中未过期的本地条目时，HTTP JSON 与 HTTP SSE 请求在访问上游前
返回 `403`，错误字段为 `error.type = "permission_error"`、
`error.code = "session_blocked_by_cyber_policy"`；已建立的 Responses WebSocket 则在对应的每个
`response.create` turn 返回 `status: 403` 的错误事件，并使用相同字段。此类本地重试仍不访问上游。

屏蔽条目只存在于 Redis 的可过期 session marker 中：首次写入确定 TTL，后续写入或本地重试都不会
续期；设置编辑也不改变已存条目的剩余 TTL。关闭开关时忽略现有条目，新请求使用新发布的运行策略。

## 4. 管理员认证

| 方法 | 路由 | 请求 | 说明 |
| --- | --- | --- | --- |
| `POST` | `/api/admin/auth/login` | `{ username?, password }` | 创建管理员会话并设置 Cookie |
| `GET` | `/api/admin/auth/status` | 无 | 返回当前 Cookie 是否已认证 |
| `POST` | `/api/admin/auth/logout` | 无 | 删除当前会话并清除 Cookie |

## 5. 账号

账号 API 使用统一路由，不存在 Provider Instance 或 Provider 专属账号路由。需要 Provider 的请求只接受
`provider: "openai" | "xai"`。

| 方法 | 路由 | 主要 query/body | 说明 |
| --- | --- | --- | --- |
| `GET` | `/api/admin/accounts` | `page`、`pageSize`、`provider`、`groupId`、`search`、`status`、排序字段 | 分页查询账号与汇总 |
| `GET` | `/api/admin/accounts/detail` | `accountId` | 查询账号详情、额度和本地用量 |
| `GET` | `/api/admin/accounts/turn-state` | `accountId` | 显式读取 OpenAI 账号最近真实上游 Turn State 与手动覆盖明文 |
| `POST` | `/api/admin/accounts/turn-state/update` | `{ accountId, enabled, value?, expectedRevision }` | 按账号版本更新出站覆盖 |
| `POST` | `/api/admin/accounts/turn-state/use-observed` | `{ accountId, observationId, enabled, expectedRevision }` | 仅在观测 ID 仍有效时复制最近上游值 |
| `GET` | `/api/admin/accounts/turn-state/model` | `accountId`、`model` | 读取账号身份代次与 effective model 对应的 Responses Turn State 锁、捕获配置和当前任务 |
| `POST` | `/api/admin/accounts/turn-state/model/update` | 模型值操作及三项读取 fence | 以配置 revision、身份代次和 effective model 做 CAS，手动替换、清除、失效或导入旧账号值 |
| `POST` | `/api/admin/accounts/turn-state/model/capture` | `{ accountId, model, expectedRevision, expectedIdentityRevision, expectedEffectiveModel }` | 将一次隔离捕获加入有界队列，返回 `202` 与 `{ jobId, status }` |
| `GET` | `/api/admin/accounts/turn-state/model/capture` | `jobId` | 查询捕获状态、尝试次数、原因和时间戳 |
| `POST` | `/api/admin/accounts/turn-state/model/capture/cancel` | `{ jobId }` | 取消排队或执行中的捕获并返回完整任务 |
| `GET` | `/api/admin/accounts/export` | `accountIds`、`confirm=export_sensitive_accounts` | 显式导出最多 200 个账号的敏感 Provider 文档 |
| `POST` | `/api/admin/accounts/import` | `{ provider, data, settings?, outboundProxyId? }` | 导入或按上游身份更新账号，可同时应用调度、分组设置与默认代理 |
| `POST` | `/api/admin/accounts/refresh` | `{ accountId }` | 手工刷新 OAuth credential（`idToken` / `accessToken` / `refreshToken`），不刷新额度 |
| `POST` | `/api/admin/accounts/recover` | `{ accountId }` | 管理员显式清除该账号的本地错误/额度/cooldown 事实并重新启用，不访问上游 |
| `POST` | `/api/admin/accounts/rotate` | OpenAI rotation 字段 | 手工替换 OpenAI OAuth token |
| `POST` | `/api/admin/accounts/update` | `{ accountId, enabled, concurrencyLimit, weight, groupIds, outboundProxyId?, outboundProxyUrl? }` | 一次更新账号调度状态、并发上限（`null` 表示继承运行参数）、权重（1–100）、所属分组与出站代理 |
| `POST` | `/api/admin/accounts/batch-update` | `{ accountIds, enabled, concurrencyLimit, weight, groupIds, outboundProxyId?, outboundProxyUrl? }` | 一次事务统一更新所选账号的调度字段、完整分组集合与可选代理 |
| `POST` | `/api/admin/accounts/delete` | `{ provider, accountIds }` | 批量删除 1–200 个账号 |
| `GET` | `/api/admin/accounts/quota` | `accountId` | 读取当前额度，不强制访问上游 |
| `POST` | `/api/admin/accounts/quota/refresh` | `{ accountId }` | 访问 Provider 并刷新额度，同时同步额度所属状态 |
| `GET` | `/api/admin/accounts/profile-statistics` | `accountId` | 实时查询 OpenAI/Codex 官方个人资料中的累计活动与使用洞察 |
| `GET` | `/api/admin/accounts/reset-credits` | `accountId` | 查询 OpenAI 上游主动额度重置卡，不读取本地库存 |
| `POST` | `/api/admin/accounts/reset-credits` | `{ accountId, creditId?, redeemRequestId }` | 使用 UUIDv4 幂等键消费一张 OpenAI 上游重置卡 |
| `GET` | `/api/admin/accounts/models` | `accountId` | 优先读取该 Provider + 套餐的模型 cache，缺失时有限实时拉取 |
| `POST` | `/api/admin/accounts/models/refresh` | `{ accountId }` | 强制拉取最新模型并覆盖 cache |
| `GET` | `/api/admin/accounts/connection-test` | `accountId`、`modelId` | 通过 SSE 返回实时连接测试事件，不作为业务 Responses 用量记录 |
| `POST` | `/api/admin/accounts/oauth/start` | `{ provider, name, accountId?, outboundProxyId?, outboundProxyUrl? }` | 创建 OpenAI 或 xAI OAuth flow；`accountId` 表示重新授权 |
| `POST` | `/api/admin/accounts/oauth/complete` | `{ provider, flowId, callbackUrl, settings? }` | 消费 OAuth callback；首次授权可附带账号设置，重新授权保留原设置 |

模型 Turn State 统一兼容上游 HTTP/SSE 与 WebSocket。手动值支持 1–16384 个可打印 ASCII 字节；普通观测和自动捕获
仍只接纳正好 292 字节的可打印 ASCII。官方客户端把该值用于
单 turn sticky routing；跨 turn 模型锁是本网关的实验性本地策略，不代表上游提供了同等持久性保证。数据面优先级为
未过本地复用窗口的 active、正常 continuation；已过期或被拒绝的托管值不会从 provider session state
回流，无关的显式客户端 continuation 仍保留。关闭模型锁时，同一 turn 已携带的托管 continuation 也保留；
重新开启锁后，没有可用模型 pin 时才抑制该托管 fallback。旧版账号覆盖不再作为 runtime fallback，只能在
选择模型后显式导入。`reuseWindowSeconds` 默认 7200 秒；candidate 晋升不重置自己的 deadline。
账号策略的 `captureTriggerMode` 支持 `on_attributed_failure`（默认）、`before_expiry_if_used`、
`first_request_after_expiry` 与 `failure_or_first_after_expiry`。模式控制提前或到期首请求等额外来源；任何模式下
已经确认的实际发送值失效都会在自动捕获开启时补值。只有提前捕获模式使用 `refreshLeadSeconds`，且 active
从未在最终 HTTP/WS 发送边界实际使用时不会探测。
`missingStateAction` 支持 `natural_then_capture`（默认）和 `capture_first`。该字段只在
`lockEnabled` 与 `captureEnabled` 同时开启时控制数据面；只开捕获仍允许后台或手动收集，普通业务保持
fail-open。`capture_first` 先等待现有有界捕获任务成功，再发送一次业务请求；失败或超时不发送业务。
`natural_then_capture` 先发送一个全新无托管 state 的自然请求：收到合格 292 值时直接使用该响应；首次
业务交付前明确收到非 292 值，或直到终态/错误仍无值时，先取消并释放源流，再等待捕获成功后最多重发
一次。业务已经交付后不透明重放。

模型状态响应包含实际映射后的 `effectiveModel`、独立的 `identityRevision`、模型配置
`configRevision`、`pin`、`candidate`、`nextCaptureAt`、`nextActivationAt`、`captureNotBefore`、
`waitingReason`、脱敏的 `captureProxy`、可显式导入的 `legacyOverride` 和当前 `capture` 任务。
`pin`/`candidate` 通过 `compatibleTransports` 明确列出 `http` 与 `websocket`，并返回匹配当前版本的
`sentCount`、`lastSentAt`。
可解码的 Fernet envelope 还展示 `encodedBytes`、`rawBytes`/`decodedBytes`、`ciphertextBytes`、
`tokenVersion`、`envelopeFormat`、`issuedAt` 与 `timestampVerified: false`。`issuedAt` 是未验证签名的
envelope timestamp，不是 expiry；长度或多一个 16 字节 ciphertext block 只作为分类事实，不作为质量判断。
模型更新和手动捕获必须原样回传同一次 GET 得到的 `configRevision`、`identityRevision` 与
`effectiveModel`；任一值已变化都返回 `40901`，防止模型映射或账号身份切换后把旧页面操作写入新 scope。
保留已有值并缩短 `reuseWindowSeconds` 时只收紧 deadline，不刷新 `capturedAt`；放大窗口也不延长既有
deadline。`attemptTimeoutSeconds` 范围为 1–60，`jobTimeoutSeconds` 范围为 1–300，且前者不得大于后者。
普通 access-token refresh 只推进 credential revision，不改变账号身份代次；真实身份替换才隔离旧模型锁。
EMPTY/AGED/INVALID 都表示当前没有可用托管值；已保存的过期值只保留审计，不阻止重新学习或捕获。
实际 HTTP 响应头或
WebSocket metadata 事件返回 292 字节可打印 ASCII 值时，
按 account identity、effective model 和配置 revision 直接保存为 `observation` pin；只有 encoded byte length
明确不等于 292 时立即取消尚未交付的源流并登记捕获。已过期或已拒绝值的同值普通观测不会重置
`capturedAt`、deadline 或使其复活；正好 292 字节但不可打印的值只保留为 suspect，不保存模型锁也不消耗
住宅代理。HTTP headers 缺少该字段不表示失败，因为 SSE/WS metadata 可能稍后返回；只有实际发送完成后在
terminal、错误或超时边界仍没有值，才按 generation/candidate ID fence 退役该次实际发送值。
请求先尝试 WebSocket、后在发送 payload 前回退到 HTTP 时，按实际 HTTP 响应执行同一观察状态机。自动任务
使用选定且最近 24 小时测试成功的已管理代理；每次尝试
创建独立 HTTP 连接，只在成功 HTTP 状态后接纳 Turn State header；收到首个合格 header 或 SSE event 后立即释放剩余响应。
非 292 字节候选与其他可重试失败使用同一退避。任务在取得 commit guard 后最后检查 deadline 和取消；
检查通过并开始 Store commit 后进入不可取消区，必须等待数据库结果与进程内 pin 发布完成。此后到达的
取消请求属于 best-effort，job deadline 也不丢弃已开始的 commit；提交成功时任务最终返回 `succeeded`。
捕获不创建模型请求、用量、额度、限流、账号健康、circuit、feedback 或账号最近观测。仍可用的同值捕获
以 `succeeded / unchanged_value` 结束当前任务，不续期也不执行下一次付费尝试；过期或已拒绝的同值才进入
剩余 attempt。一次触发耗尽后清除请求信号并进入冷却，没有新的业务发送或失败信号时不会自动永动。
Responses HTTP/WS 请求确实注入当前模型锁后，明确非 292 返回、实际发送后的终态无值，以及结构化错误的
`param`/`target` 明确指向 `x-codex-turn-state`，都会按 pin fingerprint、generation 与 candidate ID CAS
退役实际发送值；迟到的 A 反馈不会淘汰已切换的 B。有可用 candidate 时原子晋升且不续期；没有 candidate
时按账号开关进入捕获。发送前网络失败、通用错误文本或无法确认是否发送的失败不据此退役模型锁。

账号列表支持以下稳定值：

- `provider`: `all`、`openai`、`xai`；
- `groupId`: 分组 ID、`ungrouped`，或省略以不过滤；
- `status`: `normal`、`quota_exhausted`、`rate_limited`、`disabled`、`error`；
- `sortBy`: `email`、`status`、`planType`、`usage`、`lastUsedAt`、`expiresAt`；
- `sortDirection`: `asc`、`desc`。

账号视图和 Dashboard 账号概览中的 `planType` 保留原始套餐值；`planTypeDisplay` 由后端先按 Provider 解析名称，
再统一为大驼峰格式，前端直接展示该字段，例如 `Free`、`SuperGrokPro`、`EduPlus`。
OpenAI 的 `self_serve_business_prolite` 等 Team 套餐显示为 `Business`；新套餐也使用相同格式。
账号套餐为空或 `unknown` 时，后端优先用已保存的上游额度响应
中的明确套餐值补全 `planType` 和 `planTypeDisplay`；两处均无套餐信息时才显示“未知套餐”。

`outboundProxyId` 绑定已保存且最近测试成功的代理；省略或 `null` 保留当前绑定，空字符串清除绑定。
`outboundProxyUrl` 兼容 HTTP、HTTPS、SOCKS5、SOCKS5H 代理 URL，可带用户名和密码；不能与 ID 同时设置。
编辑时省略或 `null` 表示保持原配置，空字符串表示清除代理并直连。列表和详情只返回
不含认证信息的 `outboundProxyEndpoint`（直连时为 `null`）；只有显式敏感导出包含完整 URL。
指定代理后，推理、OAuth 服务端交换/刷新及账号辅助请求使用同一出口；代理失败不会退回直连。
浏览器打开的第三方 OAuth 授权页仍使用浏览器自身网络。
账号出口与连接隔离见 [架构说明](architecture.md#账号出站代理)。

### OpenAI 账号 Turn State

三条接口只支持已有 OpenAI 账号，使用管理员鉴权及 `Cache-Control: no-store`；账号最近观测的
明文在这组显式接口返回，请求所属观测的明文只在已认证的请求详情接口返回。账号列表、普通请求
日志、请求列表、汇总和审计不包含该值。成功响应使用普通管理信封，`data` 形状如下：

```json
{
  "accountId": "acct_...",
  "observed": {
    "id": "observation-id", "value": "upstream-value", "bytes": 14,
    "sha256": "lowercase-hex-sha256", "observedAt": "2026-09-16T12:00:00Z",
    "transport": "http", "upstreamResponseId": null, "clientTurnId": null
  },
  "override": {
    "enabled": false, "value": null, "bytes": 0, "sha256": null, "updatedAt": null
  },
  "configRevision": 1
}
```

`observed` 未采集时为 `null`；`transport` 为 `http` 或 `websocket`。`bytes` 是 UTF-8
字节数，`sha256` 为原值的十六进制 SHA-256；可得的上游 response ID 与客户端 turn ID 随观测保存。
`configRevision` 是该账号覆盖配置的版本，从 1 开始，与全局运行设置 revision 独立。三个接口均返回
同形 `data`，写入必须提供当前 `expectedRevision`，否则返回 `40901`。

`update` 中省略 `value` 保留已保存的覆盖值；显式 `null` 清空值并关闭覆盖，空字符串不能启用。
值最大 16384 bytes，且只能包含可打印 ASCII（`0x20`–`0x7e`），保证 HTTP header 与 WebSocket
投影使用同一个无损值；换行、控制字符和非 ASCII 值返回参数错误。`use-observed` 复制当前最近一次
上游观测并执行相同校验；新观测已替换请求的
`observationId` 时返回 `40901`，须重新查询并确认。该操作不会把手动覆盖值写成上游观测。
这些旧接口保留已有值、观测和版本合同，供升级后在统一模型窗口中选择 effective model 并显式导入；
Provider 不再把账号级值作为 HTTP 或 WebSocket 的运行时 fallback。成功、失败和流式传输中实际收到的
上游值仍尽力写入最近观测；观测持久化失败不改变客户端响应。仅数据库及备份保存明文，按账号凭据保护。

### 独立代理管理 / Managed Proxies

所有端点要求管理员身份。所有响应只返回去掉认证信息的 `endpoint`，不会返回完整 URL。
All endpoints require admin authentication and redact proxy credentials from responses.

| 方法 / Method | 路径 / Path | 请求 / Request | 结果 / Result |
| --- | --- | --- | --- |
| `GET` | `/api/admin/proxies` | `page`、`pageSize`（1-200）、`search`（名称） | `{ items, page }` |
| `GET` | `/api/admin/proxies/accounts` | `proxyId`、`page`、`pageSize`（1-200）、`search`（账号名称或邮箱） | `{ items, page }` |
| `POST` | `/api/admin/proxies/accounts/remove` | `{ proxyId, accountId }` | `{ configRevision }` |
| `POST` | `/api/admin/proxies/create` | `{ name, proxyUrl }` | `201 { record, configRevision }` |
| `POST` | `/api/admin/proxies/update` | `{ id, revision, name, proxyUrl? }` | `{ record, configRevision }` |
| `POST` | `/api/admin/proxies/test` | `{ id, revision }` | 最新代理记录 / Proxy record with test result |
| `POST` | `/api/admin/proxies/delete` | `{ id, revision }` | `{ configRevision }` |

`record` 包含 `id`、`name`、`endpoint`、`hasAuthentication`、`revision`、`accountCount`、
`lastTestAt`、`lastTest: { success, latencyMs, exitIp, message }`、`createdAt`、`updatedAt`。
未测试时 `lastTestAt` / `lastTest` 为 `null`。连通性失败返回 HTTP 200 和 `lastTest.success=false`；
记录版本过期、重复 URL、删除已绑定的代理返回 409，并发测试满载返回 429。

代理列表只返回关联账号数量。关联账号按需查询，每项包含 `id`、`name`、`email`、`provider`、`enabled`、
`authenticationKind`、`planType`、`planTypeDisplay` 和 `groups: [{ id, name, color, enabled }]`，
不返回账号凭据。默认每页 20 条，按名称、ID 稳定排序；搜索不区分大小写，匹配名称或邮箱的字面子串。
不存在的代理返回 404，未绑定账号或没有匹配结果时返回空页。数量与当前页来自同一个数据库只读快照。

移除关联账号只清除指定账号的代理绑定与连接地址，使其改为直连，保留凭据、调度参数与分组。
若账号已不再绑定请求中的代理，则返回 409；成功后在同一事务中更新配置版本与审计，并发布运行时快照。

更新省略 `proxyUrl` 保留认证；连接配置改变时清除测试结果并更新所有绑定账号。
Omit `proxyUrl` to preserve credentials. Connection changes invalidate the previous test and update all bound accounts.
Tests persist only when the requested revision still matches. Connectivity failures use HTTP 200 with
`lastTest.success=false`; stale revisions, duplicate URLs and deleting an in-use proxy return 409.
The test concurrency limit returns 429.

测试固定经代理访问 `https://api.ipify.org?format=json`，超时 15 秒，每进程最多同时测试 4 条。
探测器复用 OpenAI 的证书信任配置：优先读取非空的 `CODEX_CA_CERTIFICATE`，
其次读取 `SSL_CERT_FILE`，并保留系统根证书；证书配置错误不会回退为不验证证书。
出口测试通过不表示 Provider 账号权限或额度可用；账号可用性使用账号连接测试。
导入请求可以携带顶层 `outboundProxyId`，在令牌交换前解析为默认出口；文件中显式的代理配置优先。
文件及 AT/RT 导入从凭据交换到落库期间保护所选代理；此时修改、删除或写入测试结果返回 409，
避免已轮换的凭据因代理状态变化而丢失。完成导入或请求取消后自动释放保护。
OAuth 等待回调期间不持有保护；提交仍拒绝已删除、连接配置改变或测试失败的代理。

Tests reach `https://api.ipify.org?format=json` through the configured proxy, with a 15-second timeout
and four concurrent tests per process. Provider access still requires the account connection test.
Imports accept a top-level `outboundProxyId` as the default exit before token exchange; explicit per-account
settings in the document take precedence. Credential imports reserve their selected proxy until commit;
concurrent proxy mutations return 409. OAuth commits still reject a deleted, changed or failed proxy.

### 账号连接测试 SSE

`GET /api/admin/accounts/connection-test` 固定探测请求指定的账号，不参与普通账号轮换。成功流沿用
`test_start`、`request`、`content`、`test_complete` 事件；失败事件为：

```json
{
  "type": "error",
  "source": "upstream",
  "gatewayErrorCode": "rate_limited",
  "sendState": "sent",
  "error": "upstream unavailable",
  "providerErrorCode": "usage_exhausted",
  "providerErrorType": "invalid_request_error",
  "upstreamStatus": 429,
  "upstreamContentType": "application/json",
  "upstreamBody": "{\"error\":{...}}"
}
```

账号处于过载冷却时，连接测试也会在发送上游请求前被拒绝，需等待冷却到期或显式恢复账号。

- `source` 为 `gateway`、`provider` 或 `upstream`：分别表示尚未进入 Provider、Provider 本地且未发送、
  已发送/可能已发送或已经捕获到上游事实。
- `gatewayErrorCode` 是 `GatewayErrorKind` 的稳定机器值，管理端据此生成中文摘要。
- `sendState` 为 `not_sent`、`sent`、`ambiguous`，非 Provider 错误为 `null`。
- `error`、`providerErrorCode`、`providerErrorType`、`upstreamStatus`、`upstreamContentType` 和
  `upstreamBody` 是实际捕获的原始诊断字段；缺失时为 `null`，不会由本地猜测或翻译。

导入的 `data` 必须是 JSON object，Admin API 请求上限为 64 MiB；Provider 可以收紧限制，
当前 xAI 导入上限为 16 MiB。内部 schema 由目标 Provider 独占解释：

- OpenAI 接受单账号 OAuth 文档、`accounts` 数组（最多 200 项）、CPR 账号 bundle 和含代理引用的 sub2api 导出；
- OpenAI OAuth token 字段接受 `accessToken`、`refreshToken`、`idToken`，以及官方
  `auth.json` 中的 `access_token`、`refresh_token`、`id_token`，可以嵌套在 `tokens` 等账号 object 内；
  每项至少包含 AT 或 RT。仅含 `OPENAI_API_KEY` 的客户端代理配置不是 OAuth 账号导入材料；
  RT-only 会在导入时换取 AT，AT-only 不具备自动续期能力；
- OpenAI 与 xAI 的账号条目接受 `outboundProxyUrl`；OpenAI 还会解析 sub2api 的 `proxy_key` 和顶层 `proxies`。
  代理在 token 刷新前绑定。缺失、重复、停用、带到期时间或配置回退策略的 sub2api 代理会拒绝导入；
- xAI 从单账号 object 或 `accounts` 数组中提取 OAuth token；并发、优先级等字段不参与认证；
- xAI 批量导入逐条独立校验：失败条目跳过并记录日志，不中断其余条目，仅当没有任何条目成功时整个导入才报错；
- xAI API Key 不是受支持的账号 credential；
- 导入不会只凭文件外形写入账号；目标 Provider 使用认证材料完成必要的 token exchange 或已认证账号资料补全。

管理端的 OpenAI `AT` / `RT` 标签是同一导入 API 的输入便利层：每行一个 token，最多 200 行，提交前
转换为对应的 `accounts` JSON。Admin API 本身不接收纯文本 token 列表。例如：

```json
{
  "provider": "openai",
  "data": {
    "accounts": [
      { "accessToken": "eyJ..." },
      { "accessToken": "eyJ...", "refreshToken": "rt_...", "idToken": "eyJ..." }
    ]
  }
}
```

RT-only 使用同一形状，只提交 `refreshToken`。不得把真实 token 写入日志、issue、fixture 或文档。

账号导入与首次 OAuth complete 可附带 `settings: { enabled, concurrencyLimit, weight, groupIds }`。
提供 `settings` 时四项均必填，`concurrencyLimit: null` 继承运行参数，否则为 1–4294967295 的整数；
`weight` 为 1–100，`groupIds` 为完整分组集合。设置应用于本次导入的全部账号，包括匹配到的已有账号，
与凭据在同一事务内提交；分组不存在时整次回滚。省略 `settings` 时新账号使用默认设置并保持未分组，
已有账号保留原有分组、权重与并发设置。重新授权不接受 `settings`，普通 credential refresh/rotation 也保留账号设置。

管理端先配置账号设置，再选择 OAuth、AT/RT 或账号文件完成导入。返回设置保留输入；更改出站配置会使
旧 OAuth 链接失效。文件中显式的出站配置优先于表单代理，未指定时使用表单代理。
账号列表的每个 item 返回轻量 `groups: [{ id, name, enabled }]`。

OpenAI 的 CPR 导出保持 OAuth 账号的既有 token 与过期时间字段。

OpenAI rotation 请求字段为：

```json
{
  "provider": "openai",
  "accountId": "acct_...",
  "idToken": "...",
  "accessToken": "...",
  "refreshToken": "..."
}
```

OAuth start 使用：

```json
{
  "provider": "openai",
  "name": "account name",
  "accountId": null
}
```

重新授权已有账号时，start 请求仍携带 `provider` 和展示用 `name`，只额外提供目标 `accountId`；
客户端不得提交 `credentialRevision`、旧 token 身份或其他并发控制字段。complete 请求也不重复提交
`accountId`，后端通过 `flowId` 中保存的目标绑定完成授权。

### OpenAI 身份、额度与状态

- OAuth 文件导入接受 camelCase 与 snake_case 的三个 token 字段，内部统一保存为
  `accessToken`、`refreshToken`、`idToken`，不接受含义模糊的 `token`。
  仅有 refresh token 时先换取 access token。
- 普通 OAuth 的身份补全复用官方 `token_data.rs::parse_chatgpt_jwt_claims`：优先解析 `idToken`，缺失字段再由
  `accessToken` 补齐；`email` 优先 JWT 顶层值、其次 `https://api.openai.com/profile.email`，用户 ID
  优先 `chatgpt_user_id`、其次 `user_id`。该路径不调用 `whoami`，也不信任导入文档顶层的
  `userId/accountId`。
- `at-` 开头的 Codex Personal Access Token（PAT）可直接粘贴到现有 **AT 导入** 入口，或使用
  `{"accessToken":"at-..."}` JSON；也接受官方 `auth.json` 的 `personal_access_token` 字段
  （兼容 `personalAccessToken`）。导入时向 OpenAI auth 的
  `/api/accounts/v1/user-auth-credential/whoami` 验证令牌，以响应中的用户 ID、账号 ID 和套餐建立身份，
  `email` 可缺失；不从导入文件的身份字段或附带的 ID token 回退补齐。验证失败不导入，接口区分提示
  PAT 格式无效、被上游拒绝、验证服务不可用和身份响应无效，不回显令牌或原始上游响应。
  PAT 不保存附带的 refresh token、ID token 或推测的过期时间，不参加 OAuth RT 刷新；
  失效后需取得新 PAT 再导入。普通 JWT 导入行为不变。
- 首次 OAuth 保留回调 `state`、PKCE 与官方 token exchange，并持久化 `idToken`、`accessToken`、
  `refreshToken`。刷新响应中的三个 token 字段均按官方语义独立轮换：返回新值时替换，省略时分别保留
  现值。重新授权也保留这些回调保护，但只轮换目标账号的 token。回调地址只承载 `code`/`state`，
  不以 host/path 形式作为拒绝条件。
- 账号文件导入和 OAuth complete（包括重新授权）在 credential 提交后后台尝试一次额度观测，不等待
  观测完成才返回成功。观测失败只记录告警，不回滚已提交的账号；手工或后台 RT 刷新只更新 token，
  不隐式等同于手工额度刷新，也不更新既有账号资料或 OAuth principal。xAI 导入与 OAuth complete
  使用相同的提交后观察流程。
- OAuth pending flow 先取得带过期时间的独占 claim，只有账号事务提交成功后才消费。失败会释放 claim，
  但上游 authorization code 本身通常只能交换一次；已完成过 token exchange 时应重新创建 OAuth flow。
- `GET /accounts/quota` 只读取最后一次落库快照；`POST /accounts/quota/refresh` 才访问上游。access token
  已过期时，额度刷新要求先走 credential 刷新或重新授权，不会拿过期 token 探测额度。
- OpenAI 已耗尽账号每 30 分钟主动复核一次，也会在最早未恢复窗口的 `resetAt + 2 分钟` 到期后
  提前复核。后台每 30 秒检查触发条件；同一重置边界复核后仍未恢复时回到 30 分钟重试，
  避免旧 reset 持续触发请求。各窗口独立确认恢复，时间到期本身不会直接解除账号耗尽。
- `POST /accounts/recover` 是管理员对本地事实的强制恢复：它清除 Redis cooldown 和已保存的额度/错误，
  把账号重新启用并恢复为可调度 credential；它不验证上游账号是否已经恢复，下一次真实请求仍可重新写入
  失败事实。
- 成功额度观测会 revision-fenced 写入 quota；明确 `Allowed` 投影为 `normal`，明确耗尽投影为
  `quota_exhausted`。额度观测不会清除凭据过期、无效或封禁事实；这些事实统一投影为 `error`，并由
  `errorReason` 区分。额度接口的 401/403 也不足以判定 refresh token 永久失效，credential 终态只由
  OAuth refresh 的明确永久错误写入。
- 正常 Responses 请求会解析上游响应的 rate-limit headers，合并进同一 quota 快照并同步状态。Free、
  K12 等套餐共用该状态机；套餐只参与账号展示和按套餐隔离的模型目录 cache，不存在 K12 专属额度路径。
- 账号展开区的 Token 结构、模型排行和列表 Token 汇总优先使用账号级周额度窗口，无可统计的周窗口时
  使用月额度窗口；`usage.windowLabelDisplay` 随选中的窗口返回“周额度窗口”或“月额度窗口”。查询边界
  严格为 `[resetAt - windowSeconds, resetAt)`，不是自然周/月或最近 7/30 天；额度刷新若返回了更早的
  重置时间，会按新边界重新聚合。没有边界完整、可归属到账号的周/月窗口时显示无数据，标签为
  “周/月额度窗口”，不回退到 5 小时、日窗口或历史累计。各额度条与 Dashboard 的百分比选择不受影响。
  金额原值保持完整精度，USD 展示值
  小于 1 美元时最多保留四位小数，其余保留两位。
- 账号页没有定时静默轮询。手工额度刷新只替换响应中的账号行并同步状态汇总，不触发整页 loading；若
  新状态不符合当前筛选，该行从当前页移除。请求驱动或后台任务产生的状态变化，需要下一次显式查询账号
  列表后才会显示。

### OpenAI 官方个人资料统计

`GET /api/admin/accounts/profile-statistics?accountId=...` 仅支持 OpenAI/Codex OAuth 账号。每次查询直接
访问官方个人资料端点，不读取本地 usage/billing 记录，也不缓存或估算统计结果。响应 `data` 包含：

- `displayName`、`username`、`imageUrl`：官方账号资料；
- `summary`：累计文本 Token、单日峰值 Token、最长任务时长、当前连续天数和最长连续天数；
- `dailyUsage`：按日期返回的 Token 活动；
- `activityInsights`：快速模式占比、上游原样返回的推理强度及占比、Skill 探索/使用数、聊天总数，
  以及插件与 Skill 调用排行。

官方未返回的字段保持 `null`，不使用本地数据补齐；`hasStatsError: true` 表示账号资料可用，但官方统计
部分不可用。access token 已过期或官方返回 401 时，接口要求先刷新 credential 或重新授权。原账号级
`GET /api/admin/accounts/usage-statistics` usage/billing 报表接口及其查询链路已移除。

### OpenAI 主动额度重置卡

`GET /api/admin/accounts/reset-credits?accountId=...` 每次都查询 OpenAI 上游；后端不把卡片列表写入
PostgreSQL 或 Redis。管理端只在用户打开弹窗或点击刷新时调用，并在当前浏览器会话内缓存最近一次成功
结果，用于账号行上的 `xN` 提示。

查询响应：

```json
{
  "availableCount": 1,
  "credits": [{
    "id": "credit_...",
    "status": "available",
    "title": "...",
    "expiresAt": "2026-08-31T12:00:00Z",
    "resetType": "..."
  }]
}
```

消费请求的 `redeemRequestId` 必须是小写、带连字符的 canonical UUIDv4；`creditId` 可省略，由上游选择
可用卡。一次请求发出后若传输结果不明确，重试必须复用完全相同的 `redeemRequestId`、`creditId` 和
账号。服务在单副本进程内按账号串行消费，并在 credential 需要刷新时以同一命令重试一次；它不会对不明
结果自动创建新消费。

若服务无法确认不可逆消费是否完成，返回 HTTP `502` / 业务码 `50202`；客户端应先刷新卡片与额度状态，
并在确需重试时复用原 `redeemRequestId`。明确的上游 HTTP 拒绝仍使用 `50201`，不会误标为结果未知。

```json
{
  "accountId": "acct_...",
  "creditId": "credit_...",
  "redeemRequestId": "8fbf302d-11df-4bd5-82e4-08e4b3df7874"
}
```

消费响应只返回上游结果 `code` 和可选 `credit`。消费端确认成功后应重新 GET 卡片列表，并显式调用
`POST /api/admin/accounts/quota/refresh` 回读官方额度；不得直接改写本地 `resetAt`。xAI 不支持该能力。

## 6. 账号分组

分组是 Provider-neutral 的账号集合；一个组可包含任意 Provider 账号，一个账号也可属于多个组。

| 方法 | 路由 | 主要 query/body | 说明 |
| --- | --- | --- | --- |
| `GET` | `/api/admin/account-groups` | `page`、`pageSize`、`search`、`enabled` | 分页查询分组；返回账号可用性、并发槽位（Redis 不可用时 `usedSlots=null`）及成功请求 USD 用量 |
| `POST` | `/api/admin/account-groups/create` | `{ name, description, color }` | 创建空分组；`color` 严格为 `#RRGGBBAA`，返回时统一大写 |
| `POST` | `/api/admin/account-groups/update` | `{ id, name, description, color }` | 更新名称、描述和颜色 |
| `POST` | `/api/admin/account-groups/enable` | `{ id }` | 启用 |
| `POST` | `/api/admin/account-groups/disable` | `{ id }` | 禁用；已绑定 Key 保持受限，不回退到全部账号 |
| `POST` | `/api/admin/account-groups/delete` | `{ id }` | 删除未被 Client Key 引用的组 |

列表数据为 `{ items, page, configRevision }`，其中 item 返回 `memberCount`、按 Provider 聚合的
`providerCounts` 和 `clientKeyCount`。查询分组成员使用账号列表的 `groupId` 筛选，
不提供独立的分组成员路由；账号的 Provider 不代表整个分组的 Provider。

## 7. Client Key

| 方法 | 路由 | 主要 query/body | 说明 |
| --- | --- | --- | --- |
| `GET` | `/api/admin/client-keys` | `cursor`、`limit`、`search`、`sortBy`、`sortDirection` | 游标分页查询 |
| `POST` | `/api/admin/client-keys/create` | 创建字段 | 创建带账号范围的 Client Key |
| `GET` | `/api/admin/client-keys/reveal` | `id` | 显式读取完整明文 Key |
| `POST` | `/api/admin/client-keys/update` | 更新字段 | 原子更新名称、分组范围和限额 |
| `POST` | `/api/admin/client-keys/enable` | `{ id }` | 启用 |
| `POST` | `/api/admin/client-keys/disable` | `{ id }` | 禁用 |
| `POST` | `/api/admin/client-keys/delete` | `{ id }` | 删除 |

创建字段为 `name`、可选 `label`、`groupIds`、`maxConcurrency`、`requestsPerMinute`、可选
`dailyLimitUsd` 和 `weeklyLimitUsd`，更新请求再增加
`id`。`groupIds` 必须显式提交：空数组派生 `routingScope: "all"`，非空数组派生
`routingScope: "groups"`。响应同时返回分组引用 `groups`，以及从当前有效账号池派生、仅供展示的
`providerKinds`；Client Key 不再保存 `providerKind`。创建和 reveal 响应会返回完整明文 Key，调用方
必须立即安全保存。

金额字段为非负十进制字符串，最多 10 位整数与 10 位小数，`"0"` 表示不限额。
创建时省略金额字段默认为零；更新时省略或 `null` 保留当前值，修改限额不会清空已用金额。
`maxConcurrency` 和 `requestsPerMinute` 是非负整数，零表示不限。

列表增加 `dailyLimitUsd`、`weeklyLimitUsd`、`dailyUsedUsd`、`weeklyUsedUsd`（均为字符串）、
`dailyResetsAt`、`weeklyResetsAt`（RFC3339 或 `null`）。
管理端日／周金额显示两位小数，悬停可查看原始值；记账和限额比较保留完整精度。
日窗口按北京时间零点重置；周窗口从首次准入当天零点起持续七天，到期后在下一次使用时重新开启。
费用按请求完成时间归属窗口。并发按同一 Key 的执行中请求累计，包含 SSE 与每个 WebSocket
`response.create`；空闲连接不占名额，内部重试不重复占用。
修改 Key 策略对既有 WebSocket 连接的下一次请求同样生效，已开始的请求保持原有快照。

任一已结算金额达到限额后拒绝新请求，已准入请求可完成并使金额超过阈值。
HTTP 返回 `429`，`error.code` 为 `key_daily_budget_exceeded` 或 `key_weekly_budget_exceeded`，
并附 `Retry-After`；WebSocket 每次 `response.create` 执行相同检查并返回协议错误事件。
只累计上游上报或按用量与模型价格计算出的 USD 费用；无法取得费用的尝试按零累计，
保留错误和用量诊断，不产生待核账记录或阻断。内部重试中已经取得的费用仍会累计。
预算存储不可用时返回 `503`、`key_budget_unavailable`。

自动结算按网关请求 ID 幂等执行。账本独立于使用统计日志，记录保留至删除 Key，
不受 `usageRetentionDays` 影响。

English: Daily and weekly budgets use automatically recorded costs. Missing usage or interrupted requests
do not block a Key, and no manual reconciliation is required. New requests receive `429` once recorded
costs reach the daily or weekly limit; requests already admitted can finish above that threshold.

## 8. 运行设置

| 方法 | 路由 | 说明 |
| --- | --- | --- |
| `GET` | `/api/admin/settings` | 读取运行设置 |
| `POST` | `/api/admin/settings/update` | 原子替换全部运行设置 |
| `GET` | `/api/admin/settings/client-downloads/codex-desktop/windows` | 提取 Codex Desktop Windows 离线安装直链；`refresh=true` 强制刷新进程内短缓存 |
| `GET` | `/api/admin/settings/admin-api-key` | 只返回管理 API Key 是否存在 |
| `POST` | `/api/admin/settings/admin-api-key/delete` | 删除管理 API Key |
| `POST` | `/api/admin/settings/admin-api-key/regenerate` | 重新生成并一次性返回完整管理 API Key |

设置更新字段包括：

```text
modelMappings
refreshMarginSeconds
refreshConcurrency
maxConcurrentPerAccount
requestIntervalMs
rotationStrategy
minCodexDesktopVersion
minCodexCliVersion
usageRetentionDays
opsEventRetentionDays
auditRetentionDays
wsPoolEnabled
wsPoolMaxAgeMs
wsPoolMaxConnecting
wsPoolStreamIdleTimeoutMs
wsPoolFastPathBudgetMs
overloadCooldownEnabled
overloadCooldownThreshold
overloadCooldownSeconds
cyberSessionBlockEnabled
cyberSessionBlockTtlSeconds
openaiUserAgent
openaiRequestBodyOverrideEnabled
openaiRequestTimezone
openaiSearchCountry
```

`rotationStrategy` 可取 `smart`、`quota_reset_priority`、`round_robin`、`sticky`。
两个 `minCodex*Version` 字段为 `string | null`，只设置最低版本，不存在最大版本字段。
`wsPool*` 控制 OpenAI 上游 WebSocket 连接池，更新时五个字段均必填：

| 字段 | 默认值 | 含义 |
| --- | ---: | --- |
| `wsPoolEnabled` | `true` | 是否启用上游连接池 |
| `wsPoolMaxAgeMs` | `3300000` | 连接最大存活时间，单位毫秒；到期后不再复用 |
| `wsPoolMaxConnecting` | `8` | 所有账号合计的并发建连上限，非请求并发数；最大 `4294967295` |
| `wsPoolStreamIdleTimeoutMs` | `300000` | 等待下一条上游业务消息的空闲超时，单位毫秒 |
| `wsPoolFastPathBudgetMs` | `800` | 自动传输请求等待连接就绪的预算，单位毫秒；超时回退 HTTP SSE |

四个数值字段均为正整数，关闭连接池时也需提交有效值。设置在同一事务内保存、推进
`config_revision` 并记录审计；Provider 每 5 秒从数据库对账，无需重启。运行中读取失败时保留
上次已加载的策略；请求开始准备连接时固定预算与空闲超时，后续更新不改变该请求的超时。

关闭连接池后，自动传输的新请求改用 HTTP SSE，已开始的生成继续完成，空闲和归还的连接被关闭。
必须使用 WebSocket 或原连接的续接仍遵守原协议限制，可能因连接池不可用而失败；此设置不关闭
客户端到网关的 WebSocket 接口。

迁移 `0006_ws_pool_runtime_settings.sql` 为已有实例填入表中默认值，不导入旧 `config.yaml` 的
`openai.ws_pool` 自定义值。升级后在管理端核对并保存所需参数。每次启动均在对外服务前恢复数据库
中的策略；配置文件保留兼容解析，但不覆盖已保存的运行设置。旧管理 API 调用方需补齐五个字段，
缺失时返回 `422`，不会用默认值覆盖当前配置。

过载冷号由以下三个必填字段控制，保存后新请求使用更新的调度策略：

| 字段 | 默认值 | 含义 |
| --- | ---: | --- |
| `overloadCooldownEnabled` | `false` | 是否启用 OpenAI 过载冷号 |
| `overloadCooldownThreshold` | `2` | 同一账号连续过载次数，正整数，最大 `4294967295` |
| `overloadCooldownSeconds` | `120` | 冷号时长（秒），正整数，最大 `4294967295` |

上游错误文本包含 `Our servers are currently overloaded. Please try again later.` 或
`Selected model is at capacity.` 即累计一次；覆盖 HTTP 503、HTTP SSE 和 WebSocket 错误。
两个关键词共享同一账号的计数，成功响应或其他错误清零。计数在各进程内独立维护，重启后重新计数；
冷却到期时间保存在共享 Redis。冷号期间拒绝该账号的新推理、指定账号请求和连接测试，已有请求可继续完成，
其成功不会提前解冻。关闭开关停止新触发，已有冷却按原期限结束。账号列表通过 `rate_limited` 展示冷却状态。

Cyber 会话自动屏蔽由以下两个字段控制：

| 字段 | 类型 | 默认值 | 含义 |
| --- | --- | ---: | --- |
| `cyberSessionBlockEnabled` | `boolean` | `false` | 是否启用 Responses 会话屏蔽 |
| `cyberSessionBlockTtlSeconds` | `u32` 整数 | `3600` | 屏蔽条目的 Redis TTL（秒），范围 `1`–`4294967295` |

`GET /api/admin/settings` 及成功的设置更新响应都会返回这两个字段。
`POST /api/admin/settings/update` 仍以原子方式替换完整运行设置；新增 cyber 字段可省略以兼容旧客户端，
省略时沿用当前已保存值，提供时按上述类型和范围校验。保存会在同一事务中推进 `config_revision` 并发布
新的 runtime snapshot，后续请求使用新策略，已开始的请求继续使用开始时快照。已有 Redis 条目的
剩余 TTL 不因设置编辑而变化，新写入使用新快照的 TTL；关闭开关只忽略现有条目，不会把它们用于
本地拒绝。

`openaiUserAgent` 为 `string | null`，默认 `null`，此时使用启动配置的平台和自动更新的版本。
自定义模板支持 `{originator}`、`{codex_version}`、`{desktop_version}`；版本变量使用当前已核验的
Core/Desktop 版本，无需每次发版修改模板。例如保持 Windows 平台并自动跟随版本：

```text
{originator}/{codex_version} (Windows 10.0.26100; x86_64) unknown ({originator}; {desktop_version})
```

模板最多 512 个可打印 ASCII 字符，不接受空白字符串或控制符；填写固定版本号则保持原文。
设置持久化后约 5 秒内用于新 HTTP 请求和 WebSocket 握手，重启后也会在对外服务前恢复。
已开始的请求继续完成；依赖旧 WebSocket 连接的续接可能因画像变化而失效。

OpenAI 环境与设备 metadata 覆盖由以下字段控制，管理端运行设置页提供相同的编辑项：

| 字段 | 类型 | 默认值 | 含义 |
| --- | --- | --- | --- |
| `openaiRequestBodyOverrideEnabled` | `boolean` | `true` | 是否更新 OpenAI 环境与搜索地域，并移除明确 metadata 容器中的设备字段 |
| `openaiRequestTimezone` | `string` | `America/Los_Angeles` | 有效的 IANA 时区；用于环境日期和搜索位置时区，支持夏令时 |
| `openaiSearchCountry` | `string` | `US` | 两位 ASCII 字母国家代码；保存和发送时统一为大写 |

`POST /api/admin/settings/update` 中这三个字段可分别省略或传 `null`，表示保留当前已保存值；提供值时
按上述类型和格式校验，并与其余运行设置原子保存。启用后，OpenAI Responses 的 HTTP 与 WebSocket
请求只更新最新的专用 Codex `input_text` 环境块（整个 XML 根为 `environment_context`，且直接存在
时区和日期字段）；日期由该次上游 attempt 单次捕获的 UTC 时间按配置时区（含夏令时）换算。引用文本、
工具输出、其他历史消息和其他环境块不改写；没有符合条件的环境块时不注入，也不通过
`previous_response_id` 补写历史。不添加搜索工具。

启用覆盖时，standalone Search 的 `settings.user_location` 与 Responses 中已声明且受支持的
`web_search` 工具的 `user_location` 都设置为近似位置对象：`type` 为 `"approximate"`，并写入配置的
`country` 与 `timezone`。覆盖会删除冲突的 `city` / `region`，保留其他无关字段，不添加搜索工具。
standalone Search 的 `settings` 或 `user_location` 缺失、为 `null` 时创建所需对象；已有字段不是对象时
保持不变。关闭覆盖或目标值没有变化时，standalone Search 整份正文保持原始 bytes；Responses 保留解析后的
请求结构，但不保证输入 JSON 的空白与转义字节。

同一开关还在账号 lease 选定后清理有限的设备 metadata。只处理 `client_metadata` 对象的直接键，以及
正文顶层或 `client_metadata` 内 `turnMetadata`、`turn_metadata`、`x-codex-turn-metadata` 的 JSON object
字符串；连接上下文、HTTP header、WebSocket 投影和 standalone Search 使用的 turn metadata 采用同一规则。
精确移除的键为：

```text
hostname, host_name, hostName, machine_id, machineId, device_id, deviceId,
hardware_id, hardwareId, os, os_name, osName, os_type, osType, os_version,
osVersion, platform, arch, architecture, cpu_arch, cpuArch, target_os, targetOs,
target_arch, targetArch, terminal, terminal_type, terminalType, shell
```

清理不递归，不扫描普通 `metadata`、`input`、`instructions`、`tools`、输出、opaque 或 encrypted 内容；
session、thread、window、turn、parent/root、`prompt_cache_key`、trace 和未知业务字段保持原值与相对顺序。
installation ID 仍按当前 lease 改写，账号身份与账号绑定状态仍按原 scoping 合同处理。非法 JSON、非 object
turn metadata 和非 object `client_metadata` 保持原有兼容行为；开关关闭只停用地域与设备覆盖，不停用账号
及 installation 身份隔离。

Windows 离线包接口固定解析 Microsoft Store Product ID `9PLM9XGG6VKS` 的 Retail 包，不接受调用方提供
产品 ID、上游地址、ring 或文件名。后端只返回通过包名、架构、Microsoft CDN host/path、scheme 和失效
时间校验的 `x64` / `arm64` MSIX 直链，不代理安装包字节。Store 内容通道返回 HTTP/80 临时地址时保留
原始 scheme，不强制改写为该 host 不保证支持的 HTTPS。动态链接不足 10 分钟即失效时不会下发；某个架构
解析失败时只将该架构降级到 OpenAI 官方 HTTPS 稳定 MSIX，并通过 `warning` 说明。响应形状为：

```json
{
  "resolvedAt": "2026-09-01T06:30:00Z",
  "cached": false,
  "warning": null,
  "packages": [
    {
      "architecture": "x64",
      "source": "microsoft_store",
      "version": "26.825.6671.0",
      "fileName": "OpenAI.Codex_26.825.6671.0_x64__2p2nqsd0c76g0.msix",
      "sizeBytes": 744250000,
      "downloadUrl": "http://dl.delivery.mp.microsoft.com/filestreamingservice/files/...",
      "expiresAt": "2026-09-01T07:30:00Z"
    }
  ]
}
```

`source` 为 `microsoft_store` 或 `official_openai`。Store 的四段 package version 只用于下载展示，不参与
Desktop 三段 SemVer 门禁，也不会自动回写最低版本设置。门禁规则见
[鉴权与公共约定](#1-鉴权与公共约定)，解析器职责见 [架构文档](architecture.md#11-生命周期安全与恢复)。

## 9. 备份

全部备份端点位于 `/api/admin/settings/backups/*`，内部由独立 BackupService 承担，不并入设置用例。响应继续使用 `AdminEnvelope`，wire 字段 camelCase，`Cache-Control: no-store`。

| 方法 | 路由 | 请求 | 说明 |
| --- | --- | --- | --- |
| `GET` | `/api/admin/settings/backups` | 无 | 读取存储配置（含明文 Secret）、验证状态与调度配置 |
| `POST` | `/api/admin/settings/backups/storage/update` | S3 配置 | 更新存储配置；`secretAccessKey` 为空字符串会校验失败 |
| `POST` | `/api/admin/settings/backups/storage/test` | 无 | 测试已保存的存储配置（Put/Head/Get/Delete 探针） |
| `POST` | `/api/admin/settings/backups/schedule/update` | 调度配置 | 更新 Cron、时区与保留策略 |
| `GET` | `/api/admin/settings/backups/records` | 查询参数 | 分页查询备份记录 |
| `POST` | `/api/admin/settings/backups/create` | `{ expiresInDays? }` | 创建手动备份，返回 `202 Accepted`；`expiresInDays` 为过期天数（0 或缺省表示不过期） |
| `POST` | `/api/admin/settings/backups/download-url` | `{ backupId }` | 创建 5 分钟有效预签名下载地址（仅 completed） |
| `POST` | `/api/admin/settings/backups/delete` | `{ backupId }` | 请求删除（进入 `deleting`，由 Worker 收敛硬删除） |

读取设置响应（Secret 以明文返回，由前端掩码显示）：

```text
storageRevision, endpoint, region, bucket, accessKeyId, secretAccessKey, prefix,
forcePathStyle, verified, scheduleEnabled, cronExpression, scheduleTimezone,
retentionDays, retentionCount, nextRunAt, lastVerifiedAt, updatedAt
```

更新存储请求字段：

```text
endpoint, region, bucket, accessKeyId, secretAccessKey, prefix, forcePathStyle
```

`secretAccessKey` 为空字符串会校验失败；由于 GET 会回传已保存的明文 Secret，保存时始终整体提交当前值。已有备份记录时，endpoint/region/bucket/forcePathStyle 不允许变化（存储身份锁定，`409`）；只允许轮换凭据与修改 prefix。

保存相同配置保留验证状态、定时计划及配置版本。存储配置实际变化时，会同时使验证失效、暂停定时计划并清空下次运行时间；连接测试通过后需重新启用计划。

更新调度请求字段：

```text
scheduleEnabled, cronExpression, scheduleTimezone, retentionDays, retentionCount
```

`cronExpression` 为 5 段格式；`retentionDays`/`retentionCount` 为 0 表示禁用对应清理。启用计划前必须已保存完整存储配置且通过连接测试。

记录列表查询参数：

```text
page, pageSize, status, trigger
```

`status` 可取 `queued/dumping/uploading/completed/failed/deleting`；`trigger` 可取 `manual/scheduled`。记录响应字段：

```text
id, triggerKind, status, scheduledAt, objectKey, sizeBytes, sha256, attemptCount,
errorCode, errorMessage, startedAt, completedAt, expiresAt, createdAt, updatedAt
```

`expiresAt` 在创建时确定：手动备份来自 `expiresInDays`，计划备份来自当时的
`retentionDays`；到期后由 Worker 进入删除流程。

连接测试响应：

```text
{ ok, stage, code, message }
```

`stage` 为 `putObject/headObject/getObject/deleteObject`。探测成功后以 `storageRevision` CAS 写入 `lastVerifiedAt`；测试期间配置变化则丢弃结果。

备份错误映射（`AdminErrorCode` 既有体系）：

| HTTP | 场景 |
| --- | --- |
| `400` | 配置、Cron、时区或状态参数无效 |
| `404` | 备份记录不存在 |
| `409` | 已有活跃任务、状态冲突或存储身份锁定 |
| `502` | S3 兼容服务返回无效或失败响应 |
| `503` | PostgreSQL、`pg_dump` 或对象存储暂不可用 |

审计动作：`backup.s3_config_updated`、`backup.s3_connection_tested`、`backup.schedule_updated`、`backup.created`、`backup.download_url_created`、`backup.delete_requested`。审计详情与记录表均不保存 Secret、数据库连接串或预签名 URL query。

## 10. Dashboard、用量与错误

| 方法 | 路由 | 说明 |
| --- | --- | --- |
| `GET` | `/api/admin/dashboard/summary` | Dashboard 汇总；支持 `kind`、`startTime`、`endTime` |
| `GET` | `/api/admin/dashboard/trend` | Dashboard 趋势；`kind=usage|latency|errors` |
| `GET` | `/api/admin/usage/records` | 请求记录分页列表 |
| `GET` | `/api/admin/usage/records/detail` | 按 `id` 查询请求详情 |
| `GET` | `/api/admin/usage/records/summary` | 当前筛选条件的请求汇总 |
| `GET` | `/api/admin/usage/insights/overview` | 用量、成本与成功率洞察 |
| `GET` | `/api/admin/usage/insights/diagnostics` | 按维度聚合诊断 |
| `GET` | `/api/admin/operations/errors` | 运维错误分页列表 |

用量查询可组合页码/游标、时间范围、Provider、Client Key、账号、模型、route、transport、状态码、
request/response/upstream ID、outcome 与搜索文本。诊断 `dimension` 可取 `model`、`account`、
`apiKey`、`provider`、`transport`、`failureClass`、`status`。

请求记录列表的 `search` 使用区分大小写的字面量前缀匹配，支持请求 ID、Client Key ID / Key 前缀、
账号 ID、账号邮箱与名称、请求 / 上游模型 ID、上游请求 ID。账号邮箱与名称按请求记录的历史快照检索，
不随当前账号修改或删除而改变；`%`、`_` 和 `\` 均按普通字符处理，不作为搜索通配符。

请求 ID 和上游 ID 继续通过既有字段查询；不增加入口 ID 字段，也不扫描 trace 建立查询映射。
旧响应中只有入口 ID 时仍需结合时间与入口日志定位，不能回填不存在的关联。
管理端搜索完整 `sk_` Client Key 时仅提交其可见前缀，不将完整密钥放入 URL。
错误列表的主动刷新、搜索和平台/时间条件变化会取得新的结束时间；翻页沿用该次查询快照。

已进入模型执行会话、但在首次合法 ProviderStream 建立前失败的请求也进入现有错误及详情查询，
包含无可用账号、准备失败、启动/准备超时与取消。此时 attempt 数为零，未确认的 Provider、账号及
上游传输为空；可用模型执行 ID 在“全部平台”下查询，不从路由候选推断实际调用平台。
已有合法 stream 后的失败仍保留真实 attempt，即使尚未收到首事件。
鉴权、解析、路由和准入等入口拒绝不属于该范围；请求观测仍是可能延迟或丢弃的异步投影。

汇总与洞察中的请求数与 outcome 分布覆盖筛选范围内全部请求；token、缓存、延迟与成本聚合仅统计
已完整交付客户端的成功响应。

OpenAI 请求的 `turnState` 是**最终 attempt 实际收到的上游值**，不会借用其他 attempt、账号最新
观测或手动发送的覆盖值。HTTP/SSE 响应头和 WebSocket 当前请求的 metadata 在成功、失败和取消时
均可形成观测。每个 attempt 保留最后一次有效值；`changed` 表示期间曾收到不同值。WebSocket 复用
连接的旧握手值不会归给后续请求。`bytes` 为解析后的原值 UTF-8 字节数，292 bytes 只是暂定观察规则，
不代表模型身份或回答质量。

请求列表中的每个 item 只增加摘要 `turnState: { classification, bytes }`，不包含明文、hash 或原始
metadata。列表沿用现有“完整交付成功且有用量证据”的筛选语义；失败、取消或未完成请求可按 ID 查看
详情。详情的顶层 `turnState` 在摘要之外增加 `value`、`sha256`、`observedAt`、`source`
（`http` 或 `websocket`）、`upstreamResponseId`、`attemptIndex` 和 `changed`：

```json
{
  "classification": "observed292", "bytes": 292,
  "value": "upstream-value", "sha256": "lowercase-hex-sha256",
  "observedAt": "2026-09-16T12:00:00Z", "source": "http",
  "upstreamResponseId": null, "attemptIndex": 2, "changed": false
}
```

`classification` 为 `observed292`、`observedOther`、`unobserved`、`notCollected`、
`pending` 或 `notApplicable`。前四种分别代表 292 bytes、其他长度、本版已采集但最终 attempt 未观测、
升级前的历史记录未采集；`pending` 为 OpenAI Responses 请求仍运行，`notApplicable` 为非 OpenAI
Responses 请求或尚无已确认 Provider 归属的请求。无值时 `bytes`、`value`、`sha256`、`observedAt`、
`source`、`upstreamResponseId`、
`attemptIndex` 均为 `null`，`changed` 为 `false`。

`GET /api/admin/usage/records/summary` 以相同时间和筛选范围增加：

```json
{
  "turnState": {
    "observed292": 12, "observedOther": 3, "unobserved": 5,
    "notCollected": 7, "hitRate": 0.8, "coverageRate": 0.75
  }
}
```

只统计已终结的 OpenAI 逻辑请求，每个请求一次；`pending` 与 `notApplicable` 不计。
`hitRate = observed292 / (observed292 + observedOther)`，
`coverageRate = (observed292 + observedOther) / (observed292 + observedOther + unobserved)`；
历史 `notCollected` 不入比率分母，任一分母为零时该比率为 `null`。请求级明文随
`usageRetentionDays` 的请求历史清理，普通诊断导出仍不包含它；账号最近观测保持独立的覆盖和保留语义。

详情接口按 `id` 可读取成功、失败或未完成请求。新增 `trace`（历史未采集记录为 `null`）和
`relatedRequests[]`（`requestId / relation / outcome / completedAt`）；`relation` 为 `recovered_by` 或
`recovers`。`trace` 是执行终态时的有界脱敏时间线，包含 request、attempt 和 exchange 关联、阶段、
事件摘要及淘汰计数；普通用量列表不携带此字段。
新采集的未知 JSON 键名与值只保留结构和摘要；事件摘要中的 `eventType` 为已知事件名称字符串、
未知名称的 `{ bytes, sha256 }` 摘要，或缺失时的 `null`。旧 trace 不做清理或回填，
其中的 `sanitized` 标记不能作为可直接公开的保证。

管理端下载的诊断包 `schemaVersion: 2` 用于人工反馈，不是备份或导入格式。它包含关联 ID、错误分类摘要、
请求与错误事件各自的状态、attempt、时间线阶段和计时；不自动导出 message/raw error、任意 metadata、
trace event data、请求响应正文和头部。`availability` 与 `omitted` 明示未采集、不完整或主动省略的内容，
`null` 不代表没有发生错误。版本、环境及原始错误片段仍需操作者另行补充并审阅脱敏。

错误记录中的“已自动恢复”表示系统关联到了后续成功请求，不会把原来的失败记录改为成功。
`upstreamSendState = ambiguous` 表示无法确认该次上游执行结果，不代表后续恢复请求失败；
恢复关联也不等于逐字节验证过两次请求正文。

Dashboard 的 `accountUsage[]` 由后端提供 `usageWindow`、`metricLabel`、`metricValue`。
`usageWindow` 复用账号额度窗口合同，缺失额度事实时为 `null`；窗口标签、百分比、触顶状态、重置时间
和本地用量由 Provider/Admin 投影。前端不得从套餐缺失推断免费套餐，也不得从显示时舍入的百分比推断
触顶。滚动窗口使用相应时间范围的本地用量，独立于 Dashboard 的今日统计范围。

Dashboard 的 `wireProfiles[]` 中，`userAgent` 是 Provider 当前发送的最终有效值，`userAgentSource` 为
`launch_profile` 或 `admin_override`；`target` 只表示启动平台基线。管理端覆盖任意自定义 User-Agent 时，
服务端不从字符串反推操作系统，因此 `target` 可以与 User-Agent 文本不同。

OpenAI 的 `serviceTier` 只接受上游响应生命周期事件确认的实际 `response.service_tier`；请求里的
期望档位只保留在 request summary，不能冒充响应事实。计费展示把 `priority`/`fast` 映射为 `Fast`，
`flex` 映射为 `Flex`，缺失或 `default` 映射为 `Default`；未知非空值原样展示。Fast 优先使用模型的
priority 价格，缺少专用价格时回退到标准价格的 `2.00x`；Flex 为 `0.50x`，Default 为 `1.00x`。

## 11. 版本、更新与重启

| 方法 | 路由 | 主要 query/body | 说明 |
| --- | --- | --- | --- |
| `GET` | `/api/admin/system/version` | 无 | 当前构建、部署模式和可用更新 |
| `GET` | `/api/admin/system/update/detail` | `refresh=true|false` | 读取或强制刷新 Release 详情 |
| `GET` | `/api/admin/system/update/events` | 无 | SSE 更新事件流 |
| `POST` | `/api/admin/system/update` | 可选 `{ targetVersion }` | 开始在线更新 |
| `GET` | `/api/admin/system/update/status` | 无 | 查询当前更新或回滚状态 |
| `POST` | `/api/admin/system/rollback` | 无 | 回滚到保留的上一版本 |
| `POST` | `/api/admin/system/restart` | 无 | 请求进程重启 |

在线更新仅在当前部署模式、Release 资产和进程重启能力都满足要求时可用，且只在同一 major 版本内
提供：跨大版本目标会以 `40901` 冲突拒绝，需按发布说明重新部署。
实例升级和仓库发版见 [部署文档](../deploy/README.md#镜像升级与源码构建)。
