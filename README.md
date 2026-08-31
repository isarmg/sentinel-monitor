# 哨界 Sentinel Monitor

Sentinel Monitor `0.2.0` 是只支持 Linux 物理机原生部署的浏览器摄像头监控系统。Rust/Axum 控制面
负责用户、摄像头、PTZ、审计和期望态；固定版本的 MediaMTX companion 负责 RTSP 接入、WHEP/HLS
播放和录像；SQLite 保存当前业务状态。

产品只理解当前 `0.2.0` Schema、`/api/v2` 协议、凭据 envelope 和固定发行树，不读取旧数据库、
旧密文、旧 runtime 或旧配置，也不提供迁移、备份和恢复命令。稳定版本形成后的代际变更才会交给
`sarmg-upgrade`；当前开发阶段没有历史升级 edge。

浏览器源码统一位于 `clients/web/`；可提交的环境样例和受审 MediaMTX 配置统一位于 `config/`。真实
credentials、运行环境文件和录像不进入源码仓库。

## 快速验证

```bash
cargo +1.98.0 fmt --all -- --check
cargo +1.98.0 check --locked --all-targets
cargo +1.98.0 clippy --locked --all-targets -- -D warnings
cargo +1.98.0 test --locked --all-features
cd clients/web && npm ci && npm run build
./native/lifecycle-test.sh
./native/relocated-smoke-test.sh
```

## 文档

- [文档总览](docs/README.md)
- [初学者学习指南](docs/beginner-guide/README.md)
- [项目工作流程与流程树](docs/project-workflow.md)
- [完整功能与取舍清单](docs/feature-inventory-and-tradeoffs.md)
- [原生部署、安全、诊断与故障运维](docs/operations.md)
