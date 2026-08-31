# 04. 服务端请求、认证与摄像头管理

## 4.1 正式启动门

Server 在监听前验证不可变发行树、Web fingerprint、数据库路径和锁、当前 Schema、lease 不变量及全部
持久 credential 可由当前 external key 认证。无法证明任何一项即拒绝，不在请求期间懒修复。

## 4.2 登录链路

```text
bounded request -> source/account/global admission -> bounded Argon2
 -> Session digest + CSRF -> Secure Cookie
```

写 API 再验证 Session、CSRF 和 Origin/Host。forwarded header 只有在明确可信代理边界内才可使用。

## 4.3 创建摄像头

请求严格验证名称、URL/Host、协议参数和 credential；Secret 在同一事务中加密，持久化期望摄像头与
pending operation，再返回 operation 文档。HTTP 成功只表示意图已可靠接收，不表示媒体已可用。

## 4.4 修改与删除

每次变更均创建新的持久 operation，以资源 revision/幂等键防止并发覆盖。删除成功要区分控制面记录、
MediaMTX path 和录像保留策略；不能把“隐藏 UI 行”当成资源已删除。

## 4.5 状态查询

浏览器查询安全投影：资源 ID、展示字段、期望/实际摘要和 operation 状态。响应不得包含 encrypted request、
credential ciphertext、actor 内部标识、完整上游错误或播放 signing secret。

## 4.6 幂等

调用者重试同一意图应使用同一稳定键；相同 actor/resource/action/key 且相同请求返回原 operation，不同
请求复用必须冲突。生成新 key 会创建新副作用，不能用于“看看是否成功”。

## 4.7 响应语义

- `202`：持久 operation 已创建或找到。
- `401/403`：身份/CSRF/授权失败。
- `409`：revision、幂等或状态冲突。
- `422`：输入符合 JSON 但不满足业务边界。
- `429/503`：准入或依赖暂时不可用，可按响应策略重试。

## 4.8 调试

使用 request ID、operation ID 和 camera ID 关联日志，不打印 URL credential。先证明操作是否已持久化，
再检查 claim/lease、MediaMTX 请求与终态事务，最后才看 Web 刷新。

## 4.9 API 变更

同步修改 Rust DTO/路由、Web client、严格测试、发行 API 身份和文档；直接删除旧字段/路径，不注册 alias。
