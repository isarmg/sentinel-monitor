# 09. 部署、安全与生产运维

## 9.1 生产布局

不可变 release tree、0600 环境、SQLite、runtime locks、MediaMTX config/contract 和 recordings 分区管理。
服务账户只能写业务状态，不得修改 binary、Web 或 manifest。公网只暴露可信代理需要的路由。

## 9.2 上线流程

验证 tag/归档/checksum；安装新精确版本目录；设置 ownership/mode；准备当前组合状态和 external key；运行
release verify 与 offline doctor；启动 MediaMTX 后启动 Sentinel；检查两个 readiness、登录、摄像头读取
和实验播放。任一步失败停止，不覆盖原目录。

## 9.3 配置和 Secret

管理员密码、credential key、JWT/signing material、摄像头密码与 TLS key 分开管理。环境文件只放必要
引用/值且 0600。轮换要有变更窗口、备份、验证与明确回滚点。

## 9.4 监控指标

关注 readiness、登录拒绝、Session、operation backlog/age/unknown、lease、reconcile latency/error、
MediaMTX API、publisher、播放错误、录像磁盘/inode、SQLite/WAL 和 outbox。

## 9.5 日常 Doctor

offline doctor 验证 release/Schema/SQLite/key/MediaMTX 合同与录像目录；online 再探测运行进程。doctor
失败是停止自动变更的信号，不是手改 metadata 或跳过检查的理由。

## 9.6 备份恢复

只使用 Sentinel 专用组合命令。定期验证 manifest、所有文件 Hash、recordings inventory 和 external key
认证，并在隔离主机 restore + doctor + 播放。只验证备份目录存在毫无意义。

## 9.7 事件处置

疑似入侵时隔离公网/摄像头 VLAN，停止扩大写入，保全 release SHA、数据库 generation、录像、配置摘要、
审计和日志；轮换 Session、管理员密码、JWT、credential key、摄像头密码和 TLS 材料。

## 9.8 故障表

| 故障 | 操作 |
|---|---|
| operation unknown | 冻结同资源写入，核对实际状态，禁止盲重试 |
| key 解密失败 | 检查受保护 key 来源/ID，不启用旧 key fallback |
| Schema drift | 停止并保全 generation，交给升级工具 |
| 录像盘满 | 停止新增写入，按策略扩容/归档，不手删控制文件 |
| MediaMTX 合同不符 | 恢复精确制品/config，不临时放宽 SHA |

## 9.9 回滚

保持两个服务停止，按 `sarmg-upgrade` recovery journal 选择 commit 或 rollback，验证完整组合状态和 key，
再安装与其精确匹配的 release。不能只回滚 binary 或数据库的一半。
