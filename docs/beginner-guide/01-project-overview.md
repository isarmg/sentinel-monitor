# 01. 项目全景与版本边界

## 1.1 产品定位

Sentinel Monitor 是自托管摄像头控制面：管理摄像头凭据和配置，协调 MediaMTX 拉流/发布，提供浏览器
播放、录像与审计。它不自己实现 RTSP/WHEP/HLS 媒体服务器，也不把摄像头密码暴露给浏览器。

## 1.2 四层结构

```text
Browser -> Sentinel API -> SQLite + operation reconciler -> MediaMTX API
   \----------- WHEP/HLS media through trusted proxy --------/
Camera -------------------- RTSP --------------------> MediaMTX
```

| 层 | 权威事实 |
|---|---|
| Web | 用户意图和展示，不保存权威 Secret |
| Sentinel | 身份、摄像头期望状态、操作、审计 |
| SQLite | 当前控制面持久状态与加密 envelope |
| MediaMTX/录像树 | 实际媒体状态和媒体字节 |

## 1.3 当前版本整体

`0.2.0` binary、API、Schema revision 1、固定 Schema SHA、credential envelope、MediaMTX binary/config
合同和发行 manifest 必须精确匹配。不读取其他代数据库或密文，不注册旧 API，也不在运行时迁移。

## 1.4 期望与实际

数据库记录用户希望存在的摄像头与运行状态；MediaMTX 是远端实际状态。网络和进程崩溃会让二者短暂
不一致，因此 reconciler 持久化意图、租约和结果，而不是假设一次 HTTP 调用就是事务。

## 1.5 组合状态

完整业务状态至少包括 SQLite generation、MediaMTX config/contract、recordings tree 和 external
credential key 身份。只复制 `app.db` 不能形成可恢复备份；原始 key bytes 又不能塞进备份包。

## 1.6 仓库地图

```text
src/          Rust API、认证、状态、reconciler、release
clients/web/  React/Vite 控制台
config/       环境样例、MediaMTX 配置和 companion lock
deploy/       主机侧 Caddy 同源 TLS/媒体代理模板
native/       MediaMTX 生命周期和真实发行 smoke
docs/         教程、流程、取舍和运维
```

## 1.7 明确不做

不提供云账号、多租户 SaaS、任意远程 Shell、摄像头固件更新、旧状态兼容、产品内备份恢复、自动猜测
unknown 操作结果，亦不把 external key 存入数据库。

## 1.8 关键取舍

- SQLite 和单实例简化部署，牺牲 active-active。
- 持久操作准确表达媒体端不确定性，牺牲同步“立即成功”的假象。
- external key 降低数据库单独泄露风险，增加独立 Secret 备份要求。
- 固定 MediaMTX 合同确保可重现，增加升级时的联合验证工作。

## 1.9 学习检查

进入下一章前，应能区分控制 API 和媒体流量、期望状态和实际状态、数据库和组合备份、credential 密文
和 external key。
