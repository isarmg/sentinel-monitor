# 哨界 Sentinel Monitor

一个以 Rust 为控制面、MediaMTX 为媒体面的浏览器摄像头监控系统，只支持物理机原生部署。

## 架构

```text
IP Camera (RTSP / ONVIF)
          |
          v
      MediaMTX  <------>  Rust / Axum  <------> SQLite
       |   |                |   |
    WHEP  HLS          Auth/API/PTZ       encrypted secrets
       \   /                |
        Caddy same-origin gateway
                 |
             Browser UI
```

## 物理机部署

Sentinel 0.2.0 使用唯一物理版本目录，不支持旧 `.env.native`、旧 runtime 或任何就地迁移。正式发布
必须来自完全干净、HEAD 正好由 annotated `v0.2.0` 标记的 checkout。准备与
[`native/mediamtx.lock`](native/mediamtx.lock) 匹配的 MediaMTX `linux_amd64` 二进制，然后执行一次性发布：

```bash
export SENTINEL_MEDIAMTX_SOURCE=/absolute/path/to/mediamtx
./native/build.sh

/opt/isarmg/sentinel-monitor/releases/0.2.0/native/bootstrap.sh
sudoedit /etc/isarmg/sentinel-monitor/sentinel-monitor.env
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/bootstrap.sh --confirm-config
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/start.sh
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/status.sh
```

`bootstrap.sh` 不启动服务、不覆盖现有环境文件，也不回显随机初始秘密。停止服务使用发行目录中的入口：

```bash
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/stop.sh
```

默认布局只有物理 `/opt/isarmg/sentinel-monitor/releases/0.2.0`，不创建 `current`、`latest` 或其他
可切换别名；配置位于 `/etc/isarmg/sentinel-monitor`，数据位于 `/var/lib/isarmg/sentinel-monitor`，
运行锁位于 `/run/isarmg/sentinel-monitor`。`start.sh` 与 `stop.sh` 还共享一个短期 `operations.lock`，防止并发
启动器竞争 PID 文件；它不会被长寿命子进程继承。完整流程和权限边界见
[`native/README.md`](native/README.md)。

仓库不发布 systemd unit，也不依赖 systemd 的相对工作目录或环境注入；原生部署只有 release 内上述
脚本这一套生命周期合同。

## 环境变量

`.env.example` 只作字段参考。实际 0600 文件由已安装的 bootstrap 在源码树之外创建，核心路径必须是：

```dotenv
DATABASE_URL=sqlite:///var/lib/isarmg/sentinel-monitor/db/app.db
APP_JWT_SECRET=<至少32字符随机值>
CREDENTIALS_KEY=<base64编码的32字节随机值>
BOOTSTRAP_ADMIN_EMAIL=admin@example.com
BOOTSTRAP_ADMIN_PASSWORD=<初始管理员密码>
APP_ENV=production
SESSION_IDLE_TTL_MINUTES=30
SESSION_ABSOLUTE_TTL_HOURS=12
LOGIN_BODY_LIMIT_BYTES=16384
LOGIN_RATE_CAPACITY=4096
LOGIN_SOURCE_ATTEMPTS=30
LOGIN_SOURCE_WINDOW_SECS=60
LOGIN_ACCOUNT_ATTEMPTS=10
LOGIN_ACCOUNT_WINDOW_SECS=300
LOGIN_ARGON2_PARALLELISM=2
LOGIN_ARGON2_TIMEOUT_MS=5000
MEDIAMTX_API_URL=http://127.0.0.1:9997
MEDIAMTX_PLAYBACK_URL=http://127.0.0.1:9996
MEDIAMTX_CONFIG=/opt/isarmg/sentinel-monitor/releases/0.2.0/config/mediamtx.yml
MEDIAMTX_CONTRACT=/opt/isarmg/sentinel-monitor/releases/0.2.0/config/mediamtx.lock
MEDIAMTX_BINARY=/opt/isarmg/sentinel-monitor/releases/0.2.0/bin/mediamtx
RECORDINGS_DIR=/var/lib/isarmg/sentinel-monitor/recordings
SENTINEL_RUNTIME_DIR=/run/isarmg/sentinel-monitor
REQUEST_TIMEOUT_SECS=20
PUBLIC_HLS_BASE_URL=/media-hls
PUBLIC_WEBRTC_BASE_URL=/media-webrtc
STATIC_DIR=/opt/isarmg/sentinel-monitor/releases/0.2.0/web
```

`STATIC_DIR` 没有默认值，必须等于已验证物理版本中的 `web/`。正式二进制在读取配置、取得运行锁或访问
数据库前，先由同一进程验证自身物理位置、40 位源码 revision、target、`v2` 协议、当前 Schema、凭据
envelope、Web 静态契约、MediaMTX lock/config/二进制以及整个 manifest 文件树；缺失、增加、篡改、
错误权限、特殊文件、符号链接和硬链接都会失败。源码绑定的正式二进制只接受
`serve-release /opt/isarmg/sentinel-monitor/releases/0.2.0`；普通 `serve` 仅供明确未绑定 revision 的
开发构建使用。

生产模式始终使用 `__Host-sentinel_session` Secure/HttpOnly/SameSite Cookie。仅本机开发可设置 `APP_ENV=development`，且服务会拒绝绑定非 loopback 地址。
登录入口同时按连接来源和规范化账户名执行有界 Token Bucket 限流；Argon2 并发与超时参数应按主机内存和 CPU 预算调整。

## 当前协议边界

Sentinel 0.2 的唯一浏览器/API 前缀是 `/api/v2`，唯一 MediaMTX 回调是
`/internal/v2/media/auth`；产品不注册 0.1 路由或别名。Rust 路由、Web 客户端与 MediaMTX 配置共享
[`web/src/protocol-contract.json`](web/src/protocol-contract.json) 中的当前协议身份，静态测试会阻止三者
漂移。所有请求 DTO 都拒绝未知字段，缺少当前协议必需字段的请求在进入业务处理前失败。

媒体 JWT 固定为 Sentinel 0.2 的 `protocol`、`iss`、`aud`、`kind`，并要求非空当前
`sub/camera_id/jti` 与严格 `iat/nbf/exp`。签名密钥从 `APP_JWT_SECRET` 通过带 Sentinel 0.2 域标签的
HKDF-SHA256 派生；原始密钥签发的 0.1 token 即使尚未过期也不能通过验证。不提供 previous secret、
keyring 或协议 fallback；跨版本转换属于外部升级工具职责。

## 当前摄像头凭据 envelope

Sentinel 0.2 只接受一种摄像头密文：数据库 BLOB 必须是规范化 JSON，且恰好包含
`product=sentinel-monitor`、`application_version=0.2.0`、`envelope_revision=1`、
`key_id=sentinel-credentials-0.2.0-key-1`、`nonce` 和 `ciphertext`。后两项仅接受无填充
URL-safe Base64；未知字段、非规范序列化、其他产品/版本/revision/key ID、旧版
`nonce || ciphertext` 字节串、裸 Base64 和任何篡改都会 fail closed。

AES-256-GCM 专用 key 由 `CREDENTIALS_KEY` 使用 HKDF-SHA256 派生，salt 为
`sentinel-monitor/0.2.0/credential-envelope/key/v1`，info 为
`sentinel-credential-envelope/aes-256-gcm`。AAD 把以下 UTF-8 字段逐项编码为
`u64 big-endian 字节长度 || 原字节`：固定域
`sentinel-monitor/0.2.0/credential-envelope/aad/v1`、产品、应用版本、envelope revision、key ID、
小写连字符 UUID camera ID，以及精确数据库字段名 `main_stream_url_enc`、`sub_stream_url_enc`、
`username_enc` 或 `password_enc`。因此密文不能跨摄像头或字段互换。

产品没有 previous key、运行时 keyring、旧密文解析或自动改写。`username` 在当前 schema 中也只以
`username_enc` 保存；密码和流 URL 不进入 API 响应、审计或日志。任何 0.1 到当前 envelope 的转换由
外部升级工具在产品停止时完成。

## MediaMTX 一致性模型

摄像头写接口不再把一次 HTTP 请求伪装成数据库与 MediaMTX 的原子事务：

1. 创建、修改或删除摄像头时，先在同一个 SQLite 事务中提交摄像头期望态和
   `media_operations` 记录；
2. 后台协调器原子领取 `pending` 操作，再在事务之外调用 MediaMTX；
3. 成功后记录 `succeeded` 与 `media_actual_paths`，明确失败记录 `failed` 并指数退避；
4. 全局协调租约和单操作租约都绑定唯一 owner，租期按最坏 MediaMTX 请求次数与
   `REQUEST_TIMEOUT_SECS` 计算并在阶段边界续期；只有同时持有未过期全局/操作租约的 owner 才能
   finalize。启动恢复只把确实过期（或缺失租约）的 `running` 转为 `unknown`，不会清空其他健康
   owner 的租约；
5. 周期性比较期望 Path 与 MediaMTX 的配置/Publisher/Recording 实际态，发现漂移会创建新的
   `drift_detected` 操作。

正式 `serve-release`（以及开发 `serve`）在访问数据库前同时持有数据库同目录下的
`.app.db.sentinel-monitor.instance.lock`（排他）与
`.app.db.sentinel-monitor.maintenance.lock`（共享），并继续持有绝对路径
`SENTINEL_RUNTIME_DIR/app.lock`、维护 `app.pid` 到退出。数据库锁按数据库路径身份锚定，所以即使把
同一 `DATABASE_URL` 误配到两个 runtime，第二实例也会在打开 SQLite 前拒绝；数据库文件、数据库父
目录或锁文件的符号链接，以及数据库/锁文件的硬链接别名同样 fail closed。维护操作统一先取得数据库
maintenance 排他锁，再取得 runtime/MediaMTX 锁，避免跨身份锁顺序反转。数据库租约仍然是必要的
第二道 fencing，用于防御旧 worker 超时返回等业务级并发。请求超时限制为 1–300 秒，避免配置值超过
可证明的租约预算。

创建和修改响应保留原有的 `camera`、`media_synced`、`warning` 字段，并增加：

```json
{
  "operation_id": "7a5d...",
  "operation_state": "pending"
}
```

其中 `media_synced=false` 表示已可靠排队而非失败。可通过
`GET /api/v2/media/operations/{operation_id}` 查询
`pending/running/succeeded/failed/unknown`。删除接口改为 `202 Accepted` 并返回同一操作对象；
摄像头会立即从产品 API 隐藏，但 MediaMTX Path 的异步清理状态仍可追踪。

操作响应、持久错误和日志只包含摄像头 ID、操作 ID 与固定错误码。完整 RTSP Source、URL userinfo、
用户名和密码均不会进入这些边界；实际态只保存不可逆 SHA-256 摘要。

## MediaMTX Companion 契约

原生部署固定使用 `linux_amd64` MediaMTX `v1.20.0`，其二进制 SHA-256 记录在
[`native/mediamtx.lock`](native/mediamtx.lock)。构建发布和已安装的 `start.sh` 都会同时校验平台、版本
和摘要；二进制、配置与 lock 均来自同一只读 release。任一不匹配都会拒绝启动。升级 MediaMTX 时必须
在独立变更中更新二进制、lock 与配置，并重跑协调器 fake-process 和 native lifecycle 回归测试；不要
使用浮动 `latest`。

## 角色权限

| 操作 | 管理员 | 操作员 | 观察员 |
|---|---:|---:|---:|
| 实时预览、录像回放 | 是 | 是 | 是 |
| PTZ、确认事件、设备发现 | 是 | 是 | 否 |
| 摄像头配置 | 是 | 否 | 否 |
| 用户、审计和系统管理 | 是 | 否 | 否 |

## 当前 Schema 边界

Sentinel `0.2.0` 产品进程只理解并初始化一个当前 schema，不承载旧版本识别、升级 migration、备份
或恢复。数据库文件仅在目标路径完全不存在时按 [`src/current_schema.sql`](src/current_schema.sql)
初始化；任何已存在但缺少元数据、属于 `0.1.x`、版本/修订不精确或实际结构被改动的数据库，都会在
产品执行写操作前只读拒绝。升级、备份和恢复由独立升级工具仓库负责，不能调用本产品二进制伪装完成。

`product_metadata` 必须恰好一行，且精确记录：

```text
application=sentinel-monitor
application_version=0.2.0
schema_revision=1
schema_sha256=b089342e00e672d6e6c679e15f331c90e599129371042a37948a4b53e5f8e49e
```

指纹从 `sqlite_schema` 读取 `name NOT GLOB 'sqlite_*'` 且 `name <> 'product_metadata'` 的
`(type,name,tbl_name,COALESCE(sql,''))`，按 `type,name,tbl_name` 排序；每个 UTF-8 字段先馈入 u64
big-endian 字节长度，再馈入原字节，最终输出小写 SHA-256。启动会同时比对元数据和现场重新计算值。

`media_reconciler_leases` 也是 Sentinel 0.2 当前状态契约的一部分：表必须是编译期固定形状，且恰好
包含 `singleton=1` 一行。空闲态要求 `lease_owner` 与 `lease_expires_at` 同时为 `NULL`；持有态要求
owner 是小写连字符 UUIDv4、两个时间字段都是规范 UTC RFC 3339，且 `lease_expires_at > updated_at`。
启动先从 main/WAL/journal 原始字节复制出私有 generation，再只在私有副本验证表 SQL、列、约束和
唯一状态行；缺行、额外行、旧 `scope='global'` 形状、未知列或非法状态都不会打开生产连接或创建/改写
SQLite sidecar。产品不会补行、删行或修复旧状态；协调器运行中发现损坏会立即返回错误并等待下一次有
间隔的调度，不会把损坏误判为“另一 worker 正忙”或在内部忙循环。

## 运维

Sentinel 进程全生命周期持有数据库 instance/shared-maintenance 锁和 runtime `app.lock`，MediaMTX
由已安装 release 中 `start.sh` 的 `flock --no-fork` 持有 `mediamtx.lock`。外部升级工具必须在服务停止后，按
数据库 maintenance、runtime、MediaMTX 的固定顺序取得排他锁；产品仓库不提供改变历史数据代际的
命令。

```bash
set -a
source /etc/isarmg/sentinel-monitor/sentinel-monitor.env
set +a

"/opt/isarmg/sentinel-monitor/releases/0.2.0/bin/sentinel-monitor" doctor --offline

/opt/isarmg/sentinel-monitor/releases/0.2.0/native/start.sh
"/opt/isarmg/sentinel-monitor/releases/0.2.0/bin/sentinel-monitor" doctor
```

`doctor` 执行 current-schema 元数据与现场指纹比对、`integrity_check`、`foreign_key_check`、可回滚
数据库写探针、录像目录读写探针、全量凭据解密检查及 MediaMTX 二进制版本/SHA/配置契约检查；默认
再检查两个 loopback readiness 端点，停机检查使用 `--offline`。

- 将 `/etc/isarmg/sentinel-monitor/sentinel-monitor.env`、证书与私钥放在主机秘密管理机制中；部署脚本
  不会向源码树写入这些内容。
- `CREDENTIALS_KEY` 必须由主机秘密管理机制托管；任何数据代际变更都交给独立升级工具。
- 摄像头放在独立 VLAN；MediaMTX 的 9996、9997、9998 端口不应暴露到互联网。
