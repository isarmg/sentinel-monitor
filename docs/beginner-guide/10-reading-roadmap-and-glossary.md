# 10. 源码路线、练习与术语表

## 10.1 阅读顺序

先读当前 Schema/crypto/operation 类型，再看 auth 与 route，然后跟进 reconciler 和 MediaMTX client，最后
读 release/native 脚本与 Web。这样先掌握权威状态，再看展示和安装。

## 10.2 按现象找模块

| 现象 | 入口 |
|---|---|
| 启动拒绝 | release、database schema、credentials、locks |
| 登录问题 | auth、login admission、Session/CSRF |
| 操作卡住 | operations、reconciler、leases |
| 无画面 | MediaMTX client/config、JWT、proxy、Web player |
| 录像异常 | recordings policy、native lifecycle、filesystem |
| 重启后 unknown | recovery/reconciler startup |

## 10.3 练习

1. 建立临时当前数据库并运行 offline doctor。
2. 用 mock MediaMTX 完成一条成功 operation。
3. 在远端响应前断开，观察 unknown 与审计。
4. 启动第二实例，验证锁拒绝。
5. 篡改复制 release 的一个 asset，验证启动拒绝。
6. 在隔离目录演练组合 backup/verify/restore。

## 10.4 术语

| 术语 | 含义 |
|---|---|
| control plane | 管理身份、意图和状态的 API/Web |
| media plane | RTSP/WHEP/HLS 视频数据路径 |
| companion | 被固定合同管理的 MediaMTX 进程 |
| desired state | SQLite 中用户要求的资源状态 |
| actual state | MediaMTX 当前观察到的状态 |
| operation | 可重启、可查询的持久变更意图 |
| reconciler | 把期望状态协调到实际系统的 worker |
| lease | 限时声明协调所有权的持久记录 |
| unknown | 外部副作用无法证明的终态 |
| envelope | 包含算法、nonce、key ID、密文等的当前密文结构 |
| AAD | 认证但不加密、用于绑定上下文的数据 |
| outbox | 与业务事务共同持久化的待投递审计事件 |
| WHEP | 浏览器 WebRTC 接收协议 |
| fail closed | 无法验证合同时拒绝继续 |

## 10.5 学成标准

能解释为何 Server 不承载视频、为什么 operation 可能 unknown、为何 external key 不进备份、为何 companion
升级是组合变更、为何 current Schema 不能现场修补，并能在临时环境完成一次恢复演练。

## 10.6 深入入口

端到端顺序见[工作流程](../project-workflow.md)，范围判断见[功能与取舍](../feature-inventory-and-tradeoffs.md)，
生产命令、锁顺序和事件处置见[运维文档](../operations.md)。
