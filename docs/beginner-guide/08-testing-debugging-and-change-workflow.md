# 08. 测试、调试与变更方法

## 8.1 基础门禁

Web 使用仓库 `.node-version` 固定的 Node `26.7.0`，与 Foundation 设计包的 engine 合同一致。

```bash
cargo +1.98.0 fmt --all -- --check
cargo +1.98.0 check --locked --all-targets
cargo +1.98.0 clippy --locked --all-targets -- -D warnings
cargo +1.98.0 test --locked
cd clients/web
npm ci
npm run check:foundation
npm run typecheck
npm run build
cd ../..
bash -n native/*.sh
```

随后运行 `native/lifecycle-test.sh` 和 `native/relocated-smoke-test.sh`。真实 smoke 覆盖 Rust、Vite、SQLite、
MediaMTX 及 hashed assets，是静态检查不能替代的证据。

## 8.2 分层定位

按 release/config -> database/key -> auth -> operation persistence -> lease/reconciler -> MediaMTX -> proxy/
browser 排查。每跨一层保留 operation ID 和受限日志证据。

## 8.3 数据库测试

测试新库、metadata、现场 DDL、非法 lease、WAL、integrity/FK、写探针回滚、双实例和 maintenance 锁。
绝不修改生产库来制造 fixture。

## 8.4 Operation 测试

覆盖 generation 单调性、同代 active operation 唯一、per-camera 收敛、远端明确失败、响应丢失、进程
重启、unknown、健康/过期 lease 和 fencing。当前没有 Idempotency-Key 或审计 outbox，不得编写假合同
测试；应验证摄像头持久变更的审计与业务同事务，以及 PTZ/登录审计的 best-effort 边界。

## 8.5 媒体/发行测试

验证 binary SHA/config mismatch、链接、权限、额外文件、路径重定位、companion lock、start/stop 并发、
启动失败回滚和 hashed asset 篡改。使用临时根，禁止指向真实录像目录。

## 8.6 Web 测试

除渲染外测试 Session 失效、CSRF、operation polling、unknown 展示、播放 Token 过期和错误 redaction。
UI 不能把网络错误自动解释为“操作失败”。

Web 还必须证明设计边界：`main.tsx` 只从 `@sarmg/design-tokens@0.3.0` 导入 token、scoped reset 和
accessibility，`body` 带 `data-sarmg-scope`，而 `styles.css` 保留 Sentinel 品牌 token 并映射 Foundation
语义。不要在 `vendor/` 复制共享 CSS，也不要添加 CDN 或运行时网络 fallback。

## 8.7 变更联动

Schema、credential、MediaMTX、API、发行布局各自跨越多个模块。修改前列消费者表，修改后全文/路径搜索
已退役身份并运行组合 smoke。不留路径别名、额外密文解析或第二个 config fallback。

## 8.8 提交检查

格式、Clippy、Rust test、Web build/test、native 生命周期和文档链接全通过；无 Secret/录像/target/
node_modules；文档命令与当前 CLI 一致；每个大问题独立提交。
