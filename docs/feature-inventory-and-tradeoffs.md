# Sentinel Monitor 完整功能与取舍清单

## 0. 开发者决策台账

本表是删除、拆分和重构功能时的主索引；后续章节解释实现细节。分类只能取“核心、保障、可选、建议保留、开发运维”，复杂度表示同时完成代码、协议、测试和运维变更的综合难度，而不是代码行数。删除一项时必须同时处理“实现/依赖”列列出的闭包，不能只隐藏 Web 入口。

| ID | 功能/特性与当前实现 | 实现/主要依赖 | 分类 | 复杂度 | 删除后的确定后果 | 最低验证 |
|---|---|---|---|---|---|---|
| SEN-001 | 摄像头 CRUD；严格 DTO、revision 和审计 | `src/routes.rs`、`src/sqlite.rs`、Web API | 核心 | 高 | 无法建立或维护受管摄像头清单，项目退化成只读播放器 | 路由、DB、权限和浏览器测试 |
| SEN-002 | ONVIF/网络发现并把候选与已管理设备分开 | discovery client、操作员权限、Web | 可选 | 中 | 仍可手工录入，但部署和排障成本上升 | 超时、重复候选、权限测试 |
| SEN-003 | RTSP 连接意图和加密凭据管理 | cameras 表、credential envelope、MediaMTX | 核心 | 高 | MediaMTX 无法取得受控输入，实时视频主链路中断 | 正确/错误 key、URL 和重启测试 |
| SEN-004 | WHEP/WebRTC 实时预览 | `/media-webrtc`、短时 JWT、`clients/web` | 核心 | 高 | 浏览器失去低延迟直播；仅剩录像或外部播放器 | 单资源授权、过期、跨摄像头拒绝 |
| SEN-005 | HLS/录像回放及受限索引 | MediaMTX recording、录像索引、媒体鉴权 | 建议保留 | 高 | 只能看实时流，历史取证与事件回看消失 | Range、越权、缺段和索引一致性 |
| SEN-006 | PTZ 控制 | ONVIF、operator/admin RBAC、durable operation | 可选 | 高 | 监看保留，但不能移动/缩放摄像头 | 观察员拒绝、超时和 unknown |
| SEN-007 | 事件确认 | 事件表、operator/admin RBAC、审计 | 建议保留 | 中 | 告警无法形成处理闭环，只能外部记录 | 并发确认、权限、审计测试 |
| SEN-008 | 管理员/操作员/观察员三级 RBAC | auth middleware、用户表、全部路由 | 保障 | 高 | 删除或合并会扩大控制权限；单管理员模式会失去职责分离 | 每个路由的角色矩阵 |
| SEN-009 | Argon2 本地密码与等成本失败路径 | auth、用户表、登录预算 | 保障 | 中 | 认证强度或抗账户枚举能力下降 | 正确/错误/未知账号和成本边界 |
| SEN-010 | 摘要 Session、idle/absolute TTL 与撤销 | session 表、Cookie、清理任务 | 保障 | 高 | 不能安全维持浏览器登录或及时撤销会话 | 过期、版本撤销、Secure Cookie |
| SEN-011 | unsafe 请求的 CSRF 与 Origin/Host 约束 | middleware、Session token、可信代理 | 保障 | 高 | 登录浏览器可被跨站诱导执行控制动作 | 无/错 token、错 Origin、代理头测试 |
| SEN-012 | 登录来源/账户/全局限流 | 登录状态、时间窗口、body 上限 | 保障 | 中 | 密码猜测和 Argon2 资源耗尽风险显著提高 | IPv4/IPv6、窗口恢复、并发预算 |
| SEN-013 | AES-256-GCM credential envelope 与字段 AAD | external key、camera 字段、`src/crypto.rs` | 保障 | 高 | 明文落库会扩大泄露；删错 AAD 会允许密文错位 | 篡改、错 key、错 camera/字段拒绝 |
| SEN-014 | key ID 单代合同且无历史 key fallback | 环境配置、启动/doctor | 保障 | 中 | 若放宽会重新引入旧密文兼容分支；若删 key 会无法解密 | 错 key ID 和全表认证 |
| SEN-015 | Durable Operation：pending/running/终态/unknown | operations 表、worker、route 投影 | 核心 | 高 | 网络断线时无法准确表达外部副作用，易盲重试 | 崩溃点、断线、状态机非法转换 |
| SEN-016 | 幂等键与资源 revision | API header、operation 唯一约束、DB 事务 | 保障 | 高 | 重试可能重复控制；陈旧页面可能覆盖新状态 | 同键同请求/异请求、revision 冲突 |
| SEN-017 | lease claim、fencing 与启动时 running→unknown | worker、租约列、启动校验 | 保障 | 高 | 多 worker 可重复执行或崩溃后伪装失败 | 过期 lease、双 claim、重启故障注入 |
| SEN-018 | MediaMTX reconciler 和期望/实际漂移检测 | operation、companion API、定时任务 | 核心 | 高 | SQLite 配置与真实媒体路径持续漂移 | 新增/修改/删除、断线、重启收敛 |
| SEN-019 | requested/completion durable audit outbox | 业务事务、outbox、日志 sink | 保障 | 高 | 敏感控制缺少可证明审计，或业务成功但审计丢失 | sink 失败、重投、稳定事件 ID |
| SEN-020 | MediaMTX HTTP callback 与短时媒体 JWT | `config/mediamtx.yml`、`src/auth.rs`、协议合同 | 保障 | 高 | 媒体资源可能公开或全部不可用 | issuer/audience/kind/path/expiry |
| SEN-021 | MediaMTX 固定版本、平台和 SHA-256 lock | `config/mediamtx.lock`、native build/release | 保障 | 中 | companion 行为不可重现，配置/API 可能漂移 | binary SHA、版本输出、manifest |
| SEN-022 | Rust/Web/MediaMTX 单一 `/api/v2` 协议合同 | `clients/web/src/protocol-contract.json`、`src/protocol.rs` | 保障 | 中 | 三方路径漂移会在运行期产生隐蔽 404/越权 | 编译期嵌入与静态合同测试 |
| SEN-023 | SQLite 当前 Schema identity、integrity/FK 验证 | `product_metadata`、schema SHA、启动检查 | 保障 | 高 | 错库或手改 Schema 可能被当作合法当前库 | 错版本、错 SHA、DDL 漂移、sidecar |
| SEN-024 | 单实例数据库/运行锁 | maintenance、app、mediamtx locks | 保障 | 高 | 双进程会并发 claim、改库和控制 companion | 已持锁启动/doctor/维护拒绝 |
| SEN-025 | 录像树容量和数据库/录像交叉核对 | recordings inventory、路径与资源预算 | 保障 | 高 | DB 指向缺失录像或孤儿文件长期累积 | 缺失/多余/特殊文件/预算超限 |
| SEN-026 | 同源 Web 控制台 | `clients/web`、Vite、静态目录合同 | 建议保留 | 中 | API 仍可用，但没有内置操作界面 | 构建、API base、RBAC 可见性 |
| SEN-027 | 严格环境配置和生产 loopback 边界 | `src/config.rs`、`config/` 样例 | 保障 | 中 | 错拼字段或不安全监听可能静默生效 | 未知字段、相对路径、生产配置 |
| SEN-028 | `doctor` 只读诊断 | DB、目录、key、companion、readiness | 开发运维 | 中 | 故障只能靠零散命令定位，部署验收变弱 | 健康/篡改/离线 companion 场景 |
| SEN-029 | 固定 release tree 与全树 manifest | `src/release.rs`、`native/build.sh` | 开发运维 | 高 | 无法证明二进制、Web、config 与 companion 同代 | 缺失、额外、篡改、重定位测试 |
| SEN-030 | bootstrap/start/status/stop 生命周期脚本 | `native/*.sh`、私有 env、锁顺序 | 开发运维 | 高 | 运维需自行重建启动顺序，容易运行混合代 | 临时根 lifecycle 与故障注入 |
| SEN-031 | CI：Rust、Web、协议、native 生命周期 | `.github/workflows/ci.yml`、测试集 | 开发运维 | 中 | 安全合同和目录变更可能无门禁进入发行 | clean checkout 全门禁 |
| SEN-032 | 中文学习、流程、功能和运维文档 | `docs/`、README | 开发运维 | 低 | 新开发者难以判断边界，运维更依赖口头知识 | 链接检查与代码/命令抽查 |
| SEN-033 | 不提供转码、AI、人脸、云多租户和旧版导入 | 明确边界，不存在隐藏实现 | 核心 | 高 | 若新增，需重新设计隐私、算力、租户和迁移合同，不能视作小功能 | 新 RFC 覆盖威胁模型与资源预算 |

## 1. 功能清单

| 领域 | 当前功能 | 取舍/限制 |
|---|---|---|
| 摄像头 | 创建、修改、删除、发现、在线状态 | 只管理能由当前 MediaMTX/ONVIF 边界表达的设备 |
| 视频 | RTSP 输入、WHEP 预览、HLS 回放 | Rust 不转码或代理媒体正文 |
| 控制 | ONVIF PTZ、事件确认 | 观察员不具备控制权限 |
| 录像 | MediaMTX recording、索引与鉴权回放 | 录像目录是组合备份的一部分 |
| 身份 | 管理员/操作员/观察员、Session、CSRF | 不依赖外部 SSO 或共享账户服务 |
| 安全 | Argon2、限流、当前媒体 JWT、AES-GCM envelope | Secret 丢失不能由产品恢复 |
| 一致性 | durable operations、租约 fencing、漂移检测 | SQLite 与 MediaMTX 是最终一致，不是假原子事务 |
| 运维 | 固定 release、bootstrap/start/status/stop、doctor | 仅 Linux 原生部署，无容器/systemd unit 发行物 |
| 诊断 | DB/目录/凭据/MediaMTX/loopback readiness | doctor 不修改 Schema 或历史状态 |

## 2. 角色矩阵

| 操作 | 管理员 | 操作员 | 观察员 |
|---|---:|---:|---:|
| 实时预览、录像回放 | 是 | 是 | 是 |
| PTZ、确认事件、设备发现 | 是 | 是 | 否 |
| 摄像头配置 | 是 | 否 | 否 |
| 用户、审计和系统管理 | 是 | 否 | 否 |

## 3. 架构取舍

- 选择 MediaMTX companion，避免自行实现媒体协议；代价是 companion 二进制、配置和版本必须作为
  发行合同一起验证。
- 选择 SQLite + durable operation 协调外部系统；代价是 UI 必须理解 pending/unknown，而不能假设
  HTTP 200 就代表远端已完成。
- 选择字段级 envelope 和 AAD，降低密文错位风险；代价是 key 管理成为备份恢复的硬依赖。
- 选择短时媒体 JWT 和 internal auth，媒体面无需浏览器 Session；代价是系统时钟和同源代理配置必须准确。
- 选择原生物理机部署，方便接入局域网摄像头与录像磁盘；代价是没有容器编排抽象。

## 4. 当前版本边界

- 唯一 API `/api/v2`，唯一媒体回调 `/internal/v2/media/auth`；无 0.1 alias。
- 唯一 credential envelope revision 1 和 key ID；无 previous secret、keyring 或旧密文解析。
- 唯一 SQLite `0.2.0` revision 1；无 migration、backup、restore 或自修复。
- 唯一 `linux_amd64` MediaMTX `v1.20.0` lock；不下载 `latest`。
- 唯一物理 `releases/0.2.0`；无 `current`、`latest` 或覆盖安装。

## 5. 明确不提供

不提供云端多租户、移动 App、摄像头固件升级、视频分析/人脸识别、服务端视频转码、跨站共享、容器
镜像、自动旧版导入或内置备份。新增媒体能力必须评估带宽、GPU/CPU、隐私、录像保留和故障恢复，
不能绕过 durable operation 与当前版本合同。

## 6. 摄像头生命周期明细

| 阶段 | 当前实现 | 权威事实 | 失败后的状态 |
|---|---|---|---|
| 发现 | 在当前 ONVIF/网络边界内枚举候选 | 发现结果只是快照 | 不自动持久化未知设备 |
| 创建 | 严格 DTO、加密凭据、pending operation | SQLite 期望状态 | 远端不确定时为 unknown |
| 修改 | revision/授权后持久 operation | 新期望状态与审计 | 冲突要求刷新，不覆盖 |
| 启用/停用 | reconciler 调整 MediaMTX path | operation + actual state | 可重试前先分类副作用 |
| 删除 | 管理员意图与远端清理协调 | operation 终态 | 录像保留不由 UI 隐式删除 |
| 凭据轮换 | 新 Secret 认证加密并协调连接 | 当前 envelope | 日志/审计不含新旧明文 |

## 7. 媒体能力分层

| 能力 | Sentinel | MediaMTX | Reverse proxy/浏览器 |
|---|---|---|---|
| RTSP 拉流 | 保存/解密连接意图 | 建立并维持 publisher | 不接触摄像头 credential |
| WHEP 预览 | 签发短时资源授权 | WebRTC 输出 | TLS、路由和播放 |
| HLS 回放 | 鉴权/索引投影 | 分段媒体输出 | Range/缓存按当前代理策略 |
| 录像 | 管理策略和受限索引 | 写 recordings tree | 浏览器只访问获准资源 |
| PTZ | RBAC、持久/受审计请求 | 通过支持接口作用设备 | 观察员无控制权 |
| 转码/AI | 不拥有 | 仅固定 companion 能力 | 不宣称产品能力 |

## 8. Durable Operation 完整状态

| 状态 | 含义 | 是否自动执行 | 操作者动作 |
|---|---|---:|---|
| pending | 意图已持久化，未 claim | 是 | 观察 backlog/age |
| running | 某 worker 持有 lease 并已开始 | 当前 worker | 不并发提交同资源冲突动作 |
| succeeded | 可证明达到目标 | 否 | 正常继续 |
| failed | 可证明业务拒绝或未达到目标 | 依错误策略 | 修正输入后新操作 |
| unknown | 外部副作用不可证明 | 否 | 核对 MediaMTX/设备 actual state |

requested/completion 审计使用 outbox 与业务事务共同持久化。调用者断开不取消 operation；重启遗留 running
转 unknown，而不是自动重发。

## 9. 身份与授权面

| 身份 | 凭据 | 可访问面 | 禁止事项 |
|---|---|---|---|
| 管理员 | Argon2 密码 -> Session/CSRF | 用户、摄像头、审计、系统管理 | Session 不用作媒体长期 Token |
| 操作员 | 独立账户/Session | 预览、回放、PTZ、事件 | 不改摄像头/用户/系统 |
| 观察员 | 独立账户/Session | 预览和回放 | 不控制设备 |
| 媒体请求 | 短时当前 JWT/internal auth | 单资源/用途/时间 | 不访问管理 API |
| Companion | loopback/锁/固定 config 合同 | MediaMTX 管理与媒体边界 | 不公开到不可信网络 |

## 10. 组合状态与恢复

| 资源 | 为什么必需 | 验证内容 |
|---|---|---|
| SQLite generation | 账户、摄像头、operation、审计、密文 | metadata、DDL SHA、integrity/FK、业务不变量 |
| MediaMTX config | path、录制和接口行为 | 精确内容/Hash、mode、普通文件 |
| Companion contract | binary 版本和 SHA | code allowlist、manifest |
| Recordings tree | 业务媒体字节与目录语义 | entry/type/mode/size/Hash/inventory |
| External key requirement | 解密摄像头与未完成请求 | key ID 且实际认证全部密文 |

原始 key bytes 不进备份。恢复使用 Sentinel 专用命令，在 database maintenance -> runtime -> companion
锁顺序下安装完整状态；generic SQLite restore 会造成混合代，因此明确禁止。

## 11. 容量、超时与资源预算

摄像头数、请求体、字符串、operation backlog、worker 并发、远端超时、审计 outbox、录像目录、数据库/
WAL、JWT 时窗均必须有当前上限或运维阈值。录像容量是最主要的持续增长面；磁盘满可同时破坏媒体写入
和 SQLite 提交，因此必须预留升级 stage、recovery 与备份空间。

## 12. 故障语义表

| 故障 | 系统不做 | 安全收口 |
|---|---|---|
| MediaMTX 调用断线 | 不断言失败/自动重发 | operation unknown，人工核对 |
| Reconciler 崩溃 | 不遗忘 running | 启动恢复并标 unknown |
| Lease 非法/过期组合 | 不自动清零坏字段 | 启动/doctor 拒绝 |
| Credential 解密失败 | 不尝试多把历史 key | fail closed，外部恢复 |
| Companion Hash 不符 | 不运行“相近版本” | start/doctor 拒绝 |
| 录像树/DB 单独恢复 | 不启动混合状态 | 专用组合 restore |
| JWT 时钟偏差 | 不延长到无限期 | 修复 NTP 后重新签发 |

## 13. 候选需求取舍

| 候选需求 | 当前决定 | 主要成本/风险 |
|---|---|---|
| 人脸识别/视频分析 | 不提供 | 生物信息、GPU、模型供应链与索引删除 |
| 云端多租户 | 不提供 | 租户隔离、计费、跨区域和更大认证面 |
| 移动 App | 不提供 | 推送、后台、签名、移动媒体播放器矩阵 |
| 自动固件升级 | 不提供 | 设备变砖、供应链签名与回滚 |
| 容器镜像 | 不提供 | 当前物理路径、录像盘、companion 和锁合同未证明等价 |
| 自动解决 unknown | 不提供 | 无法安全推断外部副作用 |
| 任意 MediaMTX binary | 不提供 | 不可重现的配置/API/安全行为 |

## 14. 功能完成定义

新能力必须覆盖 RBAC、严格 API、持久 operation、reconciler、MediaMTX/设备实际状态、Secret/AAD、容量与
超时、审计、组合备份恢复、native 生命周期、真实重定位 smoke、故障注入和中文运维。只有 UI 控件或
一次成功远端调用不能列为完整功能。
