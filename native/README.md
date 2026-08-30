# WSL原生运行

此目录用于在Debian WSL中原生运行项目，不依赖容器运行时。

默认路径：

- 项目源码：`/mnt/sarmg.org/sentinel-monitor`
- Rust临时构建产物：`/dev/shm/sentinel-monitor-build`
- Rust运行二进制：`/mnt/c/Users/micro/sentinel-runtime/bin/sentinel-monitor`
- MediaMTX：`/mnt/c/Users/micro/sentinel-runtime/bin/mediamtx`
- SQLite：`/mnt/c/Users/micro/sentinel-runtime/data/sentinel.sqlite3`
- 录像：`/mnt/c/Users/micro/sentinel-runtime/recordings`
- 日志和PID：`/mnt/c/Users/micro/sentinel-runtime/logs`
- 私密配置：`/mnt/sarmg.org/sentinel-monitor/.env.native`

构建与运行：

```bash
cd /mnt/sarmg.org/sentinel-monitor
./native/bootstrap.sh
./native/build.sh
./native/start.sh
./native/status.sh
```

停止：

```bash
./native/stop.sh
```

浏览器入口为`http://127.0.0.1:8080`。WHEP使用8889/TCP，HLS使用8888/TCP，WebRTC媒体使用8189/UDP。

## Companion 版本契约

`native/mediamtx.lock` 固定 MediaMTX 的版本、平台和 SHA-256。`start.sh` 在启动任何进程前校验
`bin/mediamtx`；本地替换二进制而不更新受审查的锁文件会被拒绝。当前契约为：

```text
version=v1.20.0
platform=linux_amd64
sha256=25947caac403f37ec881c9be213af2cad67e344a6c7098905b0d31c17f40e336
```

Sentinel `0.2.0` 产品只初始化并接受带精确 `product_metadata` 与现场 schema 指纹的当前数据库。
已有的无元数据/`0.1.x` 数据库会只读拒绝；升级、备份与恢复只由独立升级工具仓库执行，产品二进制
不提供这些命令。不要手工复制 WAL 主文件，也不要在产品启动失败后让产品尝试改写旧库。

Rust `serve` 进程自身持有数据库同目录的 instance/shared-maintenance 锁，并同时持有
`$SENTINEL_RUNTIME_DIR/app.lock`、维护 `app.pid` 到退出；因此同一数据库即使误配不同 runtime，第二
实例也会在打开 SQLite 前被拒绝。符号链接路径及数据库/锁文件硬链接别名均拒绝。MediaMTX 由
`native/start.sh` 使用 `flock --no-fork` 在同一 PID 中 exec 并保留 `mediamtx.lock` 文件描述符。
外部升级工具必须按数据库 maintenance、runtime、MediaMTX 的顺序校验锁与 PID，并在服务未完全
停止时拒绝继续。current-schema 与 Doctor 契约见项目
[`README.md`](../README.md) 的“运维”章节。

服务日志：

```text
/mnt/c/Users/micro/sentinel-runtime/logs/app.log
/mnt/c/Users/micro/sentinel-runtime/logs/mediamtx.log
```

如果从局域网其他设备访问，把`.env.native`中的公开WHEP/HLS地址改成Windows主机局域网IP，并确保Windows防火墙允许8080、8888、8889/TCP和8189/UDP。
