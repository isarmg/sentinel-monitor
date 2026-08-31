# 08. 测试、调试与变更方法

## 8.1 基础门禁

```bash
cargo +1.98.0 fmt --all -- --check
cargo +1.98.0 check --locked --all-targets
cargo +1.98.0 clippy --locked --all-targets -- -D warnings
cargo +1.98.0 test --locked
cd clients/web
npm ci
npm run build
bash -n ../native/*.sh
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

覆盖幂等同请求、幂等冲突、per-camera 串行、远端明确失败、响应丢失、进程重启、unknown、lease 抢占和
outbox 重投。测试最终状态和 Secret 不泄漏。

## 8.5 媒体/发行测试

验证 binary SHA/config mismatch、链接、权限、额外文件、路径重定位、companion lock、start/stop 并发、
启动失败回滚和 hashed asset 篡改。使用临时根，禁止指向真实录像目录。

## 8.6 Web 测试

除渲染外测试 Session 失效、CSRF、operation polling、unknown 展示、播放 Token 过期和错误 redaction。
UI 不能把网络错误自动解释为“操作失败”。

## 8.7 变更联动

Schema、credential、MediaMTX、API、发行布局各自跨越多个模块。修改前列消费者表，修改后全文/路径搜索
旧身份并运行组合 smoke。不留旧路由、旧密文解析或另一 config fallback。

## 8.8 提交检查

格式、Clippy、Rust test、Web build/test、native 生命周期和文档链接全通过；无 Secret/录像/target/
node_modules；文档命令与当前 CLI 一致；每个大问题独立提交。
