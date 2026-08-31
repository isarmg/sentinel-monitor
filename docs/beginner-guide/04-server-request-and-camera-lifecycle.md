# 04. 服务端请求、认证与摄像头管理

## 4.1 正式启动门

Server 在监听前验证不可变发行树、Web fingerprint、数据库路径和锁、当前 Schema、lease 不变量及全部
持久 credential 可由当前 external key 认证。无法证明任何一项即拒绝，不在请求期间懒修复。

## 4.2 登录链路

```text
{username,password} exact request -> Foundation username normalization
 -> source/canonical-account/global admission -> bounded Argon2
 -> Session digest + CSRF -> Secure Cookie
```

成功响应只有 `{authenticated,user_id,username,role:"admin",csrf_token}` 五个字段。写 API 再验证 Session、
CSRF 和 Origin/Host。forwarded header 只有在明确可信代理边界内才可使用。管理 username 只标识
Administrator；摄像头 username 是加密设备凭据，不参与这条登录链。

## 4.3 创建摄像头

请求严格验证名称、URL/Host、协议参数和 credential；Secret 在进入事务前按 camera/字段上下文加密，随后持久化期望摄像头与
pending operation，再返回 operation 文档。HTTP 成功只表示意图已可靠接收，不表示媒体已可用。

## 4.4 修改与删除

每次摄像头 create/update/delete 都增加 desired generation 并创建或收口该代 operation；当前没有通用
`Idempotency-Key` header，也没有客户端 revision CAS。唯一活跃 generation 索引和 reconciler 的
desired-state 收敛防止同代重复执行。删除成功要区分控制面 soft-delete、MediaMTX path 清理和录像保留；
不能把“隐藏 UI 行”当成全部媒体字节已删除。

## 4.5 状态查询

浏览器查询安全投影：资源 ID、展示字段、是否存在子流/ONVIF、状态和 operation 状态。响应不得包含
RTSP URL、credential ciphertext、密码、完整上游错误或播放 signing secret。

## 4.6 重试语义

摄像头配置是期望态：请求成功后保存返回的 operation ID，并查询其状态；网络超时不能据此断言请求未
持久化。当前 API 不接受幂等键，调用方不能凭空假设同一请求会返回原 operation。PTZ 则是同步瞬时动作，
当前不进入 durable operation；断线后结果无法从 operation API 恢复，禁止自动盲重放 move。

## 4.7 响应语义

- `201`：摄像头已创建且 operation 已持久化；`200`：更新响应；`202`：删除意图已接收。
- `401/403`：身份/CSRF/授权失败。
- `409`：当前资源或账户状态冲突。
- `400`：JSON、字段或业务边界验证失败。
- `429/503`：准入或依赖暂时不可用，可按响应策略重试。

## 4.8 调试

使用时间、operation ID 和 camera ID 关联日志，不打印 URL credential。当前只启用 tower TraceLayer，没有
请求 ID 中间件，不能让排障流程依赖不存在的字段。先证明操作是否已持久化，再检查 claim/lease、
MediaMTX 请求与终态事务，最后才看 Web 刷新。

## 4.9 API 变更

同步修改 Rust DTO/路由、Web client、严格测试、发行 API 身份和文档；直接删除旧字段/路径，不注册 alias。
