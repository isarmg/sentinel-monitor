# Sentinel Monitor 工作流程与流程树

## 1. 总流程树

```text
Sentinel Monitor 0.2.0
├─ 构建
│  ├─ Node 26.7.0 -> check:foundation -> TypeScript strict -> Vite 7
│  ├─ Rust 1.98.0 -> x86_64-unknown-linux-gnu binary
│  └─ Web、Rust、MediaMTX、配置 -> 不可变 release manifest
├─ 启动
│  ├─ 验证固定 release + MediaMTX contract
│  ├─ 解析当前配置与 Secret
│  ├─ 取得 database/runtime/MediaMTX 锁
│  ├─ 验证当前 SQLite、Schema 与全部摄像头凭据
│  └─ 启动 MediaMTX、Rust API 和 reconciler
├─ Administrator 控制面
│  ├─ login/session/logout -> Session + CSRF
│  ├─ 摄像头期望态 -> durable operation
│  ├─ reconciler -> MediaMTX actual state
│  └─ 用户、事件、审计与 operation status
├─ 媒体面
│  ├─ 浏览器申请资源限定的短时 JWT
│  ├─ MediaMTX internal auth 回调
│  └─ WHEP 实时预览 / HLS 回放
└─ 运维
   ├─ start/status/stop/doctor
   ├─ lifecycle + relocated smoke tests
   └─ 外部升级仓处理未来代际数据操作
```

产品运行时只理解当前合同。数据库转换、历史数据导入、备份和恢复不进入 Sentinel 运行路径；这些能力
必须由单独仓库在停机、排他锁和明确输入输出合同下完成。

## 2. 原生发布与首次启动

`native/build.sh` 只接受干净且 annotated `v0.2.0` 指向 HEAD 的 checkout、Linux x86_64 builder，以及与
`config/mediamtx.lock` 匹配的 `linux_amd64` MediaMTX `v1.20.0`。Rust target 固定为
`x86_64-unknown-linux-gnu`，没有其他架构、OS 或 libc 的正式构建分支。脚本构建 Web 和 source-bound
Rust binary，生成完整 manifest，在同一文件系统暂存并验证后，以 no-clobber 语义发布固定版本目录。

`bootstrap.sh` 创建目录、0600 环境文件和初始 Administrator 配置材料，但账户行要到应用首次打开全新
数据库时才创建。它不启动服务、不覆盖配置、不回显随机 Secret。操作者
编辑密码并运行 `--confirm-config` 后，`start.sh` 才按锁顺序启动 companion 和应用；运行机同样必须是
Linux x86_64。

## 3. 正式进程启动顺序

1. 验证进程物理位置、revision、target、API、Schema、Web、credential epoch、MediaMTX 和 manifest 全树。
2. 解析环境；生产 Cookie 必须 Secure，`STATIC_DIR` 必须等于发行树 Web 目录。
3. 取得数据库 instance 排他锁和 maintenance 共享锁。
4. 取得 runtime `app.lock` 并维护 PID；MediaMTX 由脚本持有 companion lock。
5. 私有复制并验证 SQLite generation、租约 singleton 与所有加密摄像头凭据。
6. 启动后台 reconciler、HTTP 服务与 readiness；任何合同不能证明时 fail closed。

## 4. Administrator 认证流程

```text
POST /api/v2/auth/login  {username,password}
  -> candidate 为 1..64 bytes printable ASCII
  -> trim ASCII whitespace + ASCII lowercase
  -> canonical 3..64 bytes、首尾字母数字、字符仅 [a-z0-9._-]
  -> 请求体/来源/账户/全局准入
  -> Argon2 校验
  -> 写 browser_sessions 的 Session/CSRF digest
  -> Set-Cookie: __Host-sentinel_session（Secure/HttpOnly/SameSite）
  -> 返回严格 AdministratorSession

GET /api/v2/auth/session
  -> 验证 Session Cookie 与 idle/absolute TTL
  -> 轮换 CSRF token digest
  -> 返回严格 AdministratorSession

POST /api/v2/auth/logout + X-CSRF-Token
  -> 撤销 Session
  -> 过期 Session Cookie
```

`AdministratorSession` 的 wire 形状固定为
`{authenticated:true,user_id,username,role:"admin",csrf_token}`。`role` 是跨项目 wire 常量，不是数据库字段；
`users` 表不保存身份等级，也不存在运行时身份切换。除 login 外，`/api/v2` 业务路由都要求有效
Administrator Session；unsafe method 还要求当前 CSRF、Origin/Host/URI authority 边界以及单值
`Sec-Fetch-Site: same-origin`。

摄像头表中的 RTSP/ONVIF `username`、`password` 是访问设备的加密业务凭据，绝不是 Sentinel 控制面
用户、权限或 Session。二者不得共用存储、日志字段或前端状态。

这一身份变更只覆盖 Server 的 `users.username`、管理认证 API 和由 Server 发布的 React/Vite Web。
摄像头身份、MediaMTX internal auth、媒体 JWT subject/camera/actions、录像和播放流程都保持原合同；
不能机械替换摄像头表或媒体 token 中的数据面字段。

## 5. 摄像头写操作状态机

```text
HTTP camera create/update/delete
  -> 验证 Administrator Session、CSRF、Origin/Host、严格 DTO
  -> SQLite transaction: desired camera + media_operation + audit_logs
  -> 返回 camera + operation_id（删除为 202）
  -> reconciler 领取 global lease + operation lease
  -> transaction 外调用 MediaMTX
       ├─ 成功 -> actual path + succeeded
       ├─ 明确失败 -> failed + 可解释错误
       └─ 结果不可证明 -> unknown
  -> GET /api/v2/media/operations/{id}
```

只有仍同时持有未过期全局/操作租约的 owner 能 finalize。启动恢复只处理确实过期或缺失租约的 running，
不会清空健康 owner。删除 API 立即隐藏摄像头，但 MediaMTX 清理状态仍由 operation 跟踪；录像不会因删除
页面条目而被隐式擦除。

PTZ 不走上述状态机：`POST /api/v2/cameras/{id}/ptz` 同步调用 ONVIF，成功后 best-effort 写审计。当前没有
PTZ operation、通用 `Idempotency-Key` 或客户端 revision CAS；网络结果不确定时不能自动重放 move。

## 6. 漂移修复

周期任务读取期望态，在事务外读取 MediaMTX 配置、Publisher、Recording 实际态；摘要比较发现差异
后创建 `drift_detected` 操作。日志和持久错误仅包含 camera/operation ID 与固定错误码，不包含 RTSP
URL、userinfo、用户名、密码或远端错误正文。

## 7. 播放授权

```text
Administrator Browser Session
  -> /api/v2 请求单摄像头媒体授权
  -> 当前 HKDF key 签发短时 JWT
  -> Browser 访问同源 /media-webrtc 或 /media-hls
  -> Reverse proxy 转给 MediaMTX
  -> MediaMTX 调用 /internal/v2/media/auth
  -> Rust 验证 protocol、issuer、audience、kind、camera、jti 与时间
```

媒体 JWT 只能授权指定资源与用途，不能调用管理 API；Session Cookie 也不能替代媒体 JWT。

## 8. 停机与代际数据边界

`stop.sh` 通过 operations lock 串行生命周期动作，先停止应用，使数据库/reconciler/runtime 锁释放，再
停止 MediaMTX。直接 kill 或乱序停止可能把外部效果留在 unknown，应保全日志后由 reconciler/人工判断。

未来备份、恢复或升级必须在两进程停止后，由外部升级仓固定按 database maintenance、runtime、MediaMTX
顺序取得排他锁，把 SQLite、MediaMTX config/contract、recordings 和 external key 身份作为组合状态处理。
当前产品不包含迁移器、不扫描其他代路径、不解析非当前 Schema/密文，也不通过 fallback 修补数据。

## 9. Web 构建与 Foundation 0.3 流程

```text
package.json + package-lock.json 精确锁定 Foundation 0.3 和工具链
  -> npm ci
  -> check:foundation
       ├─ 校验 Node/React/TypeScript/Vite 精确版本
       ├─ 校验四个 Foundation Web 包及 lock 来源
       ├─ 校验认证 hook、运行时守卫和 data-sarmg-scope
       └─ 校验 token/reset/accessibility 内容摘要和品牌语义映射
  -> TypeScript 5.8.3 strict typecheck
  -> React 19.2.8 + ReactDOM 19.2.8
  -> Vite 7.3.6 + @vitejs/plugin-react 4.7.0 build
  -> dist/index.html + hashed assets
  -> native build 纳入 release manifest
  -> relocated smoke 验证引用、重定位与篡改拒绝
```

开发期命令：

```bash
cd clients/web
npm ci
npm run check:foundation
npm run build
```

`build` 已把 `check:foundation` 设为硬前置，因此不能通过直接执行 Vite 跳过共享边界。构建产物完全自包含；
浏览器运行时不解析 npm 包，也不访问 npm registry、Foundation 仓库或远程 CSS。
构建期四个 Web 包来自 Foundation `v0.3.0` GitHub Release 归档，并由 lockfile integrity 锁定字节；
Rust crate 由版本 `=0.3.0` 和 revision `1fe326081cfd896f05ff502e80f99504797c14c6` 双重锁定。两者都不读取
共同父目录或 sibling checkout，也不提供旧来源 fallback。

## 10. Foundation 共享层与产品层调用树

```text
clients/web/src/main.tsx
├─ @sarmg/admin-web
│  ├─ createAdministratorApiClient
│  ├─ /react: useAdministratorSession
│  ├─ /vite: createSarmgReactViteConfig
│  └─ /tsconfig.json: strict TypeScript baseline
├─ @sarmg/contracts
│  └─ auth path、AdministratorSession、ErrorEnvelope 的类型与运行时守卫
├─ @sarmg/http-client
│  └─ same-origin、Cookie、CSRF、超时、响应大小、Content-Type 与错误解析
├─ @sarmg/design-tokens
│  └─ token、scoped reset、focus/reduced-motion/forced-colors 基线
└─ Sentinel 产品代码
   ├─ 摄像头/录像/事件/审计 DTO 的运行时守卫
   ├─ 页面、WHEP/HLS 播放、交互与中文文案
   └─ 纸张/墨色/警示色、布局、组件和响应式品牌样式

Rust/Axum
├─ sarmg-contracts 0.3.0 -> Administrator 认证 DTO/路径和跨语言合同
├─ sarmg-error 0.3.0 -> 严格 ErrorEnvelope/ErrorCode
├─ sarmg-server-target 0.3.0 -> 编译期 x86_64-unknown-linux-gnu 门禁
└─ Sentinel 产品代码 -> Schema、Cookie、Session 存储、摄像头、媒体、审计与运维
```

共享层只统一真正跨产品的合同；它不拥有 Sentinel 的业务 DTO、数据库、媒体状态机、密码策略、Cookie
名称、页面或品牌。产品层也不得复制共享实现形成第二事实源。
