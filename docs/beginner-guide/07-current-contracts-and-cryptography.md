# 07. 当前协议、加密与状态合同

## 7.1 API 当前代

浏览器和服务端只实现当前版本路由与 DTO。unknown fields、非法枚举、超长文本和合同外 ID 必须拒绝。
前端类型不能代替服务端运行时验证。

## 7.2 Schema 身份

`product_metadata` 精确绑定 application、version、revision 和 code-owned DDL SHA。启动/doctor 从实际
`sqlite_schema` 重新规范计算，不信任 metadata 自报。空文件、旧库和漂移库都只读拒绝。

## 7.3 Credential envelope

摄像头 Secret 使用当前算法、version、nonce、key ID 和上下文 AAD 认证加密。AAD 把密文绑定到产品、
记录和字段，防止复制到另一行仍能解密。解析时拒绝任何额外/缺失字段。

## 7.4 External key

原始 key 来自受保护环境/credential file，不写数据库、备份、日志或 JSON。启动时不仅比较 key ID，还
实际认证全部持久 Secret；仅 ID 相同不足以证明 key 正确。

## 7.5 Key rotation

运行时只接受一个当前 key，没有 previous-key keyring。轮换、全量 re-encryption 和验证由停机的
`sarmg-upgrade` 完成，成功安装新 generation 后产品只看新 key。

## 7.6 Lease 合同

单例 lease 行和字段组合是 Schema 之外的业务不变量。owner 必须是规范 UUIDv4，时间为 UTC RFC 3339，
expiry 晚于 updated；空闲态 owner/expiry 同空。非法状态不自动清零。

## 7.7 发行合同

source-bound binary 验证精确 release root、manifest、文件 Hash/mode/type、Web fingerprint、API 与 Schema
身份。拒绝 symlink、额外文件、硬链接 alias 和服务账户可写资产。

## 7.8 变更合同的步骤

定义新唯一格式；更新生产者和消费者；更新 code-owned identity 与负例；在升级仓实现精确转换；删除旧
解析和 dual write；构建真实发行物做重定位与篡改测试。

## 7.9 密码学注意事项

不要自创算法、复用 nonce、把错误区分到可形成 oracle、记录明文或为“救数据”跳过认证。密文损坏应
视为状态不可验证，并在隔离副本上通过已审核工具处理。
