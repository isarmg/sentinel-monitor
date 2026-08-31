# Sentinel Monitor 运维文档

## 1. 唯一生产布局

```text
/opt/isarmg/sentinel-monitor/releases/0.2.0/
├─ RELEASE-MANIFEST
├─ bin/{sentinel-monitor,mediamtx}
├─ web/{index.html,assets/...}
├─ config/{mediamtx.yml,mediamtx.lock}
└─ native/{bootstrap,start,status,stop,common}.sh

/etc/isarmg/sentinel-monitor/sentinel-monitor.env
/var/lib/isarmg/sentinel-monitor/{db,recordings,logs}
/run/isarmg/sentinel-monitor/{operations.lock,app.lock,app.pid,mediamtx.lock,mediamtx.pid}
```

版本树 root-owned、只读且无 symlink alias。本仓库不发布 systemd unit；唯一生命周期入口是 release
内绝对路径脚本。自建 unit 也只能调用这些入口，不能重新实现启动顺序。

## 2. 构建和首次配置

准备 lock 精确匹配的 MediaMTX `linux_amd64 v1.20.0`：

```bash
export SENTINEL_MEDIAMTX_SOURCE=/absolute/path/to/mediamtx
./native/build.sh

/opt/isarmg/sentinel-monitor/releases/0.2.0/native/bootstrap.sh
sudoedit /etc/isarmg/sentinel-monitor/sentinel-monitor.env
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/bootstrap.sh --confirm-config
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/start.sh
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/status.sh
```

停止：

```bash
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/stop.sh
```

同版本第二次 build/bootstrap 不覆盖既有 release 或环境文件。bootstrap 不启动服务、不读取旧 `.env`，
也不回显随机 JWT Secret、Credential Key 或临时管理员密码。

## 3. 核心配置

实际配置是 0600 文件，`config/sentinel-monitor.env.example` 只作字段参考。主要字段：

| 类别 | 变量 | 要求 |
|---|---|---|
| 数据 | `DATABASE_URL`、`RECORDINGS_DIR`、`SENTINEL_RUNTIME_DIR` | 使用固定外部绝对路径 |
| 身份 | `APP_JWT_SECRET` | 至少 32 字符随机值，主机秘密管理 |
| 凭据 | `CREDENTIALS_KEY` | Base64 编码的 32 字节随机值，必须备份到独立秘密系统 |
| 首管 | `BOOTSTRAP_ADMIN_EMAIL/PASSWORD` | 仅全新数据库初始化 |
| 环境 | `APP_ENV=production` | 开发模式只允许 loopback |
| Session | `SESSION_IDLE_TTL_MINUTES=30`、`SESSION_ABSOLUTE_TTL_HOURS=12` | 按风险调整 |
| 登录 | body/rate/Argon2 concurrency/timeout | 按 CPU/内存容量调整，不取消边界 |
| MediaMTX | API、playback、config、contract、binary | 必须指向同一固定 release |
| Web | `STATIC_DIR` | 必须是 release 的 `web/` 真实路径 |
| 公网 | `PUBLIC_HLS_BASE_URL`、`PUBLIC_WEBRTC_BASE_URL` | 由同源 Caddy 路由 |

MediaMTX 的 9996/9997/9998 只应在本机/受控网络可达；摄像头放入隔离 VLAN。

## 4. 当前数据库与凭据合同

`product_metadata` 必须恰好一行：

```text
application=sentinel-monitor
application_version=0.2.0
schema_revision=1
schema_sha256=b089342e00e672d6e6c679e15f331c90e599129371042a37948a4b53e5f8e49e
```

`media_reconciler_leases` 必须是当前固定结构且恰有 `singleton=1`。空闲 owner/expiry 同为 NULL；持有态
owner 是规范 UUIDv4，时间为 UTC RFC 3339 且 expiry 晚于 updated。产品不会修补非法状态。

所有敏感字段必须是当前规范 envelope，并能用当前 external key 解密。不要直接编辑数据库或复制密文
字段。

## 5. Doctor 与健康检查

```bash
set -a
source /etc/isarmg/sentinel-monitor/sentinel-monitor.env
set +a

"/opt/isarmg/sentinel-monitor/releases/0.2.0/bin/sentinel-monitor" doctor --offline
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/start.sh
"/opt/isarmg/sentinel-monitor/releases/0.2.0/bin/sentinel-monitor" doctor
```

offline 检查 Schema、SQLite integrity/foreign keys、回滚写探针、录像目录、全量凭据解密、MediaMTX
binary/version/SHA/config。在线模式再检查两个 loopback readiness。失败时先保全日志和状态，不能反复
启动掩盖 unknown operation。

## 6. 锁顺序

应用全生命周期持有数据库 instance 排他、maintenance 共享和 runtime app lock；MediaMTX 由
`flock --no-fork` 持有 companion lock。维护工具必须按 database maintenance -> runtime -> MediaMTX
取得排他锁。不要用不同 runtime 指向同一数据库；database identity lock 仍会拒绝第二实例。

## 7. 备份、恢复和升级

产品没有这些命令。停止应用与 MediaMTX 后，使用 `sarmg-upgrade` 的 Sentinel 专用组合命令同时处理：

- SQLite main/WAL/journal；
- MediaMTX config 与 lock；
- 完整 recordings tree（含空目录和摘要 inventory）；
- 外部 `CREDENTIALS_KEY` 的非秘密 key ID/Hash 要求。

原始 key 不进入备份，但恢复验证必须提供相同受保护 key。定期在隔离主机演练恢复与媒体播放。

## 8. 发布测试

```bash
bash -n native/*.sh
./native/lifecycle-test.sh
./native/relocated-smoke-test.sh
```

lifecycle test 仅使用临时根，覆盖 no-clobber、alias/旧环境拒绝、秘密不回显、失败回滚、start/stop
串行化及 symlink/hardlink 防御；relocated smoke 使用真实 Vite/Rust/SQLite，读取所有 hashed asset 并
证明篡改后拒绝重启。

## 9. 故障定位

| 现象 | 优先检查 |
|---|---|
| start 拒绝 | release manifest、权限、物理路径、MediaMTX SHA/config |
| 登录失败/循环 | 系统时钟、HTTPS、Secure Cookie、Origin/Host、限流 |
| operation 长期 pending | reconciler 日志、global/operation lease、MediaMTX API |
| operation unknown | 对照远端 actual state，禁止盲重试 |
| 无画面 | 摄像头 RTSP、publisher、JWT 时间窗、Caddy WHEP/HLS 路由 |
| doctor Schema 失败 | 停止服务，保全 generation，交给升级工具 |
| 凭据解密失败 | 确认当前 key 和 key ID；不要自动换 key 或回退旧格式 |

## 10. 安全事件

先隔离公网和摄像头 VLAN，停止扩大写入，保全数据库 generation、recordings、manifest、配置摘要、审计
与 Journal，再轮换 Session、管理员密码、JWT Secret、Credential Key、摄像头密码和 TLS 材料。不要在
公开 issue 上传数据库、录像、RTSP URL、账号、密钥或日志 Secret；使用私密漏洞报告渠道。只支持
当前发布版本与当前 `main`。
