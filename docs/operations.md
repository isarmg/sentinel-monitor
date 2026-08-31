# Sentinel Monitor 运维文档

本产品的正式服务端构建和运行平台只有 `x86_64-unknown-linux-gnu`。Linux aarch64、musl、Windows、macOS
以及其他 target 都不属于可部署范围，也没有兼容分支。Rust 工具链固定为 `1.98.0`；Web 构建机固定为
Node `26.7.0`。

## 1. 唯一生产布局

```text
/opt/isarmg/sentinel-monitor/releases/0.2.0/
├─ RELEASE-MANIFEST
├─ bin/{sentinel-monitor,mediamtx}
├─ web/{index.html,assets/...}
├─ config/{mediamtx.yml,mediamtx.lock}
└─ native/{bootstrap,start,status,stop,common}.sh

/etc/isarmg/sentinel-monitor.env
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
sudoedit /etc/isarmg/sentinel-monitor.env
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/bootstrap.sh --confirm-config
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/start.sh
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/status.sh
```

构建机必须是 Linux x86_64，并安装 Rust `1.98.0` 的 `rustfmt`、`clippy` 组件及
`x86_64-unknown-linux-gnu` target。`native/build.sh` 显式使用该 target；`start.sh` 会再次拒绝错误运行
平台。不得用修改 manifest 字符串、跳过 target gate 或复制别的平台 binary 形成“临时支持”。

停止：

```bash
/opt/isarmg/sentinel-monitor/releases/0.2.0/native/stop.sh
```

同版本第二次 build/bootstrap 不覆盖既有 release 或环境文件。bootstrap 不启动服务，只读取固定的平面
环境路径，也不回显随机 JWT Secret、Credential Key 或临时管理员密码。

## 3. 核心配置

实际配置是 0600 文件，`config/sentinel-monitor.env.example` 只作字段参考。主要字段：

| 类别 | 变量 | 要求 |
|---|---|---|
| 数据 | `DATABASE_URL`、`RECORDINGS_DIR`、`SENTINEL_RUNTIME_DIR` | 使用固定外部绝对路径 |
| 身份 | `APP_JWT_SECRET` | 至少 32 字符随机值，主机秘密管理 |
| 凭据 | `CREDENTIALS_KEY` | Base64 编码的 32 字节随机值，必须备份到独立秘密系统 |
| 首管 | `BOOTSTRAP_ADMIN_USERNAME/PASSWORD` | 仅全新数据库初始化；默认 username 为 `admin` |
| 环境 | `APP_ENV=production` | 开发模式只允许 loopback |
| Session | `SESSION_IDLE_TTL_MINUTES=30`、`SESSION_ABSOLUTE_TTL_HOURS=12` | 按风险调整 |
| 登录 | body/rate/Argon2 concurrency/timeout | 按 CPU/内存容量调整，不取消边界 |
| MediaMTX | API、playback、config、contract、binary | 必须指向同一固定 release |
| Web | `STATIC_DIR` | 必须是 release 的 `web/` 真实路径 |
| 公网 | `PUBLIC_HLS_BASE_URL`、`PUBLIC_WEBRTC_BASE_URL` | 由同源 Caddy 路由 |

MediaMTX 的 9996/9997/9998 只应在本机/受控网络可达；摄像头放入隔离 VLAN。
仓库 `Caddyfile` 的默认站点占位为 `:80`，不是生产 TLS 证明；生产必须设置真实 `SITE_ADDRESS` 并取得
有效证书，同时用防火墙阻止浏览器直连 Axum/MediaMTX。应用本身不终止 TLS，也不拒绝所有明文直连。

## 4. 当前数据库与凭据合同

`product_metadata` 必须恰好一行：

```text
application=sentinel-monitor
application_version=0.2.0
schema_revision=1
schema_sha256=f547ddc817d830d23b5305bb1f88b29898d6531568edd6eb194c2b629eb560c0
```

`users` 表只保存管理员账户的 `id`、canonical `username`、密码摘要、启停状态、Session version 和时间
字段，不保存 email 或 `role`。username 的 Schema CHECK 精确要求 3–64 bytes、ASCII 小写、首尾
`[a-z0-9]`、其余字符仅 `[a-z0-9._-]`；唯一索引直接作用于 canonical 值。登录 candidate 可包含首尾
ASCII whitespace/大写，但 Foundation 规范化后才查询；`@`、Unicode、内部空白、控制字符和首尾分隔符
都会拒绝。所有成功认证的控制面账户都是 Administrator；禁止通过手改表或增加配置字段制造身份等级。

`media_reconciler_leases` 必须是当前固定结构且恰有 `singleton=1`。空闲 owner/expiry 同为 NULL；持有态
owner 是规范 UUIDv4，时间为 UTC RFC 3339 且 expiry 晚于 updated。产品不会修补非法状态。

所有敏感字段必须是当前规范 envelope，并能用当前 external key 解密。不要直接编辑数据库或复制密文
字段。

## 5. Doctor 与健康检查

```bash
set -a
source /etc/isarmg/sentinel-monitor.env
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

产品没有这些命令，也没有迁移入口或非当前格式 reader。停止应用与 MediaMTX 后，未来只能使用独立升级
仓库的 Sentinel 专用组合命令同时处理：

- SQLite main/WAL/journal；
- MediaMTX config 与 lock；
- 完整 recordings tree；升级仓必须生成并校验包含空目录在内的独立摘要 inventory；
- 外部 `CREDENTIALS_KEY` 的非秘密 key ID/Hash 要求。

原始 key 不进入备份，但恢复验证必须提供相同受保护 key。定期在隔离主机演练恢复与媒体播放。

## 8. 发布测试

```bash
bash -n native/*.sh
./native/lifecycle-test.sh
./native/relocated-smoke-test.sh
```

lifecycle test 仅使用临时根，覆盖 no-clobber、合同外环境拒绝、秘密不回显、失败回滚、start/stop
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
| 凭据解密失败 | 确认当前 key 和 key ID；不要自动换 key 或绕过认证 |

## 10. 安全事件

先隔离公网和摄像头 VLAN，停止扩大写入，保全数据库 generation、recordings、manifest、配置摘要、审计
与 Journal，再轮换 Session、管理员密码、JWT Secret、Credential Key、摄像头密码和 TLS 材料。不要在
公开 issue 上传数据库、录像、RTSP URL、账号、密钥或日志 Secret；使用私密漏洞报告渠道。只支持
当前发布版本与当前 `main`。

控制面认证只接受以下三个路径：

- `POST /api/v2/auth/login`；
- `GET /api/v2/auth/session`；
- `POST /api/v2/auth/logout`。

login/session 成功响应必须严格是
`{authenticated:true,user_id,username,role:"admin",csrf_token}`，登录请求必须严格为
`{username,password}`；额外字段、已删除的 email 字段、其他 `role` 值和合同外路径都视为
合同错误。wire 中固定的 `role:"admin"` 仅用于跨产品响应一致性，不对应数据库列。摄像头 RTSP/ONVIF
用户名和密码是加密设备凭据，与 Administrator 身份无关。

`BOOTSTRAP_ADMIN_USERNAME`、管理 API 和内置 Web 是 Server 范围。摄像头 username、MediaMTX internal
auth body、媒体 JWT、WHEP/HLS 播放与录像状态不使用 Administrator username；运维轮换管理 username/
密码时不能同步改摄像头凭据或媒体 key，反之亦然。

## 11. Web 设计依赖与发布证明

四个 Foundation `0.3.0` Web 包都是构建期依赖，不是生产运行服务。发布机使用 `.node-version` 固定的
Node `26.7.0`，从 lockfile 对应的精确来源安装，然后执行共享边界检查、TypeScript strict 检查和 Vite
构建：

- Web 包固定到 Foundation GitHub Release `v0.3.0` 下四个 `sarmg-<name>-0.3.0.tgz` URL；lockfile 必须
  对 `admin-web`、`contracts`、`design-tokens`、`http-client` 分别保存相同 URL、版本和 `sha512`
  integrity。
- Rust crate 固定 `https://github.com/isarmg/sarmg-foundation.git`、版本 `=0.3.0` 和完整 revision
  `1fe326081cfd896f05ff502e80f99504797c14c6`。
- 独立 checkout/CI 不读取共同父目录中的 Foundation 源码，不允许把依赖改回 `file:` 或 `path`。

```bash
cd clients/web
npm ci
npm run check:foundation
npm run build
```

`build` 自身以 `check:foundation` 为前置，不能绕开。精确工具链为 React/ReactDOM `19.2.8`、TypeScript
`5.8.3`、Vite `7.3.6`、`@vitejs/plugin-react` `4.7.0`、`@types/react` `19.2.18` 和
`@types/react-dom` `19.2.5`。成功后 Foundation token/reset/accessibility 和 Sentinel `styles.css` 已合并进 `dist/assets/*.css`。后续
`native/build.sh` 把该 Hash 文件写入发行 manifest，`relocated-smoke-test.sh` 证明实际归档引用它并在篡改
后拒绝启动。生产目录中不应出现 `node_modules`、源包、`vendor/sarmg-design` 或远程 CSS URL。

设计测试或依赖安装失败时，不要复制本地 `reset.css` 临时绕过，也不要在 `index.html` 添加 CDN。应修复
当前 Foundation 来源/lockfile，重新生成整个 Web dist 和不可变 release。Foundation 版本切换属于直接
替换当前合同，不保留并行 CSS 或媒体查询式版本 fallback。

### 11.1 Foundation 包的运维边界

| 包 | Sentinel 使用内容 | 运维必须证明 | 不由该包负责 |
|---|---|---|---|
| `@sarmg/contracts` | auth 路径、Administrator DTO、ErrorEnvelope 类型与严格守卫 | 版本/来源锁定；不可信 JSON 通过守卫；未知字段拒绝 | 摄像头、录像、事件、审计等产品 DTO |
| `@sarmg/http-client` | same-origin、Cookie、CSRF、超时、响应上限、Content-Type、错误解析 | unsafe 请求携带当前 CSRF；401 使本地 Session 失效；无跨 origin | 自动重试写操作、大文件下载、业务响应判定 |
| `@sarmg/design-tokens` | token、scoped reset、键盘焦点、reduced motion、forced colors | `data-sarmg-scope`、CSS 摘要、无 CDN/仓库运行依赖 | Sentinel 品牌、组件、布局和全部可访问性责任 |
| `@sarmg/admin-web` | API client、React Session hook、Vite 配置、strict tsconfig、精确工具链台账 | `check:foundation` 与 build 前置；固定三个 auth 端点 | 用户表、Cookie 属性、密码策略、Session 数据库和页面 |

### 11.2 Foundation Rust crate 的运维边界

| crate | 共享能力 | Sentinel 保留的产品责任 | 删除后果 |
|---|---|---|---|
| `sarmg-contracts = 0.3.0` | Administrator 路径/DTO、跨语言合同类型 | 路由挂载、密码校验、Session 持久化与业务 DTO | Rust/Web 认证合同可能静默漂移 |
| `sarmg-error = 0.3.0` | `ErrorCode`、严格 `ErrorEnvelope` | 状态码映射、脱敏文案、Retry-After 与日志诊断 | Web 无法可靠解析错误，可能泄漏产品内部结构 |
| `sarmg-server-target = 0.3.0` | 编译期唯一 server target 与 target 常量 | release 脚本、运行平台检查、MediaMTX 平台合同 | 非正式平台可能误编译或 manifest target 漂移 |

这些共享包不提供旧合同兼容。升级包版本时同时替换依赖、lockfile、代码消费者、检查和整套发行物，
不在 Sentinel 内加入双读、别名或 fallback。
