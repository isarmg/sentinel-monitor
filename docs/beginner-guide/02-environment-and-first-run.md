# 02. 开发环境与第一次运行

## 2.1 所需工具

需要固定 Rust 工具链和 Web lockfile 对应的 Node/npm。完整原生生命周期还需要 Linux、shell 工具以及
仓库固定的 MediaMTX 制品。不要用系统中碰巧安装的另一个 MediaMTX 代替合同资产。

```bash
cargo +1.98.0 check --locked --all-targets
cd clients/web
npm ci
npm run build
```

## 2.2 临时实验环境

为数据库、runtime、recordings、MediaMTX config 与 binary 分别创建受保护临时路径；生成仅用于实验的
32 字节 credential key 和管理员密码，并设置 `BOOTSTRAP_ADMIN_USERNAME=admin`（或其他 Foundation
canonical username）。这个值只初始化 Server 管理账户；摄像头的 RTSP/ONVIF username 仍在摄像头表中
独立加密。所有路径使用绝对路径，避免工作目录变化改变身份。

## 2.3 第一次开发启动

开发模式只绑定回环，使用已构建 Web `dist` 和临时 SQLite。按 `config/sentinel-monitor.env.example`/当前配置解析器设置必要
变量后运行开发 `serve`。正式 source-bound binary 只能从验证过的 release root 启动。

```bash
cargo +1.98.0 run -- serve
```

## 2.4 启动 companion

使用 `native/start.sh` 让脚本验证 binary SHA、版本、配置、目录权限和锁，再启动 MediaMTX；不要绕过
脚本直接后台运行。随后启动 Sentinel，并分别检查应用与 companion 的 loopback readiness。

## 2.5 第一条摄像头练习

只使用实验摄像头或本地测试源：管理员登录，创建摄像头，观察 operation 从 pending/running 到终态，
确认 MediaMTX path，再通过可信代理测试播放。密码不应出现在浏览器状态、审计或日志中。

## 2.6 成功标准

- release/config/Schema/credential 预检均通过；
- 两个进程各自只存在一个实例；
- 创建操作持久化并到达可解释终态；
- 浏览器只拿短期播放授权；
- 重启后 pending 可继续；只有 operation lease 已过期的 running 才转为 unknown，健康 owner 不被改写；
- offline/online doctor 都通过。

## 2.7 常见失败

| 现象 | 先检查 |
|---|---|
| MediaMTX 拒绝启动 | binary SHA、版本、config exact contract |
| Sentinel 拒绝库 | metadata、DDL SHA、lease、密文或锁 |
| 登录循环 | HTTPS、Secure Cookie、Origin/Host、系统时钟 |
| 登录 400 | 请求是否精确为 `username/password`，username 是否符合 3–64 bytes canonical 规则 |
| operation 不结束 | reconciler lease、MediaMTX API、网络 |
| 无画面 | RTSP source、publisher、JWT、代理 WHEP/HLS 路由 |

## 2.8 实验清理

停止两个进程并确认锁释放后删除临时根。不要把临时 credential key、数据库或摄像头 URL 提交到仓库，
也不要拿生产录像做测试 fixture。
