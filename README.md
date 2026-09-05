# 哨界 Sentinel Monitor

Sentinel Monitor `0.2.1` 是只支持 `x86_64-unknown-linux-gnu` 物理机原生部署的浏览器摄像头监控系统。Rust/Axum 控制面
负责用户、摄像头、PTZ、审计和期望态；固定版本的 MediaMTX companion 负责 RTSP 接入、WHEP/HLS
播放和录像；SQLite 保存当前业务状态。

产品只理解当前 `0.2.1` Schema、`/api/v2` 协议、凭据 envelope 和固定发行树，不读取其他代数据库、
密文、runtime 或配置，也不提供迁移、备份和恢复命令。稳定版本形成后的代际变更才会交给
`sarmg-upgrade`；当前开发阶段没有历史升级 edge。

浏览器源码统一位于 `clients/web/`；可提交的环境样例和受审 MediaMTX 配置统一位于 `config/`；主机侧
代理模板统一位于 `deploy/`。真实 credentials、运行环境文件和录像不进入源码仓库。生产环境文件唯一位置是
`/etc/isarmg/sentinel-monitor.env`。本仓库刻意不提供 systemd unit；正式生命周期由不可变发行树内的
`native/bootstrap.sh|start.sh|status.sh|stop.sh` 管理。

Sentinel 和 MediaMTX 的控制/媒体上游默认都只监听 loopback。生产唯一公网入口是按
`deploy/Caddyfile` 安装并配置真实 TLS 站点的可信网关；模板只代理本机 `127.0.0.1` 端口，不存在 Docker
Compose，也不识别 `app`、`mediamtx` 等容器 DNS 名称。

## 快速验证

```bash
cargo +1.98.0 fmt --all -- --check
cargo +1.98.0 check --locked --all-targets
cargo +1.98.0 clippy --locked --all-targets -- -D warnings
cargo +1.98.0 test --locked --all-features
cd clients/web && npm ci && npm run check:foundation && npm run build
./native/lifecycle-test.sh
./native/relocated-smoke-test.sh
```

Web 使用 Node `26.7.0`、React/React DOM `19.2.8`、TypeScript strict `5.8.3` 和 Vite `7.3.6`；
`build` 自身会先执行 `check:foundation`，单独列出该命令是为了让依赖漂移更早、更易定位。控制面只有
Administrator 身份，认证端点固定为 `/api/v2/auth/login`、`/api/v2/auth/session` 和
`/api/v2/auth/logout`，不实现路径别名或分级身份。

Server 管理身份采用 Foundation 当前 username 合同：登录精确为 `{username,password}`，Session 精确为
`{authenticated,user_id,username,role:"admin",csrf_token}`。内置 React/Vite Web 的登录、当前管理员和
管理员 CRUD 同步使用 username；`users` 表不再含 email。摄像头 RTSP/ONVIF username、加密密码、媒体
JWT/camera identity 与浏览器媒体逻辑仍是原有数据面合同，不能与 Administrator username 混用。

## 文档

- [文档总览](docs/README.md)
- [初学者学习指南](docs/beginner-guide/README.md)
- [项目工作流程与流程树](docs/project-workflow.md)
- [完整功能与取舍清单](docs/feature-inventory-and-tradeoffs.md)
- [原生部署、安全、诊断与故障运维](docs/operations.md)
