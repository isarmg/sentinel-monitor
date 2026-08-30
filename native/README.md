# Sentinel 0.2.0 原生部署

本目录只创建全新的 0.2.0 部署，不读取或迁移 `.env.native`、源码目录下的 `.env`、旧 runtime 或旧版
SQLite。历史数据升级由独立升级工具负责。

## 发行布局

默认布局如下：

```text
/opt/isarmg/sentinel-monitor/
├── releases/0.2.0/
│   ├── RELEASE-MANIFEST
│   ├── bin/{sentinel-monitor,mediamtx}
│   ├── web/{index.html,assets/...}
│   ├── config/{mediamtx.yml,mediamtx.lock}
│   └── native/{bootstrap,start,status,stop,common}.sh
└── current -> releases/0.2.0

/etc/isarmg/sentinel-monitor/sentinel-monitor.env
/var/lib/isarmg/sentinel-monitor/{db,recordings,logs}
/run/isarmg/sentinel-monitor/{operations.lock,app.lock,app.pid,mediamtx.lock,mediamtx.pid}
```

版本目录全部只读。`build.sh` 先在 `releases/` 内相邻暂存，完整验证后 rename 为 `0.2.0`，再通过同父
目录临时 symlink 与原子 rename 切换 `current`。重复发布完全相同的内容是幂等操作；同一版本只要任一
二进制、Web 文件、配置或脚本不同就拒绝，绝不覆盖。

## 构建与首次配置

先准备官方 `linux_amd64` MediaMTX `v1.20.0` 二进制的绝对路径。构建脚本会按
[`mediamtx.lock`](mediamtx.lock) 同时核对平台、版本和固定 SHA-256，然后将它和 Rust 应用、精确
Web 构建、配置及运维脚本一起发布：

```bash
export SENTINEL_MEDIAMTX_SOURCE=/absolute/path/to/mediamtx
./native/build.sh

/opt/isarmg/sentinel-monitor/current/native/bootstrap.sh
```

`bootstrap.sh` 以 `create_new` 语义创建 0600 环境文件，绝不覆盖已有配置，也不会回显生成的 JWT、
Credential Key 或临时管理员密码。它不会启动服务。使用受保护编辑器替换管理员密码并检查配置后，
显式确认：

```bash
sudoedit /etc/isarmg/sentinel-monitor/sentinel-monitor.env
/opt/isarmg/sentinel-monitor/current/native/bootstrap.sh --confirm-config
/opt/isarmg/sentinel-monitor/current/native/start.sh
/opt/isarmg/sentinel-monitor/current/native/status.sh
```

停止顺序保持为应用（数据库/运行锁）在前、MediaMTX companion 锁在后：

```bash
/opt/isarmg/sentinel-monitor/current/native/stop.sh
```

运维脚本必须从 `releases/0.2.0`（通常经 `current`）运行，并在每次启动/状态检查前验证完整
`RELEASE-MANIFEST`。它们不从 Git checkout 读取应用、Web、MediaMTX 配置、lock 或秘密。
本仓库不发布 systemd unit，也不依赖 systemd 的工作目录或环境注入；0.2.0 的唯一原生生命周期入口
就是上述 release 内脚本。若主机另行编写 unit，它也只能调用这些绝对入口，不能重新定义运行路径。

## Web 构建契约

`build.sh` 在独立暂存目录生成 Web，并把精确目录/文件集合、大小和 SHA-256 manifest 编译进 Rust
二进制。`STATIC_DIR` 必须是版本目录中 Web 的绝对真实路径，不能经 symlink。服务在取得运行锁或打开
SQLite 前重新遍历资源：拒绝缺失、增加、篡改、symlink、特殊文件、硬链接，以及生产模式下带任意
写权限位的目录或文件。未由这套构建流程绑定静态 manifest 的普通 `cargo build` 二进制会 fail closed，
不能启动 `serve`。

## 测试覆盖

`./native/lifecycle-test.sh` 只在 `mktemp` 根下运行，不修改 `/opt`、`/etc`、`/var` 或 `/run`。它覆盖
发布幂等/冲突、原子 current、源码删除后的 bootstrap/start/status/stop、旧 `.env.native` 不导入、
秘密不回显、配置不覆盖、启动失败回滚、start/stop 串行化、中间/最终 symlink 和 release hardlink 拒绝。

`./native/relocated-smoke-test.sh` 构建真实 Vite 资源和绑定该精确 manifest 的 Rust 二进制，将二者放到
临时 release 后从 `/` 启动真实 SQLite 服务，读取首页及全部 hashed assets，并证明篡改后重启会拒绝。

测试可通过以下绝对路径变量重定向；生产默认值不依赖这些测试路径：

```text
SENTINEL_NATIVE_INSTALL_ROOT
SENTINEL_NATIVE_CONFIG_DIR
SENTINEL_NATIVE_STATE_DIR
SENTINEL_NATIVE_RUNTIME_DIR
SENTINEL_BUILD_TARGET
```
