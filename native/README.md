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

SQLite 数据库和录像目录共同构成可恢复数据集。备份前应暂停 Sentinel 与 MediaMTX，完成 SQLite
checkpoint，并同时复制 `data/sentinel.sqlite3` 和 `recordings/`；恢复演练必须验证数据库外键、
录像可读性以及启动后的期望态/实际态对账。

服务日志：

```text
/mnt/c/Users/micro/sentinel-runtime/logs/app.log
/mnt/c/Users/micro/sentinel-runtime/logs/mediamtx.log
```

如果从局域网其他设备访问，把`.env.native`中的公开WHEP/HLS地址改成Windows主机局域网IP，并确保Windows防火墙允许8080、8888、8889/TCP和8189/UDP。
