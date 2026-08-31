# Sentinel Monitor 工作流程与流程树

## 1. 总流程树

```text
Sentinel Monitor
├─ 启动
│  ├─ 验证固定 release + MediaMTX contract
│  ├─ 解析生产配置与秘密
│  ├─ 取得 database/runtime/MediaMTX 锁
│  ├─ 验证当前 SQLite 与凭据
│  └─ 启动 MediaMTX、Rust API 和 reconciler
├─ 管理
│  ├─ 登录 -> Session/CSRF/RBAC
│  ├─ 摄像头期望态 -> durable operation
│  ├─ reconciler -> MediaMTX actual state
│  └─ 审计与 operation status
├─ 媒体
│  ├─ 浏览器申请短时 JWT
│  ├─ MediaMTX internal auth 回调
│  └─ WHEP 实时预览 / HLS 回放
└─ 运维
   ├─ start/status/stop/doctor
   ├─ lifecycle + relocated smoke tests
   └─ sarmg-upgrade 负责代际数据操作
```

## 2. 原生发布与首次启动

`native/build.sh` 只接受干净且 annotated `v0.2.0` 指向 HEAD 的 checkout，以及与
`config/mediamtx.lock` 匹配的 `linux_amd64` MediaMTX `v1.20.0`。它构建 Web 和 source-bound Rust
binary，生成完整 manifest，在同一文件系统暂存、验证后 no-clobber 发布固定版本目录。

`bootstrap.sh` 创建账户、目录和 0600 环境文件，但不启动服务、不覆盖配置、不回显随机秘密。操作者
编辑密码并运行 `--confirm-config` 后，`start.sh` 才按锁顺序启动 companion 和应用。

## 3. 正式进程启动顺序

1. 验证进程物理位置、revision、target、API、Schema、Web、凭据 epoch、MediaMTX 和 manifest 全树。
2. 解析环境，生产 Cookie 必须 Secure，`STATIC_DIR` 必须等于发行树 Web。
3. 取得数据库 instance 排他与 maintenance 共享锁。
4. 取得 runtime `app.lock`，维护 PID；MediaMTX 由脚本持有 companion lock。
5. 私有复制并验证 SQLite generation、租约 singleton 与所有凭据可解密。
6. 启动后台 reconciler、HTTP 服务与 readiness。

## 4. 摄像头写操作状态机

```text
HTTP create/update/delete
  -> 验证 RBAC/CSRF/DTO
  -> SQLite transaction: desired camera + media_operation + audit
  -> 202/pending
  -> reconciler 领取 global lease + operation lease
  -> transaction 外调用 MediaMTX
       ├─ 成功 -> actual path + succeeded
       ├─ 明确失败 -> failed + backoff
       └─ 结果不可证明 -> unknown
  -> GET /api/v2/media/operations/{id}
```

只有仍同时持有未过期全局/操作租约的 owner 能 finalize。启动恢复只将确实过期或缺失租约的 running
转为 unknown，不清空健康 owner。删除 API 立即隐藏摄像头，但 MediaMTX 清理状态仍由 operation 跟踪。

## 5. 漂移修复

周期任务读取期望态，在事务外读取 MediaMTX 配置、Publisher、Recording 实际态；摘要比较发现差异
后创建 `drift_detected` 操作。日志和持久错误仅包含 camera/operation ID 与固定错误码，不包含 RTSP
URL、userinfo、用户名、密码或远端错误正文。

## 6. 播放授权

```text
Browser Session + RBAC
  -> /api/v2 请求媒体授权
  -> 当前 HKDF key 签发短时 JWT
  -> Browser 访问同源 /media-webrtc 或 /media-hls
  -> Caddy 转给 MediaMTX
  -> MediaMTX 调用 /internal/v2/media/auth
  -> Rust 验证协议、audience、camera、jti 与时间
```

## 7. 停机

`stop.sh` 通过 operations lock 串行生命周期动作，先停止应用，使数据库/reconciler/runtime 锁释放，再
停止 MediaMTX。直接 kill 或乱序停止可能把外部效果留在 unknown，应保全日志后由 reconciler/人工判断。

## 8. 数据代际流程

备份、恢复或升级前先停止两进程。外部工具固定按 database maintenance、runtime、MediaMTX 顺序取得
排他锁，把 SQLite、MediaMTX config/contract、recordings 和外部 key 身份作为组合状态处理。当前产品
不扫描历史路径，也不通过 runtime fallback 修复旧数据。
