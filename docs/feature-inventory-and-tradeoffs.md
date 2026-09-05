# Sentinel Monitor 完整功能与取舍清单

本文按当前 `0.2.0` 工作树逐项盘点 Sentinel Monitor 的真实能力、保证、交付工具和明确边界。代码、
`schema/generated/current_schema.sql`、`clients/web/src/protocol-contract.json`、`config/mediamtx.lock` 与发行 manifest 是
最终事实源；本文不是未来愿望清单，也不把测试中不存在的行为写成已实现功能。

本清单帮助开发人员回答四个问题：某段代码保护什么；删除后哪条用户旅程或安全不变量会消失；删除
需要同时清理哪些消费者；变更完成至少要取得什么证据。

## 1. 阅读规则

### 1.1 分类

| 分类 | 判断标准 |
|---|---|
| 核心 | 直接构成浏览器摄像头监控、录像回放或设备管理主目标，删除后产品定位改变 |
| 保障 | 用户未必直接看到，但负责身份、机密性、一致性、资源边界或失败关闭 |
| 可选 | 只服务部分设备或部署，可在接受明确功能损失后删除 |
| 建议保留 | 不决定产品存在，但显著改善可用性、诊断或操作闭环 |
| 开发运维 | 构建、验证、安装、诊断、发布、文档和供应链能力 |

### 1.2 复杂度

| 复杂度 | 判断标准 |
|---|---|
| 低 | 单个独立入口或小范围配置，通常不改持久状态和跨组件协议 |
| 中 | 跨两个以上模块、前后端或脚本，需要成组删除和回归验证 |
| 高 | 跨协议、Schema、密码学、外部系统、持久状态或发行闭包，不能只删按钮或路由 |

### 1.3 身份边界

Sentinel 控制面只有 Administrator 一种身份。`users` 表没有 `role` 列，登录成功的 wire response 固定
`role:"admin"`，身份键为 canonical `users.username`；创建、停用、删除用户只是在管理 Administrator
账号，不会产生 observer、operator 或
viewer。摄像头的 RTSP/ONVIF `username`、加密 `password` 和媒体 JWT `actions` 都是数据面凭据或资源
授权，不是控制面角色。相同 username 文本不会把摄像头身份与 Administrator 关联。

### 1.4 删除闭包

删除任一功能时至少检查：Rust 路由与 DTO、SQLite DDL/查询、React 页面与运行时 guard、MediaMTX 配置、
原生生命周期脚本、配置样例、发行 manifest、正反测试和本套中文文档。隐藏 React 按钮不等于删除功能；
只删数据库列也不等于删除协议。

## 2. 总体定位、平台与架构

| ID | 当前功能/特性与真实行为 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 最低验证/边界 |
|---|---|---|---|---|---|---|
| SEN-P-001 | 浏览器摄像头监控产品：Rust 控制面管理摄像头和授权，MediaMTX 承担 RTSP、WHEP、HLS 与录像 | `src/routes.rs`、`src/mediamtx.rs`、`config/mediamtx.yml` | 核心 | 高 | 删除任一主组件都会失去控制面或媒体面，项目不再完整 | 摄像头添加、直播、录像、重启链路 |
| SEN-P-002 | Server 开发、测试、正式编译目标唯一为 `x86_64-unknown-linux-gnu` | `sarmg-server-target`、`build.rs`、`rust-toolchain.toml` | 保障 | 高 | 放宽后会产生未经验证的平台二进制，且可能与 companion 平台失配 | 非目标编译必须失败；目标常量与 Cargo target 一致 |
| SEN-P-003 | 正式运行主机唯一为 Linux AMD64；原生脚本再次核对 `uname` | `native/common.sh`、`native/build.sh`、`native/start.sh` | 保障 | 中 | 错误架构可能走到状态创建或 companion 启动后才失败 | Linux x86_64 正例；aarch64/非 Linux 负例 |
| SEN-P-004 | MediaMTX companion 固定为 `v1.20.0 linux_amd64` 和精确 SHA-256 | `config/mediamtx.lock`、`native/build.sh`、`native/start.sh` | 保障 | 高 | API、配置和媒体行为不可复现，发行身份失去意义 | version 输出、platform、binary SHA 三者同时匹配 |
| SEN-P-005 | 控制面和媒体面分离；Rust 不代理 RTSP 输入，也不转码视频 | `src/mediamtx.rs`、`deploy/Caddyfile` | 核心 | 高 | 把媒体搬入 Rust 会重写容量、协议和攻击面；删 companion 则无直播/录像 | Rust 路由不存在 RTSP 转发；MediaMTX path 实测 |
| SEN-P-006 | 当前版本唯一合同；产品不内置数据迁移、备份或恢复命令 | `src/main.rs` CLI、`src/sqlite.rs` | 保障 | 高 | 加入代际 reader 会长期扩大状态和测试矩阵 | CLI 只有 serve/doctor/release 类命令；非当前库零写入拒绝 |
| SEN-P-007 | Server 端 React 19 + TypeScript strict + Vite 7 控制台位于 `clients/web/` | `clients/web/package.json`、`clients/web/src/main.tsx` | 建议保留 | 高 | API 和媒体能力仍在，但没有内置可操作控制台 | typecheck、Vite build、发行静态树验证 |
| SEN-P-008 | Foundation 是唯一上游平台；当前 Rust/Web 为工作区联调来源，新不可变发行版与独立 checkout 尚待 P13 | Cargo、八个 @sarmg 包、manifest/lock | 保障 | 高 | 第二套平台实现导致漂移；联调路径不能作为独立发行证明 | 当前构建/门禁；P13 统一来源和独立验收 |
| SEN-P-009 | `config/` 只存可提交样例和受审 companion 合同；真实 Secret 不进仓库 | `config/sentinel-monitor.env.example`、`.gitignore` | 开发运维 | 低 | Secret 容易误提交，或部署字段缺少审查入口 | Secret 扫描；样例字段与 parser 对照 |
| SEN-P-010 | 本仓库刻意不发布 systemd unit，生命周期只由 release 内 `native/*.sh` 实现 | `native/bootstrap.sh`、`start.sh`、`status.sh`、`stop.sh` | 开发运维 | 中 | 运维方需自行重建锁序和失败回滚，容易启动半套服务 | 生命周期测试；发行树中无 unit；脚本可重定位 |

## 3. 配置、启动和生命周期

| ID | 当前功能/特性与真实行为 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 最低验证/边界 |
|---|---|---|---|---|---|---|
| SEN-C-001 | 生产环境文件固定为 `/etc/isarmg/sentinel-monitor.env`，而不是项目子目录 | `native/common.sh`、`config/*.env.example` | 开发运维 | 中 | 多路径来源会造成操作者修改无效文件或混合配置 | bootstrap、start、文档和测试使用同一平面路径 |
| SEN-C-002 | `bootstrap.sh` 排他创建配置、状态和运行目录，不覆盖既有环境文件 | `native/bootstrap.sh` | 保障 | 高 | 重跑初始化可能无提示覆盖 Secret 或状态定位 | 首次创建、第二次 no-clobber、symlink 负例 |
| SEN-C-003 | bootstrap 写入默认 canonical `BOOTSTRAP_ADMIN_USERNAME=admin`，随机生成 JWT Secret、32 字节 credential key 和初始密码且不回显 | `native/bootstrap.sh` | 保障 | 中 | 终端、CI 日志或 shell history 可能泄漏高价值 Secret，或首管身份与 Server parser 漂移 | lifecycle 日志中搜索 Secret；username 精确；文件 mode 0600 |
| SEN-C-004 | 人工确认标记阻止未审阅初始 Secret 直接启动 | `sentinel-monitor.REVIEW-SECRETS-BEFORE-START`、`bootstrap.sh` | 保障 | 低 | 默认凭据可能直接进入运行环境 | 未确认 start 失败；`--confirm-config` 后标记消失 |
| SEN-C-005 | `BIND_ADDR` 缺省值和正式模板均为 `127.0.0.1:8080`；`APP_ENV=development` 进一步禁止显式外部绑定；生产使用 Secure `__Host-` Cookie | `src/config.rs`、`config/sentinel-monitor.env.example`、`native/bootstrap.sh`、`src/auth.rs` | 保障 | 中 | 默认监听任意网卡会绕开 TLS 网关；非 Secure 开发 Cookie 可能暴露到局域网 | 缺省 loopback、IPv4/IPv6 loopback正例；development 外部地址负例；正式样例一致 |
| SEN-C-006 | `APP_JWT_SECRET` 至少 32 bytes，`CREDENTIALS_KEY` 必须标准 Base64 且解码恰为 32 bytes | `src/config.rs` | 保障 | 中 | 弱密钥或歧义 key 长度会降低媒体授权和凭据保护 | 缺失、短值、非法 Base64、31/33 bytes 负例 |
| SEN-C-007 | `SENTINEL_RUNTIME_DIR` 与 `STATIC_DIR` 必须绝对路径 | `src/config.rs` | 保障 | 低 | cwd 变化会把锁或前端指到不同位置 | 相对路径拒绝；绝对路径接受 |
| SEN-C-008 | 登录 body、bucket 容量、来源/账户窗口、Argon2 并发与超时均有范围 | `src/config.rs`、`src/login_security.rs` | 保障 | 中 | 错误配置可能关闭限流或耗尽 CPU/内存 | 最小/最大/越界值；超时后许可回收 |
| SEN-C-009 | Media token TTL、状态刷新、reconcile 周期、上游请求超时均显式配置 | `src/config.rs` | 建议保留 | 中 | 删除可调性会把不同网络/规模强行绑定同一节奏 | 0/极端值行为；周期任务不重叠失控 |
| SEN-C-010 | ONVIF discovery timeout 和上报 XAddr CIDR allowlist 可配置 | `ONVIF_DISCOVERY_TIMEOUT_MS`、`ONVIF_XADDR_ALLOWLIST` | 保障 | 中 | 发现可能长时间阻塞或请求不受信地址 | CIDR 解析、超时、allowlist 正反例 |
| SEN-C-011 | 正式 `serve-release` 要求 `STATIC_DIR` 等于已验证发行根的 `web/` | `src/main.rs` | 保障 | 中 | 可把已验证 Rust 与任意前端混搭 | 同发行路径正例；外部静态目录负例 |
| SEN-C-012 | start 按 companion→readiness→应用顺序启动；任一步失败会清理本次启动的进程 | `native/start.sh` | 保障 | 高 | 失败可能遗留孤儿 MediaMTX 或错误 PID 文件 | companion 失败、应用失败、并发 start、回滚 |
| SEN-C-013 | stop 先停应用，再停 MediaMTX，遵循数据库/协调器先释放的顺序 | `native/stop.sh` | 保障 | 中 | 先停媒体面会扩大 operation 结果不确定窗口 | 正常停止、重复停止、PID 身份不匹配 |
| SEN-C-014 | PID 文件和进程可执行路径共同校验，不只信任 PID 数字 | `native/common.sh` | 保障 | 中 | PID 重用可能误杀无关进程或误报运行状态 | 伪 PID、已退出 PID、不同 executable 负例 |

## 4. Administrator 认证与请求安全

| ID | 当前功能/特性与真实行为 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 最低验证/边界 |
|---|---|---|---|---|---|---|
| SEN-A-001 | 所有控制面账户都是 Administrator；平台表不持久化角色；管理员 ID 为不透明 TEXT，不要求 UUID | Foundation admin-core/admin-sqlite、生成 Schema | 核心 | 高 | 失去认证或业务外键错误绑定 | 当前 DDL、真实 bootstrap ID、业务外键回归 |
| SEN-A-002 | 三个认证路径精确为 `/api/v2/auth/login`、`/api/v2/auth/session`、`/api/v2/auth/logout` | Foundation constants、`src/routes.rs` | 保障 | 中 | 路径漂移会让共享客户端失效，别名会形成额外攻击面 | 三路径方法矩阵；其他形状 404/405 |
| SEN-A-003 | 登录 body 精确为 `{username,password}`，未知字段与已删除的 email 字段由 DTO 拒绝；Session 精确为 `{authenticated,user_id,username,role:"admin",csrf_token}` | `AdministratorLoginRequest`、`AdministratorSession`、Axum `Json` | 保障 | 低 | 歧义输入可能在 Server、Foundation 和 Web 产生不同解释 | 缺失、额外、类型错误、超限 body、Session exact keys |
| SEN-A-004 | username 使用 Foundation 唯一规则：candidate 1..64 bytes printable ASCII，经 trim ASCII/lowercase 后 canonical 必须为 3..64 bytes、首尾字母数字、字符仅 `[a-z0-9._-]`；Schema 保存同一 canonical 形状 | `normalize_administrator_username`、`require_canonical_administrator_username`、`current_schema.sql` | 保障 | 中 | 同一管理员可用变体绕过唯一约束/限流，或跨产品身份语义不一致 | 大写/首尾空白正例；`@`、Unicode、内部空白、控制字符、首尾分隔符负例 |
| SEN-A-005 | 密码只接受 Foundation 当前策略和精确 Argon2id hash 参数 | `sarmg-admin-auth`、`src/auth.rs` | 保障 | 高 | 放宽 hash 会形成多策略验证分支；弱 hash 降低离线攻击成本 | 当前 PHC 正例；参数、版本、salt/output 偏差拒绝 |
| SEN-A-006 | 未知账户使用当前 dummy hash，减少账户枚举时序差异 | Foundation AdministratorService | 保障 | 中 | 未知 username 明显更快返回 | 已知错误密码与未知账户成本 |
| SEN-A-007 | 登录按来源 IP 与 canonical username 分别限流，全局 bucket 有界 | Foundation AdministratorService | 保障 | 高 | 暴力猜测或耗尽认证资源 | 规范化、窗口恢复、有界容量、429 |
| SEN-A-008 | Argon2 计算使用共享 semaphore 和等待预算 | Foundation AdministratorService | 保障 | 高 | blocking worker 耗尽 | 许可上限、超时、失败释放 |
| SEN-A-009 | Session token 为 32 随机字节，平台库仅保存 SHA-256 digest | Foundation _sarmg_admin_sessions | 保障 | 高 | 明文库可转为活跃登录凭据 | token/digest 形状、无明文 |
| SEN-A-010 | Session 具有固定 idle/absolute TTL，平台节流刷新 last_seen | Foundation authenticate_session | 保障 | 高 | 会话永久存活或写入过密 | 过期、刷新预算、CSRF 比较更新、时间不倒退 |
| SEN-A-011 | 改密/停用增加 session_version，并原子撤销该账户全部 Session | Foundation manage_administrator | 保障 | 高 | 旧会话继续控制设备 | 改密、停用、审计回滚、失效 Cookie |
| SEN-A-012 | 生产 Cookie 为 __Host-sarmg-sentinel-monitor-session，Secure/HttpOnly/SameSite=Strict/Path=/ | Foundation admin-core/admin-axum | 保障 | 低 | 窃取与跨站风险扩大 | Set-Cookie 精确属性、开发 Cookie |
| SEN-A-013 | logout 撤销 Session、提交平台安全审计并过期 Cookie | Foundation AdministratorService/admin-axum | 建议保留 | 低 | 无法主动结束会话 | 注销后 401、Cookie 清理 |
| SEN-A-014 | 恢复 Session 以 CAS 轮换 CSRF 摘要，迟到的 restore/touch 不能恢复旧摘要 | Foundation rotate_session_csrf | 保障 | 高 | CSRF 轮换可被并发请求撤销 | SQLite/Static CAS、旧/新摘要、错误映射 |
| SEN-A-015 | unsafe 请求要求单个 `X-CSRF-Token` 且 constant-time 比较 digest | `enforce_browser_security`、Foundation helper | 保障 | 高 | 已登录浏览器可能被跨站触发控制动作 | 缺失、重复、逗号合并、错误、正确 token |
| SEN-A-016 | 浏览器请求要求严格同源 Origin/Host/URI authority 与 `Sec-Fetch-Site: same-origin` | `require_administrator_same_origin`、`src/auth.rs` | 保障 | 高 | 代理歧义或跨站请求可能绕过 CSRF 边界 | HTTP/1 Host、HTTP/2 authority、重复头、cross-site |
| SEN-A-017 | 认证、业务和路由 rejection 使用 Foundation `ErrorEnvelope` | `src/error.rs`、`sarmg-error` | 保障 | 中 | Web 无法稳定按 code/retryable 处理，内部错误可能泄漏 | 400/401/403/404/409/429/500 exact envelope |
| SEN-A-018 | Foundation 管理接口支持创建、列表、改密和停用；无物理删除、改名或重新启用；最后一个 active 账户不能停用 | /api/v2/platform/administrators、事务内授权 | 保障 | 高 | 无账号可登录或授权快照竞态 | 并发相互停用、过期会话、CSRF 轮换 |
| SEN-A-019 | 管理员写入与安全审计同事务；密码/停用包含会话撤销审计；成功登录与 Session 创建审计也原子提交 | Foundation admin-sqlite | 保障 | 高 | 状态与审计分叉 | 审计故障回滚、actor/subject/request ID，无凭据泄漏 |

## 5. 摄像头、凭据与 ONVIF

| ID | 当前功能/特性与真实行为 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 最低验证/边界 |
|---|---|---|---|---|---|---|
| SEN-K-001 | 摄像头 list/create/update/soft-delete；删除后不再出现在普通查询 | `src/routes.rs`、`cameras.deleted_at` | 核心 | 高 | 无法维护受管设备清单 | CRUD、404、删除 operation、重启 |
| SEN-K-002 | 主流必须是 `rtsp://` 或 `rtsps://` 且有 host；子流可选 | `validate_rtsp` | 保障 | 中 | 任意 URL 会传入 companion，错误更晚且可能扩大出站面 | scheme、host、空值组合 |
| SEN-K-003 | ONVIF URL 使用独立 HTTP URL/地址策略验证 | `onvif::validate_configured_url` | 保障 | 高 | 可形成 SSRF 到本机、link-local 或非预期目标 | DNS、多地址、private/registered、allowlist |
| SEN-K-004 | 主/子 RTSP URL、用户名、密码以 AES-256-GCM envelope 存库 | `src/crypto.rs`、`cameras.*_enc` | 保障 | 高 | 改成明文会扩大数据库泄漏；删字段则无法连接需认证摄像头 | 加解密、库中无明文、重启 |
| SEN-K-005 | HKDF-SHA256 派生 credential key，AAD 绑定产品、版本、camera UUID 和字段 | `SecretBox`、`credential_aad` | 保障 | 高 | 密文可被跨记录或跨字段搬移而不报错 | 错 camera、错 field、错 key、篡改 tag |
| SEN-K-006 | envelope 严格拒绝未知字段、非规范 Base64、错误 revision/key ID | `CredentialEnvelope`、`decode_canonical_base64` | 保障 | 高 | 宽松解析会形成隐含多格式支持和歧义 | 缺失/额外字段、padding、revision、key ID |
| SEN-K-007 | 启动、readiness、system status 和 doctor 会认证全部持久凭据 | `validate_stored_camera_credentials`、`doctor.rs` | 保障 | 高 | 错 key 或坏密文直到访问单摄像头才暴露 | 任一字段损坏使整体检查失败且不改库 |
| SEN-K-008 | 对浏览器只返回是否有子流、是否配置 ONVIF 和可选 username，不返回流 URL/密码 | `CameraView::from_record` | 保障 | 中 | 管理 API 泄漏内网拓扑或设备 Secret | JSON keys 负例；浏览器网络记录 |
| SEN-K-009 | WS-Discovery 搜索 ONVIF 设备，返回候选 XAddr，不自动写库 | `onvif::discover`、`/discovery/onvif` | 可选 | 中 | 仍可手工录入，但部署发现成本上升 | 超时、重复候选、无设备、未认证拒绝 |
| SEN-K-010 | ONVIF XML 有节点、深度、文本、响应大小和 XAddr 数量上限 | `src/onvif.rs` 常量 | 保障 | 高 | 恶意或损坏设备可消耗内存/CPU | 超深、超大、过多地址、格式错误 XML |
| SEN-K-011 | PTZ 只接受 move/stop，pan/tilt/zoom 各在 `[-1,1]` | `PtzRequest`、`routes::ptz` | 可选 | 中 | 删除后仍可监看但不能从控制台云台控制 | 边界值、错误动作、无 ONVIF 配置 |
| SEN-K-012 | PTZ 是同步 ONVIF 调用并在成功后 best-effort 审计；当前不使用 durable operation | `routes::ptz`、`onvif::ptz` | 可选 | 高 | 若误删超时/错误分类，会产生长挂；若声称 durable 会误导重试策略 | 明确成功、超时、响应不可解析；文档不得宣称可恢复 operation |

## 6. 期望态、持久操作与协调器

| ID | 当前功能/特性与真实行为 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 最低验证/边界 |
|---|---|---|---|---|---|---|
| SEN-O-001 | 摄像头 create/update/delete 在同一 SQLite 事务写 desired state、operation 与审计 | `queue_camera_change`、`write_audit_in` | 核心 | 高 | HTTP 成功后没有可恢复的 MediaMTX 意图，或审计与意图分离 | 事务失败零部分写；返回 operation ID |
| SEN-O-002 | 每摄像头 desired `generation` 单调增长，operation 绑定 generation | `media_desired_states`、`media_operations` | 保障 | 高 | 陈旧 worker 可能覆盖更新后的期望状态 | 连续变更、旧 generation 完成、删除后更新 |
| SEN-O-003 | operation 状态包含 pending/running/succeeded/failed/unknown/dead_letter/resolved | `src/current_schema.sql`、`MediaOperationView` | 核心 | 高 | 无法区分可重试失败、成功和外部效果不确定 | 合法转换、非法组合、终态字段 |
| SEN-O-004 | 活跃 generation 唯一索引避免同一摄像头同代重复 active operation | `media_operations_active_generation_idx` | 保障 | 高 | 同一期望态可能被多次下发 | 并发 queue；相同 generation 冲突 |
| SEN-O-005 | 全局 singleton lease 保证同一时刻只有一个 reconciler owner | `media_reconciler_leases`、`acquire_reconciler_lease` | 保障 | 高 | 多 worker 可同时操作 companion | 双 claim、过期接管、健康 owner 不被抢占 |
| SEN-O-006 | 每条 running operation 也有 lease owner/expiry，并在远端调用前后续租 | `claim_next_operation`、`renew_claimed_leases` | 保障 | 高 | 失去 ownership 的 worker 仍可能 finalize | 过期、慢调用、owner mismatch、fencing |
| SEN-O-007 | 启动只把 lease 已过期的 running operation标为 unknown，保留健康 lease | `recover_interrupted_operations` | 保障 | 高 | 全量改写会破坏仍在工作的实例事实；完全不恢复会永久卡住 | 活跃/过期两组 fixture；无副作用验证 |
| SEN-O-008 | 对上游明确 HTTP 失败与无法证明响应分别分类 failed/unknown | `AppError::Upstream`、`UpstreamUnknown`、`sanitized_failure` | 保障 | 高 | 网络断线会被误报“失败”并诱发重复副作用 | timeout、连接断、明确 4xx/5xx、解析失败 |
| SEN-O-009 | retry 有 attempt、max_attempts、`retry_at` 和有界退避；不可安全重试进入终态 | `retry_delay`、`finish_failure` | 保障 | 高 | 远端故障会热循环或永久不再收敛 | attempt 边界、时间推进、dead_letter |
| SEN-O-010 | superseded operation 明确收口，不执行已被新 generation 取代的意图 | `finish_superseded` | 保障 | 高 | 快速连改会下发过时配置 | 连续更新/删除、队列次序、审计状态 |
| SEN-O-011 | reconciler 按 desired state 新增/更新/删除 main 与可选 sub path | `apply_desired`、`MediaMtxClient::upsert_path/delete_path` | 核心 | 高 | 数据库配置不再作用于真实媒体服务 | 主/子流、enable、record flag、delete |
| SEN-O-012 | source digest 比较避免在日志/持久观察状态中保存完整带凭据 RTSP URL | `source_digest`、`media_applied_paths` | 保障 | 中 | 漂移检测可能泄漏 Secret 或无法比较配置 | digest 变化、相同 source、日志脱敏 |
| SEN-O-013 | 周期比较 expected 与 MediaMTX path config/actual publisher/recording 并排队 drift operation | `observe_and_schedule_drift` | 建议保留 | 高 | 外部手改或 companion 重启后持续漂移 | 缺 path、错误 source/record、已一致不重复排队 |
| SEN-O-014 | operation 查询 API 返回持久状态，浏览器在摄像头保存后轮询 | `/media/operations/{id}`、`main.tsx` | 建议保留 | 中 | 页面只能显示“请求已接收”，无法知道最终收敛结果 | pending→终态、unknown 展示、404 |
| SEN-O-015 | 当前没有通用请求 Idempotency-Key 或客户端 revision CAS；幂等来自 generation/唯一索引和 desired-state 收敛 | `queue_camera_change`、Schema 索引 | 保障 | 高 | 误以为有 header 级幂等会导致调用方不安全重放 | 文档/API 不声明不存在的 header；并发测试按实际 generation 语义 |

## 7. MediaMTX、直播、录像和媒体授权

| ID | 当前功能/特性与真实行为 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 最低验证/边界 |
|---|---|---|---|---|---|---|
| SEN-M-001 | stream ticket 只为已启用摄像头的 main/sub profile 签发 | `/cameras/{id}/stream-ticket` | 核心 | 高 | 浏览器无法获得受限媒体入口；放宽则可读不存在/停用资源 | main/sub、无子流、disabled、未知 camera |
| SEN-M-002 | 媒体 JWT 使用 HS256 key，经 HKDF 从 `APP_JWT_SECRET` 派生 | `issue_media_token`、`media_signing_key` | 保障 | 高 | 直接复用根 Secret 或弱 key 会扩大泄漏影响 | key 派生确定性、错 Secret、算法固定 |
| SEN-M-003 | JWT 严格绑定 protocol、issuer、audience、kind、subject、camera、path、actions、jti、iat/nbf/exp | `MediaClaims`、`decode_media_token` | 保障 | 高 | Token 可跨产品、跨摄像头、跨用途重放 | 每字段篡改、未知字段、时间窗、jti |
| SEN-M-004 | MediaMTX HTTP auth callback 有 4 KiB/字段上限并核对 path 与 action | `/internal/v2/media/auth`、`MediaAuthRequest` | 保障 | 高 | callback 可被超大字段耗尽，或 Token 越权到其他 path | 超限、错 path/action、过期、额外字段 |
| SEN-M-005 | WHEP 浏览器播放器生成 recvonly offer、等待 ICE、设置 answer 并 DELETE resource | `clients/web/src/whep.ts` | 核心 | 高 | 失去低延迟直播或遗留服务端 WHEP Session | 成功连接、12 秒 timeout、close、unmount |
| SEN-M-006 | WHEP OPTIONS/POST 与资源 DELETE 携带短时 Bearer；ticket runtime guard 当前只验证 URL 是字符串 | `WhepPlayer`、`isStreamTicket` | 保障 | 高 | 媒体入口可能未授权或把 Token 发往意外 origin | 生产 `PUBLIC_WEBRTC_BASE_URL` 必须保持同源相对路径；当前播放器接受绝对 ticket/Location 且会携带 Bearer，尚无 same-origin 强制 |
| SEN-M-007 | 录像列表通过 MediaMTX playback API，查询可选 start/end 和指定 camera/profile | `list_recordings`、`MediaMtxClient::recordings` | 建议保留 | 高 | 直播保留，但无法定位历史片段 | 时间范围、main/sub、无录像、上游错误 |
| SEN-M-008 | 录像播放只允许 mp4/fmp4，单次 0.1 秒至 6 小时 | `play_recording` | 保障 | 中 | 无边界请求可放大上游和带宽资源消耗 | duration 边界、format、非法时间 |
| SEN-M-009 | 播放代理只转发 Content-Type/Length/Range/Disposition 白名单响应头并流式正文 | `play_recording` | 保障 | 高 | 全量透传上游头可能改变安全策略；整段缓冲会耗内存 | 200/206、Range、上游错误、大正文 |
| SEN-M-010 | MediaMTX 录制 fMP4、15 分钟 segment、默认保留 168 小时 | `config/mediamtx.yml` | 建议保留 | 中 | 删除 record 失去历史回放；改保留期直接改变容量需求 | config lock、record path、过期清理实测 |
| SEN-M-011 | start 通过环境把录像根固定到 `/var/lib/isarmg/sentinel-monitor/recordings` | `MTX_PATHDEFAULTS_RECORDPATH`、`native/start.sh` | 保障 | 中 | inert 样例路径或 cwd 可能成为真实写入位置 | 进程环境、路径权限、release relocation |
| SEN-M-012 | Caddy 将 `/media-webrtc/*`、`/media-hls/*` 与应用汇聚到一个浏览器 origin；三个上游精确为本机 `127.0.0.1:8889/8888/8080`，不支持容器 DNS 别名 | `deploy/Caddyfile`、CI proxy gate | 保障 | 中 | 跨 origin 会复杂化 Cookie、CORS 和媒体授权；容器名在当前原生部署中无法解析 | 根级副本缺失；WHEP/HLS/API 同源；管理端口不公网暴露；拒绝 `app:`/`mediamtx:`；生产设置真实 `SITE_ADDRESS` |
| SEN-M-013 | MediaMTX API、metrics、playback 默认绑定 loopback；摄像头网络另行隔离 | `config/mediamtx.yml` | 保障 | 高 | 管理 API 公网暴露可让攻击者改 path 或读取内部状态 | 监听地址、防火墙、代理路由扫描 |

## 8. 事件、状态与审计

| ID | 当前功能/特性与真实行为 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 最低验证/边界 |
|---|---|---|---|---|---|---|
| SEN-E-001 | 后台周期读取 MediaMTX path，将摄像头状态更新为 online/offline/disabled | `background::refresh_statuses` | 核心 | 高 | UI 状态长期停留 pending，无法判断设备可用性 | publisher 出现/消失、disabled、last_seen |
| SEN-E-002 | 状态变化写 `events`，severity 只允许 info/warning/critical | `background::emit_event`、Schema CHECK | 建议保留 | 中 | 设备上下线没有持久事件记录 | online/offline、pending 抑制、非法 severity |
| SEN-E-003 | 事件列表支持 camera、仅未确认和 1–500 条 limit | `EventQuery`、`list_events` | 建议保留 | 中 | 大量事件无法按当前需求过滤，或响应无界 | filters、limit clamp、排序 |
| SEN-E-004 | 事件确认记录时间和 Administrator ID | `ack_event`、`acknowledged_*` | 建议保留 | 中 | 告警无法形成最小人工闭环 | 不存在 ID、重复确认、CSRF、账号删除后的 FK |
| SEN-E-005 | SSE 只作实时通知，SQLite 是事实源；lagged 时发送 `resync-required` 后断开 | `event_stream`、broadcast channel | 保障 | 高 | 静默跳过会让页面误以为事件完整；删 SSE 则只能轮询 | 正常事件、lag、关闭、重新全量查询 |
| SEN-E-006 | 审计表记录用户、动作、实体、细节和时间；查询最多 500 条 | `audit_logs`、`list_audit` | 建议保留 | 中 | 敏感变更追溯能力下降 | create/update/delete/PTZ/login；limit clamp |
| SEN-E-007 | 摄像头/用户持久变更审计与业务同事务；PTZ/登录审计当前 best-effort | `write_audit_in`、`write_audit` | 保障 | 高 | 若把两类语义混同，运维会错误承诺审计不丢 | DB 故障注入；两类语义分别说明 |
| SEN-E-008 | 当前没有审计 outbox、独立 sink 或后台重投表 | Schema 与生产模块不存在该表/worker | 核心 | 高 | 若未来需要“必达外部审计”，必须新增状态机，不能把当前表描述为 outbox | 文档、Schema 和代码搜索一致 |
| SEN-E-009 | system status 汇总数据库、MediaMTX 与摄像头 total/online/recording | `/system/status` | 建议保留 | 低 | 控制台缺少一页式运行概况 | companion 不可达、坏 credential、空设备 |

## 9. React/Vite 管理 Web

| ID | 当前功能/特性与真实行为 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 最低验证/边界 |
|---|---|---|---|---|---|---|
| SEN-W-001 | 共享 Shell 负责登录、恢复、退出、导航、主题、诊断、通知和安全错误；产品只传身份和业务页面 | createSarmgAdminApplication | 保障 | 高 | 产品复制平台状态机 | Foundation 10 项浏览器验收及消费者浏览器回归 |
| SEN-W-002 | 页面只在内存持有 Session/CSRF；Cookie 由浏览器 HttpOnly 管理 | `@sarmg/admin-web`、`@sarmg/http-client` | 保障 | 高 | 把 Secret 放 local/sessionStorage 会扩大 XSS 泄漏 | storage 扫描、刷新、401 清理 |
| SEN-W-003 | 所有业务响应经过 TypeScript runtime guard 检查必需字段/类型，不只依赖静态类型 | `clients/web/src/api.ts` | 保障 | 高 | 异常或漂移 JSON 会在组件深处被错误使用 | 缺失/错误类型、数组成员；产品 guard 当前容忍额外响应字段 |
| SEN-W-004 | Camera 页面支持搜索、分页、添加、编辑、删除和卡片直播 | `CameraView`、`CameraEditor` | 核心 | 高 | 失去主要管理旅程 | 空态、搜索、翻页、mutation operation |
| SEN-W-005 | 详情使用共享 Dialog，主码流及鼠标/键盘 PTZ；move/stop 串行，松开、取消、失焦及关闭均触发停止 | CameraDrawer | 可选 | 中 | 缺少精细控制或停止竞态 | pointer cancel、Space/Enter、窗口 blur、关闭清理 |
| SEN-W-006 | Recordings 页面按摄像头和时间范围查询并播放 | `RecordingsView` | 建议保留 | 中 | API 尚在但普通用户难以回放 | 无摄像头、无结果、播放 URL 清理 |
| SEN-W-007 | Events 页面筛选未确认、手动刷新和确认事件 | `EventsView`、SSE effect | 建议保留 | 中 | 事件 API 无内置操作界面 | SSE resync、确认、camera name 映射 |
| SEN-W-008 | 系统页组合媒体状态、共享 AdministratorsPanel 与业务审计；不请求 /users、不保留 UserEditor | SystemView、Foundation admin-shell | 建议保留 | 高 | 账号管理或业务状态缺失 | 创建/列表/改密/停用、最后管理员保护、业务审计独立 |
| SEN-W-009 | Foundation design tokens、scoped reset、focus/reduced-motion/forced-colors 基线 | CSS imports、`data-sarmg-scope` | 保障 | 中 | 基础交互和可访问性在项目间漂移 | CSS 摘要、键盘焦点、减弱动态、高对比度 |
| SEN-W-010 | 产品 CSS 仅维护业务布局，颜色/字体/控件来自 Foundation；视频黑底属于媒体业务 | clients/web/src/styles.css | 建议保留 | 中 | 私有平台样式导致主题和可访问性漂移 | 无 token 覆盖、无私有字体、移动明暗主题 WCAG AA |
| SEN-W-011 | WHEP player 在 component cleanup、profile/camera 变化时关闭 peer/resource | `LiveVideo` effect、`WhepPlayer.close` | 保障 | 高 | 切页后仍保留媒体连接和资源 | mount/unmount、快速切换、失败重试 |
| SEN-W-012 | 精确 Node 26.7.0、React/DOM 19.2.8、TS 5.8.3、Vite 7.3.6 工具链 | `.node-version`、`package.json`、lockfile | 开发运维 | 中 | CI/开发/发行 bundle 不可复现 | clean `npm ci`、engine、lock 来源、typecheck |
| SEN-W-013 | `build` 强制先执行 `check:foundation`，再 strict typecheck 与 Vite build | `package.json`、`tests/design-foundation.test.mjs` | 开发运维 | 中 | 共享依赖或 CSS 漂移时仍可能生成表面可用 bundle | 故意改版本/import/scope 后 build 在 bundling 前失败 |
| SEN-W-014 | 发行只包含自包含 `web/index.html` 和 hashed assets，不需要运行时 npm/CDN | Vite output、`native/build.sh`、release manifest | 保障 | 高 | 运行时网络依赖会破坏离线部署和制品身份 | 断网加载、资源引用、额外文件/篡改拒绝 |

## 10. SQLite、锁、doctor、发行与供应链

| ID | 当前功能/特性与真实行为 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 最低验证/边界 |
|---|---|---|---|---|---|---|
| SEN-R-001 | 主文件不存在时只创建当前 Schema；已有空文件或非当前库拒绝 | `sqlite::prepare_current_database` | 保障 | 高 | 自动补表会把未知状态变成不可审计混合状态 | 不存在、空文件、错 application/version/revision/SHA |
| SEN-R-002 | Foundation `sarmg-schema-identity` 统一验证 `product_metadata` DDL/完整列形状、exact identity 与现场 `sqlite_schema` fingerprint；Sentinel 只实现严格 rusqlite adapter | `validate_current_connection`、`product_metadata_rows`、`schema_rows` | 保障 | 高 | 手改 metadata 可伪装结构，DDL drift 被忽略，或多产品 fingerprint 算法分叉 | metadata 0/2 行、负 revision、错误 storage class/default/列、额外表/索引、只改 SHA、WAL 只读拒绝 |
| SEN-R-003 | 验证前从 main/WAL/journal 复制私有 generation，并复核源文件身份未变 | `snapshot_generation` | 保障 | 高 | 验证可能读到跨时刻混合字节，或在源库上产生写入 | WAL、并发变化、symlink、generation cleanup |
| SEN-R-004 | SQLite integrity、foreign key 和 rollback write probe 用于 doctor | `doctor.rs`、`sqlite.rs` | 开发运维 | 高 | 仅靠 `SELECT 1` 无法发现损坏、FK 或不可写 | corruption、FK、read-only、写探针回滚 |
| SEN-R-005 | global lease singleton 除 DDL 外还验证 owner UUIDv4、RFC3339 时间和字段组合 | `validate_global_lease_values` | 保障 | 高 | 非法业务不变量可通过 Schema SHA 后进入 worker | 多行、缺行、非规范 UUID/时间、expiry 顺序 |
| SEN-R-006 | Application lock 绑定数据库身份、runtime 目录和 release 路径 | `src/runtime_lock.rs` | 保障 | 高 | 双实例可并发 claim、改库和控制 companion | 同库同/不同 runtime；symlink/hardlink；PID |
| SEN-R-007 | maintenance lock 为外部停机工具保留排他协调边界 | `DatabaseMaintenanceLock` | 保障 | 高 | 备份/恢复工具可能与运行服务同时操作组合状态 | 服务共享持有、维护排他、锁顺序 |
| SEN-R-008 | doctor 验证 release、数据库、凭据、recordings 根、MediaMTX binary/config/contract | `src/doctor.rs` | 开发运维 | 高 | 上线验收只能依靠零散命令，难以证明组合一致 | offline 正反例；每项单独篡改 |
| SEN-R-009 | online doctor 额外探测应用与 MediaMTX loopback readiness | `DoctorOptions.offline`、`live_probe` | 开发运维 | 低 | 只能证明静态状态，不能证明两个进程正在响应 | offline 不依赖进程；online 一项失败即报告 |
| SEN-R-010 | release identity 绑定产品、版本、source revision、target、API、Schema、Web、credential 与 MediaMTX | `src/release.rs::ReleaseIdentity` | 保障 | 高 | 可把不同提交/协议/companion 拼成同名发行物 | identity JSON 与 manifest header 一致 |
| SEN-R-011 | 全树 manifest 精确验证 path/type/mode/size/SHA，拒绝额外条目 | `verify_release`、`static_assets.rs` | 保障 | 高 | 攻击者或误部署可插入/替换资产而仍启动 | missing/extra/tamper/mode/symlink/hardlink |
| SEN-R-012 | release root 必须是规范物理版本路径，正式父目录 root-owned | `validate_release_root`、`PRODUCTION_RELEASE_ROOT` | 保障 | 高 | 可通过 alias 或可写父目录替换已验证内容 | symlink parent、相对路径、错误 suffix、ownership |
| SEN-R-013 | `native/build.sh` 要求 clean checkout、annotated `v0.2.0` 指向 HEAD 和 Linux AMD64 | `native/build.sh` | 开发运维 | 中 | 无法把制品稳定追溯到源码与版本 | dirty tree、lightweight/wrong tag、wrong host |
| SEN-R-014 | build 在同一文件系统 stage，验证后 no-clobber 安装固定发行目录 | `native/build.sh` | 保障 | 高 | 半写 release 或同版本覆盖会让重启内容不可预测 | 中途失败、并发 build、第二次 build |
| SEN-R-015 | lifecycle test 使用临时根覆盖 no-clobber、Secret、锁、失败回滚和链接防御 | `native/lifecycle-test.sh` | 开发运维 | 高 | 脚本安全语义容易在普通单元测试外回归 | 临时根运行；不得访问真实 `/var/lib` |
| SEN-R-016 | relocated smoke 使用真实 Rust/Vite/SQLite/MediaMTX 制品验证重定位和篡改拒绝 | `native/relocated-smoke-test.sh` | 开发运维 | 高 | 静态脚本检查无法证明真实发行闭包 | 真实启动、hashed assets、字节篡改、source-bound binary |
| SEN-R-017 | CI 同时门禁 Rust fmt/check/clippy/test、Web、native 生命周期与 Caddy 当前代理合同 | `.github/workflows/ci.yml` | 开发运维 | 高 | 任一语言或交付层可独立漂移进入 main；代理可能重新指向不存在的容器 | clean checkout 全 job；锁文件模式；根级 Caddyfile/容器上游负例；三个 loopback 上游精确一次 |
| SEN-R-018 | Rust 固定 1.98.0，Cargo.lock 与 npm package-lock 都纳入提交 | `rust-toolchain.toml`、lockfiles | 开发运维 | 中 | 依赖解析随时间变化，构建结果不可复现 | `--locked`、`npm ci`、工具链版本 |
| SEN-R-019 | 源配置统一为 `config/`，主机部署资产为 `deploy/`，客户端为 `clients/web/`，生命周期为 `native/`；根目录不放散落部署文件 | 仓库目录结构、CI proxy gate | 开发运维 | 低 | 配置、客户端和部署资产散落，开发者难以判断事实源；双份代理模板会漂移 | 目录清单；根级 `Caddyfile` 不存在；脚本/文档不引用已移除位置 |
| SEN-R-020 | 当前 Schema identity 为 application `sentinel-monitor`、version 0.2.0、revision 1、SHA `f547ddc817d830d23b5305bb1f88b29898d6531568edd6eb194c2b629eb560c0`；`users` 只有 username，没有 email/role | `src/current_schema.sql`、`src/sqlite.rs`、`native/lifecycle-test.sh` | 保障 | 高 | 发行物、运行库和运维文档可能各自接受不同管理身份 DDL | code-owned fingerprint 重算、metadata/现场 schema、列清单、lifecycle identity 一致 |

## 11. 可观测性、容量和故障边界

| ID | 当前功能/特性与真实行为 | 实现/代码锚点 | 分类 | 复杂度 | 删除后的确定后果 | 最低验证/边界 |
|---|---|---|---|---|---|---|
| SEN-Q-001 | `/healthz` 只证明进程能响应；`/readyz` 同时要求 DB/凭据与 MediaMTX 健康 | `routes::live/ready` | 开发运维 | 中 | 编排器无法区分存活和可服务 | companion down、坏 credential、DB down |
| SEN-Q-002 | tower HTTP trace 和结构化应用日志提供请求/后台错误线索 | `TraceLayer`、`tracing` | 开发运维 | 中 | 线上请求与 reconcile 故障难关联 | status/latency；禁止记录 Token、密码、完整 RTSP URL |
| SEN-Q-003 | 上游错误只持久化/返回固定脱敏类别，不保存远端正文和 Secret | `sanitized_failure`、`AppError` | 保障 | 高 | 摄像头凭据或内网内容可能进入 DB、JSON、Journal | 故意含 Secret 的上游错误负例 |
| SEN-Q-004 | 事件 broadcast 容量固定 256，lag 通过 resync 协议显式暴露 | `broadcast::channel(256)` | 保障 | 中 | 无界内存或静默丢实时通知 | 超 256 事件、慢消费者、SQLite 回查 |
| SEN-Q-005 | 录像容量和保留由 MediaMTX `recordMaxPartSize`、segment 和 deleteAfter 约束 | `config/mediamtx.yml` | 保障 | 中 | 单文件/总保留失控可能填满磁盘 | 文件增长、168h 清理、磁盘/inode 监控；应用当前不建录像 inventory 表 |
| SEN-Q-006 | HTTP JSON、login、ONVIF XML、媒体 auth 字段、playback duration 都有显式上限 | routes/config/onvif | 保障 | 高 | 外部输入可无界消耗内存、CPU、连接或上游带宽 | 每个上限的边界和恢复测试 |
| SEN-Q-007 | SQLite 是事件与 operation 的持久事实源；SSE、React state 和日志只是投影 | Schema、routes、Web effects | 保障 | 高 | 重启或断线后页面内存会被误当作最终事实 | 重连全量读取、进程重启、SSE lag |

## 12. 明确边界与取舍

| ID | 当前决定 | 实现/边界锚点 | 分类 | 复杂度 | 若改变会发生什么 | 实施前最低证据 |
|---|---|---|---|---|---|---|
| SEN-X-001 | 不提供 observer/operator/viewer 或任何 RBAC 开关 | 无 role 列；所有业务 route 解析 `CurrentUser` | 核心 | 高 | 需重做权限矩阵、Session contract、Web 条件展示、审计和升级转换 | 独立授权设计、逐路由测试、Schema 与 Foundation 决策 |
| SEN-X-002 | 不提供内置 TLS 或应用层 HTTPS 强制；由 `deploy/Caddyfile` 所示同源网关终止 HTTPS，后端默认 loopback 并必须由防火墙隔离 | `deploy/Caddyfile`、`src/config.rs`、正式环境样例 | 保障 | 高 | 后端直连会让登录密码/Session 经过明文；内置 TLS 则需承担证书、续期和监听安全 | 生产 `SITE_ADDRESS`、真实证书、三个本机上游、后端不可公网直连；默认 `:80` 仅是模板占位 |
| SEN-X-003 | 不提供视频转码、AI、人脸识别或语义搜索 | 无相关 worker/model/schema | 核心 | 高 | 增加 GPU/CPU、模型供应链、生物特征隐私和派生物状态 | 独立 RFC、资源预算、隐私删除和失败恢复 |
| SEN-X-004 | 不提供云多租户或组织隔离；一个部署是一套 Administrator 与摄像头 | 数据模型无 tenant | 核心 | 高 | 所有查询、JWT、录像路径和审计都要加入租户边界 | 威胁模型、逐查询隔离、计费/配额设计 |
| SEN-X-005 | 不提供运行时 Schema migration、双读或非当前密文 keyring | `src/sqlite.rs`、`src/crypto.rs` | 保障 | 高 | 产品复杂度会随代数增长，并在启动期写未知数据 | 转换只进入 `sarmg-upgrade`，产品保留单一当前格式 |
| SEN-X-006 | 不提供通用操作 Idempotency-Key；camera desired generation 是当前收敛语义 | Schema/queue 实现 | 保障 | 高 | 新 header 必须定义存储期限、payload digest、冲突和重放响应 | API/Schema/容量/清理/并发完整设计 |
| SEN-X-007 | 不保证 PTZ durable；请求断线时不能从 operation API 查询终态 | 同步 `routes::ptz` | 可选 | 高 | 若要保证需为设备副作用建立专用 operation/fencing/unknown 模型 | 设备 mock 故障注入、重复动作风险分析 |
| SEN-X-008 | 不提供审计外部必达 sink/outbox | 只有 `audit_logs` | 可选 | 高 | 若要合规导出需新增持久投递、重试、死信、脱敏和容量控制 | sink 合同、outbox Schema、故障注入、保留策略 |
| SEN-X-009 | 不承诺应用核对每个录像文件的 Hash/inventory；doctor 目前检查安全目录和写探针 | `doctor::recording_write_probe` | 建议保留 | 高 | 若新增完整 inventory，doctor 时间、存储和备份合同都会扩大 | 百万文件预算、增量索引、特殊文件与并发写设计 |
| SEN-X-010 | 不发布 systemd unit；脚本是唯一受支持生命周期入口 | `native/`、release layout | 开发运维 | 中 | 自建 unit 只能调用脚本，不能复制启动逻辑 | 发行/运维文档明确；lifecycle 测试 |
| SEN-X-011 | 不支持 Server ARM、musl、Windows 或 macOS；客户端仅浏览器 | compile/runtime gates | 核心 | 高 | 扩平台不能只删除 compile gate，还需 companion、脚本、锁和发行等价证明 | 新平台完整 CI、真实媒体和部署安全验证 |

## 13. 关键取舍说明

### 13.1 为什么只有 Administrator

当前部署目标是单一可信管理域。保留一个角色让每个已认证业务路由的含义明确，避免“按钮隐藏但 API
仍可调用”、事件查看与摄像头密码管理权限错位等问题。`role:"admin"` 留在 wire 中是 Foundation 的跨
项目身份常量，不表示数据库存在 RBAC。

### 13.2 为什么摄像头变更异步而 PTZ 同步

摄像头配置是长期期望态，必须在重启后继续收敛，所以使用 durable operation、generation 和 lease。
PTZ 是瞬时动作，当前同步调用并只报告本次 HTTP 所能证明的结果。两者故障语义不同，不能在文档中
笼统称为“所有外部写操作均可恢复”。

### 13.3 为什么固定 MediaMTX

Sentinel 依赖 MediaMTX 的配置字段、API path、WHEP/HLS 和 recording 行为。只固定 Rust 而允许任意
companion，无法复现实际媒体面；因此 binary version、platform、SHA、config 和 release manifest 是一个
不可拆分的发行身份。

### 13.4 为什么不在产品里迁移

启动路径只证明当前 Schema、当前 credential envelope 和当前 external key。任何代际转换、组合备份或
恢复都需要停机排他锁和独立审计，应由 `sarmg-upgrade` 单独完成。Sentinel 本身不扫描其他目录、不猜测
格式、不尝试“尽量启动”。

## 14. 功能删除检查表

删除前先在本清单记录目标 ID 和接受的用户后果；随后逐项完成：

1. 删除或替代 Rust route、DTO、查询、后台任务和配置字段。
2. 若涉及状态，生成一个新的完整当前 Schema；产品代码不写 migration。
3. 删除 React 入口、runtime guard、样式和异步清理逻辑。
4. 更新 MediaMTX config/lock、Caddy 路由与 native 生命周期闭包。
5. 从 release identity、manifest、CI 和依赖图中移除对应资产。
6. 添加“已删除入口不可访问、已删除字段不可解析”的当前合同负例。
7. 同步 README、初学者指南、流程树、本清单和运维文档。

只有上述闭包完成，才能认为功能真正删除。若只是关闭配置、隐藏页面或停止测试，它仍然存在于项目
边界中，并继续产生维护和安全责任。
