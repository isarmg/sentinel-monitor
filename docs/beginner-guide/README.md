# Sentinel Monitor 初学者学习指南

本手册按十章从摄像头、媒体面和控制面的基本概念，逐步进入持久操作、凭据、MediaMTX、测试与生产
运维。单页速览保留在章节索引之后，具体不变量和失败处置以专题章节为准。

1. [项目全景与版本边界](01-project-overview.md)
2. [开发环境与第一次运行](02-environment-and-first-run.md)
3. [Rust、视频链路与 Web 基础](03-rust-media-and-web-basics.md)
4. [服务端请求、认证与摄像头管理](04-server-request-and-camera-lifecycle.md)
5. [持久操作、协调器与故障恢复](05-operations-reconciler-and-recovery.md)
6. [MediaMTX、录像与播放链路](06-mediamtx-recording-and-playback.md)
7. [当前协议、加密与状态合同](07-current-contracts-and-cryptography.md)
8. [测试、调试与变更方法](08-testing-debugging-and-change-workflow.md)
9. [部署、安全与生产运维](09-deployment-security-and-operations.md)
10. [源码路线、练习与术语表](10-reading-roadmap-and-glossary.md)

以下内容是快速导读。

## 1. 先认识控制面和媒体面

摄像头视频不经过 Rust 应用转发。MediaMTX 连接 RTSP 摄像头并向浏览器提供 WHEP/HLS；Rust 应用保存
“应该有哪些 MediaMTX Path”的期望态，通过 MediaMTX 本地 API 协调实际态，并负责浏览器身份、角色、
临时媒体授权和 ONVIF 控制。

```text
IP Camera --RTSP/ONVIF--> MediaMTX --WHEP/HLS--> Caddy --> Browser
                              ^                    |
                              | local API/auth     | /api/v2
                              +------ Rust/Axum <--+
                                        |
                                      SQLite
```

这种拆分让媒体协议交给专用组件，Rust 控制面保持可审计；代价是必须严格绑定 companion 版本、配置、
二进制摘要以及数据库与 MediaMTX 的最终一致性。

## 2. 目录阅读顺序

1. `clients/web/src/protocol-contract.json`：浏览器、Rust 路由和 MediaMTX 回调共享的当前协议身份。
2. `src/main.rs`、`config.rs`、`routes.rs`：CLI、配置和 `/api/v2` 入口。
3. `auth.rs`、`login_security.rs`：用户 Session、CSRF、媒体 JWT 和登录保护。
4. `crypto.rs`：摄像头敏感字段的唯一当前 envelope。
5. `reconciliation.rs`、`mediamtx.rs`：期望态操作、租约和实际态协调。
6. `onvif.rs`：设备发现和 PTZ 边界。
7. `release.rs`、`native/*.sh`：固定发行树和原生生命周期。

## 3. 开发环境

需要 Rust `1.98`、Node/npm，以及可供集成测试使用的 Linux 工具。Web：

```bash
cd clients/web
npm ci
npm run build
```

开发构建必须显式设置 `APP_ENV=development`、回环绑定和 `STATIC_DIR`，再运行：

```bash
cargo run -- serve
```

正式 source-bound binary 拒绝普通 `serve`。不要在源码树保存生产 `.env`、MediaMTX binary 或凭据。

## 4. 浏览器登录

管理员初始身份在全新数据库初始化时由环境提供，密码使用 Argon2。登录成功后浏览器取得
`__Host-sentinel_session` Secure/HttpOnly/SameSite Cookie；写请求同时需要 Session 绑定 CSRF。
登录按真实连接来源和规范化账户分别限流，并受请求体、Argon2 并发与超时预算保护。

角色：观察员可预览和回放；操作员额外拥有 PTZ、事件确认和发现；管理员再拥有摄像头、用户、审计
及系统管理。

## 5. 摄像头凭据为什么是 envelope

主/辅流 URL、用户名和密码都以规范 JSON envelope 的 AES-256-GCM 密文保存。专用 key 从
`CREDENTIALS_KEY` 经 HKDF-SHA256 派生；AAD 绑定产品、版本、revision、key ID、camera UUID 和精确
数据库字段，因此密文不能复制到另一摄像头或另一字段。

当前 key ID 固定为 `sentinel-credentials-0.2.0-key-1`。产品没有 previous key/keyring，不接受旧
`nonce || ciphertext` 或宽松 Base64。`CREDENTIALS_KEY` 丢失意味着密文不可恢复。

## 6. 一次摄像头变更

HTTP 请求不能同时原子提交 SQLite 和远端 MediaMTX。因此 API 先在一个 SQLite 事务写入摄像头期望态
和 `media_operations`，立即返回 operation ID；后台协调器取得租约，在事务外调用 MediaMTX，再写入
成功、失败或 unknown。客户端通过状态 API 查询结果。

`unknown` 表示远端效果可能已经发生，不能盲目新建重复请求。周期 reconciler 会比较期望 Path、实际
配置、Publisher 和 Recording，发现漂移后创建显式操作。

## 7. 播放和录像

浏览器先向当前 API 申请短时媒体 JWT，再通过同源 Caddy 入口访问 WHEP/HLS。MediaMTX 调用唯一
`/internal/v2/media/auth` 校验。JWT 使用从 `APP_JWT_SECRET` 派生的当前签名 key，严格绑定 protocol、
issuer、audience、kind、camera、jti 与时间窗；不验证旧 token。

录像目录由 MediaMTX 写入、控制面索引和授权。Web 不应直连 9996/9997/9998 管理/媒体端口。

## 8. 当前 Schema

数据库只在主文件完全不存在时创建。启动从 main/WAL/journal 原始字节构造私有 generation，验证唯一
`product_metadata`、实际 `sqlite_schema` 指纹和 reconciler singleton 状态，再打开生产写连接。已有空
文件、旧版、额外列、非法租约或 Schema drift 都只读拒绝，不自动补表/补行。

## 9. 修改代码的方法

- 路由变化：同步 Rust、Web contract、MediaMTX 配置和测试。
- 凭据变化：定义新的完整当前 envelope；历史转换只能放在升级仓库。
- 协调器变化：证明租约 owner、过期、续期、finalize 和 shutdown 的 fencing。
- MediaMTX 升级：同一变更更新版本、SHA-256、lock、配置和 lifecycle 测试。
- 发行变化：更新全树 manifest 和重定位/篡改负例。

## 10. 术语

- **RTSP**：摄像头常用实时流输入协议。
- **ONVIF**：摄像头发现、能力和 PTZ 控制标准。
- **WHEP**：基于 HTTP 协商 WebRTC 播放的协议。
- **HLS**：基于分段 HTTP 的媒体播放协议。
- **PTZ**：Pan/Tilt/Zoom，云台水平、俯仰和变焦。
- **reconciliation**：把声明的期望态持续收敛为外部系统实际态。
- **fencing**：阻止过期 worker 在失去租约后提交结果的约束。
