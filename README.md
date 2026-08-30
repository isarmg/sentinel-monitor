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

准备受控版本的 MediaMTX、Caddy，以及本项目原生脚本。可参考
[`native/README.md`](native/README.md)。

简要步骤：

```bash
cd /mnt/sarmg.org/sentinel-monitor
cp .env.example .env.native
# 编辑 .env.native 中的数据库、JWT、管理员密码和媒体地址
./native/bootstrap.sh
./native/build.sh
./native/start.sh
./native/status.sh
```

停止：

```bash
./native/stop.sh
```

浏览器入口为 `http://127.0.0.1:8080`。WHEP 使用 `8889/TCP`，HLS 使用 `8888/TCP`，WebRTC
媒体使用 `8189/UDP`。

## 环境变量

复制 `.env.example` 为本机私有文件，并按需设置：

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
MEDIAMTX_CONFIG=/etc/sentinel-monitor/mediamtx.yml
MEDIAMTX_CONTRACT=/etc/sentinel-monitor/mediamtx.lock
MEDIAMTX_BINARY=/opt/sentinel-monitor/bin/mediamtx
RECORDINGS_DIR=/var/lib/sentinel-monitor/recordings
SENTINEL_RUNTIME_DIR=/run/sentinel-monitor
REQUEST_TIMEOUT_SECS=20
PUBLIC_HLS_BASE_URL=/media-hls
PUBLIC_WEBRTC_BASE_URL=/media-webrtc
STATIC_DIR=web/dist
```

生产模式始终使用 `__Host-sentinel_session` Secure/HttpOnly/SameSite Cookie。仅本机开发可设置 `APP_ENV=development`，且服务会拒绝绑定非 loopback 地址。
登录入口同时按连接来源和规范化账户名执行有界 Token Bucket 限流；Argon2 并发与超时参数应按主机内存和 CPU 预算调整。

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

`serve` 在访问数据库前同时持有数据库同目录下的
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
`GET /api/media/operations/{operation_id}` 查询
`pending/running/succeeded/failed/unknown`。删除接口改为 `202 Accepted` 并返回同一操作对象；
摄像头会立即从产品 API 隐藏，但 MediaMTX Path 的异步清理状态仍可追踪。

操作响应、持久错误和日志只包含摄像头 ID、操作 ID 与固定错误码。完整 RTSP Source、URL userinfo、
用户名和密码均不会进入这些边界；实际态只保存不可逆 SHA-256 摘要。

## MediaMTX Companion 契约

原生部署固定使用 `linux_amd64` MediaMTX `v1.20.0`，其二进制 SHA-256 记录在
[`native/mediamtx.lock`](native/mediamtx.lock)。`native/start.sh` 会同时校验平台、版本和摘要，
任一不匹配都会拒绝启动。升级 MediaMTX 时必须在独立变更中更新二进制、锁文件、两份配置模板，
并重跑协调器 fake-process 回归测试；不要使用浮动 `latest`。

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
schema_sha256=c06dde59a25ca34d4f64f38f0306822b649efefb6e063f2f964f43e34d014de4
```

指纹从 `sqlite_schema` 读取 `name NOT GLOB 'sqlite_*'` 且 `name <> 'product_metadata'` 的
`(type,name,tbl_name,COALESCE(sql,''))`，按 `type,name,tbl_name` 排序；每个 UTF-8 字段先馈入 u64
big-endian 字节长度，再馈入原字节，最终输出小写 SHA-256。启动会同时比对元数据和现场重新计算值。

## 运维

Sentinel 进程全生命周期持有数据库 instance/shared-maintenance 锁和 runtime `app.lock`，MediaMTX
由 `native/start.sh` 的 `flock --no-fork` 持有 `mediamtx.lock`。外部升级工具必须在服务停止后，按
数据库 maintenance、runtime、MediaMTX 的固定顺序取得排他锁；产品仓库不提供改变历史数据代际的
命令。

```bash
set -a
source .env.native
set +a

"$SENTINEL_RUNTIME_DIR/bin/sentinel-monitor" doctor --offline

./native/start.sh
"$SENTINEL_RUNTIME_DIR/bin/sentinel-monitor" doctor
```

`doctor` 执行 current-schema 元数据与现场指纹比对、`integrity_check`、`foreign_key_check`、可回滚
数据库写探针、录像目录读写探针、全量凭据解密检查及 MediaMTX 二进制版本/SHA/配置契约检查；默认
再检查两个 loopback readiness 端点，停机检查使用 `--offline`。

- 将 `.env.native`、`auto.crt`、`auto.key` 放在主机秘密管理机制中，不要提交版本库。
- `CREDENTIALS_KEY` 必须由主机秘密管理机制托管；任何数据代际变更都交给独立升级工具。
- 摄像头放在独立 VLAN；MediaMTX 的 9996、9997、9998 端口不应暴露到互联网。
