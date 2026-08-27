# 哨界 Sentinel Monitor

一个以Rust为控制面、MediaMTX为媒体面的浏览器摄像头监控系统。项目默认支持RTSP摄像头、WebRTC/WHEP低延迟预览、HLS自动后备、主码流录像、浏览器回放、ONVIF发现/PTZ、账号角色、事件与审计。

## 架构

```text
IP Camera (RTSP / ONVIF)
          |
          v
      MediaMTX  <------>  Rust / Axum  <------> PostgreSQL
       |   |                |   |
    WHEP  HLS          Auth/API/PTZ       encrypted secrets
       \   /                |
        Caddy same-origin gateway
                 |
             Browser UI
```

Rust服务是摄像头、用户和权限的唯一业务事实源。MediaMTX路径由Rust定期对账，浏览器只获取两分钟有效、限定到单一路径的播放令牌。摄像头密码和RTSP地址使用AES-256-GCM加密后存入PostgreSQL。

## 已实现功能

- 管理员、操作员、观察员三种全局角色
- HttpOnly会话Cookie、Argon2id密码哈希、短期媒体JWT
- 摄像头添加、修改、停用和删除
- 主码流与子码流分离，九宫格优先使用子码流
- WHEP实时播放，连接失败自动切换HLS
- MediaMTX外部HTTP鉴权，浏览器不能匿名读取流
- 主码流fMP4分片录像、按时间检索和Range回放
- ONVIF WS-Discovery和WS-Security PasswordDigest PTZ
- 在线状态轮询、上下线事件、SSE实时通知
- 用户管理、事件确认、操作审计
- 健康检查、MediaMTX Control API和Prometheus指标
- Docker Compose、Caddy同源反向代理及可选Coturn配置

## 快速启动

要求Docker Desktop或Docker Engine以及Compose插件。

PowerShell：

```powershell
Copy-Item .env.example .env

$key = New-Object byte[] 32
[Security.Cryptography.RandomNumberGenerator]::Fill($key)
[Convert]::ToBase64String($key)

$secret = New-Object byte[] 48
[Security.Cryptography.RandomNumberGenerator]::Fill($secret)
[Convert]::ToBase64String($secret)
```

把第一个输出填入`.env`的`CREDENTIALS_KEY`，第二个输出填入`APP_JWT_SECRET`，同时修改数据库和管理员密码。然后启动：

```powershell
docker compose up -d --build
```

浏览器访问 `http://localhost`，使用`.env`中的引导管理员账号登录。引导账号只会在用户表为空时创建，后续修改环境变量不会覆盖现有密码。

## 添加摄像头

准备以下信息：

- 主码流RTSP地址，例如`rtsp://192.168.1.20/Streaming/Channels/101`
- 子码流RTSP地址，例如`rtsp://192.168.1.20/Streaming/Channels/102`
- ONVIF设备服务地址，例如`http://192.168.1.20/onvif/device_service`
- 摄像头用户名和密码

推荐摄像头编码：

- H.264 Baseline或兼容性良好的Main Profile
- 关闭B帧
- 关键帧间隔1至2秒
- 子码流控制在360p至720p、256至800Kbps
- 主码流按实际清晰度设置，尽量避免服务端转码

H.265在不同浏览器和操作系统上的WebRTC兼容性不一致。系统不会静默转码，不兼容的编码会明确表现为无法播放；需要转码时建议增加独立的GStreamer或FFmpeg边缘节点。

## 网络设置

同一台电脑访问时，`.env`中可以保留：

```dotenv
MEDIA_PUBLIC_HOSTS=127.0.0.1
```

局域网其他设备访问时，改为监控服务器的局域网地址：

```dotenv
MEDIA_PUBLIC_HOSTS=192.168.1.10
```

允许以下入站端口：

- TCP 80/443：网页、API、WHEP信令和HLS
- UDP 8189：WebRTC媒体
- TCP 8554：仅在需要向MediaMTX推送或调试RTSP时开放

生产环境将`SITE_ADDRESS`设置为真实域名，并设置：

```dotenv
SITE_ADDRESS=monitor.example.com
SESSION_COOKIE_SECURE=true
MEDIA_PUBLIC_HOSTS=monitor.example.com
```

Caddy会为可公开解析的域名自动申请和续期TLS证书。摄像头应放在独立VLAN，MediaMTX的9996、9997、9998端口不应暴露到宿主机或互联网。

## 外网与Coturn

直接UDP连接失败时，在具有公网地址的Linux服务器上设置：

```dotenv
TURN_PUBLIC_HOST=turn.example.com
TURN_SECRET=replace-with-a-long-random-secret
TURN_REALM=monitor.example.com
```

使用覆盖文件启动：

```bash
docker compose -f docker-compose.yml -f docker-compose.turn.yml up -d --build
```

开放TCP 3478以及Coturn配置的中继端口范围49160至49200。`docker-compose.turn.yml`使用host网络，因此不适用于Docker Desktop；Windows部署建议使用独立Linux Coturn节点或托管TURN服务。

## 角色权限

| 操作 | 管理员 | 操作员 | 观察员 |
|---|---:|---:|---:|
| 实时预览、录像回放 | 是 | 是 | 是 |
| PTZ、确认事件、设备发现 | 是 | 是 | 否 |
| 摄像头配置 | 是 | 否 | 否 |
| 用户、审计和系统管理 | 是 | 否 | 否 |

角色或停用状态会在每次API请求时从数据库重新确认，停用账号无需等待JWT到期。媒体令牌默认只生效120秒，并且只能读取签发时指定的摄像头路径。

## 录像和容量

MediaMTX默认每15分钟生成一个录像段，保留7天。通过`.env`修改：

```dotenv
RECORD_DELETE_AFTER=720h
```

容量估算公式：

```text
每天容量GB约等于所有录像码率Mbps乘以10.8
```

例如16路摄像头、每路2Mbps，约为346GB/天。录像保存在Docker命名卷`recordings`，数据库只保存业务和事件数据。生产环境应将该卷映射到独立磁盘、NAS或建立对象存储归档任务。

## 本地开发

后端需要PostgreSQL和MediaMTX。准备环境变量后运行：

```powershell
cargo run
```

前端开发服务器：

```powershell
Set-Location web
npm install
npm run dev
```

Vite会把API、WHEP和HLS请求代理到本机默认端口。生产构建由Dockerfile完成。

## 主要接口

| 接口 | 用途 |
|---|---|
| `POST /api/auth/login` | 登录并设置HttpOnly Cookie |
| `GET/POST /api/cameras` | 摄像头列表与创建 |
| `PUT/DELETE /api/cameras/{id}` | 摄像头修改与删除 |
| `GET /api/cameras/{id}/stream-ticket` | 获取短期WHEP/HLS播放令牌 |
| `POST /api/cameras/{id}/ptz` | ONVIF连续移动或停止 |
| `POST /api/discovery/onvif` | 局域网WS-Discovery |
| `GET /api/recordings` | 查询录像时间段 |
| `GET /api/recordings/play` | 经授权的录像Range回放 |
| `GET /api/events/stream` | SSE实时事件 |
| `GET /health/live` | 进程存活检查 |
| `GET /health/ready` | PostgreSQL和MediaMTX就绪检查 |

## 运维建议

- 定期备份PostgreSQL与录像卷，并实际演练恢复。
- 将`.env`置于主机秘密管理机制中，不要提交版本库。
- 更换`CREDENTIALS_KEY`前先迁移已加密字段，否则原有摄像头凭据将无法解密。
- 监控MediaMTX的`paths`、丢包、入站字节和WebRTC会话指标。
- 大规模部署时将媒体节点与控制面拆开；WHEP/HLS负载均衡需要会话粘滞。
- Docker Desktop通常不能可靠转发局域网WS-Discovery多播；这不影响手动添加。需要自动发现时可原生运行Rust服务或部署局域网边缘代理。
- 不同厂商对ONVIF PTZ支持存在差异。当前实现使用标准GetCapabilities、GetProfiles、ContinuousMove和Stop；只支持HTTP Digest而不支持WS-Security的旧设备可能需要厂商适配器。

