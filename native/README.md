# WSL原生运行

此目录用于在Debian WSL中原生运行项目，不依赖容器运行时。

默认路径：

- 项目源码：`/mnt/sarmg.org/sentinel-monitor`
- Rust临时构建产物：`/dev/shm/sentinel-monitor-build`
- Rust运行二进制：`/mnt/c/Users/micro/sentinel-runtime/bin/sentinel-monitor`
- MediaMTX：`/mnt/c/Users/micro/sentinel-runtime/bin/mediamtx`
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

服务日志：

```text
/mnt/c/Users/micro/sentinel-runtime/logs/app.log
/mnt/c/Users/micro/sentinel-runtime/logs/mediamtx.log
```

如果从局域网其他设备访问，把`.env.native`中的公开WHEP/HLS地址改成Windows主机局域网IP，并确保Windows防火墙允许8080、8888、8889/TCP和8189/UDP。
