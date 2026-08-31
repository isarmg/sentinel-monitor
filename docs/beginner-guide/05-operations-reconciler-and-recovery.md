# 05. 持久操作、协调器与故障恢复

## 5.1 为什么需要 Operation

对 MediaMTX 的请求可能已生效但响应在网络中丢失。同步 handler 无法把这种情况可靠归类为失败。先持久
化意图再异步执行，使重启和用户查询都围绕同一 operation ID。

## 5.2 状态机

```text
pending -> running -> succeeded
                    -> failed
                    -> unknown
```

`failed` 只用于能证明未达到目标或明确业务拒绝；`unknown` 表示副作用无法证明。unknown 不是可自动重试
的普通失败。

## 5.3 Claim 与 Lease

reconciler 在事务内 claim 可执行 operation，并更新全局/资源租约。租约有规范 owner、更新时间和 expiry；
空闲字段必须成对为 NULL。租约防止两个 worker 同时协调同一状态，但不是跨版本兼容机制。

## 5.4 执行顺序

```text
claim -> decrypt current request/credential -> validate again
 -> call MediaMTX -> classify result -> terminal transaction + audit outbox
```

外部调用不持有长 SQLite 写事务。完成事务必须检查 lease/operation 仍由当前 worker 拥有。

## 5.5 重启恢复

启动时 pending 可继续；上一进程遗留的 running 转为 unknown，因为无法知道 MediaMTX 是否已经执行。
系统不通过“再发一次”猜测。操作者核对 actual state 后采取新的明确意图或外部恢复流程。

## 5.6 审计 Outbox

requested 和 completion 审计与业务状态同事务写入 outbox，再由有界 worker 投递/整理。这样崩溃不会让
业务成功而审计消失。稳定事件 ID 使重投可幂等。

## 5.7 Reconcile 的边界

协调器可以把已知期望应用到已知实际接口，但不能自动解释未知旧状态、修复 Schema、猜测 Secret 或
覆盖人工修改。发现合同外状态时应报告并停在安全边界。

## 5.8 故障注入

测试调用前失败、远端明确拒绝、响应前断线、响应解析失败、完成事务失败、lease 过期、进程 kill、重启
和 outbox 重投。断言状态与审计，而非只断言 HTTP mock 次数。

## 5.9 处置 unknown

保持资源写入静止，查询 MediaMTX actual state 和日志，核对 operation request 摘要。若能证明目标已达成，
通过当前受审计流程收敛；否则提交新的明确操作。不得直接改数据库终态。
