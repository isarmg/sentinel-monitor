# 哨界 Sentinel Monitor

一个以 Rust 为控制面、MediaMTX 为媒体面的浏览器摄像头监控系统，只支持物理机原生部署。

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

## 物理机部署

准备 PostgreSQL、MediaMTX、Caddy，以及本项目原生脚本。可参考
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
MEDIAMTX_API_URL=http://127.0.0.1:9997
MEDIAMTX_PLAYBACK_URL=http://127.0.0.1:9996
PUBLIC_HLS_BASE_URL=/media-hls
PUBLIC_WEBRTC_BASE_URL=/media-webrtc
STATIC_DIR=web/dist
```

生产模式始终使用 `__Host-sentinel_session` Secure/HttpOnly/SameSite Cookie。仅本机开发可设置 `APP_ENV=development`，且服务会拒绝绑定非 loopback 地址。

## 角色权限

| 操作 | 管理员 | 操作员 | 观察员 |
|---|---:|---:|---:|
| 实时预览、录像回放 | 是 | 是 | 是 |
| PTZ、确认事件、设备发现 | 是 | 是 | 否 |
| 摄像头配置 | 是 | 否 | 否 |
| 用户、审计和系统管理 | 是 | 否 | 否 |

## 运维

- 定期备份 PostgreSQL 与录像目录，并实际演练恢复。
- 将 `.env.native`、`auto.crt`、`auto.key` 放在主机秘密管理机制中，不要提交版本库。
- 更换 `CREDENTIALS_KEY` 前先迁移已加密字段。
- 摄像头放在独立 VLAN；MediaMTX 的 9996、9997、9998 端口不应暴露到互联网。
