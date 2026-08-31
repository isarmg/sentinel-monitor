# 03. Rust、视频链路与 Web 基础

## 3.1 控制面与媒体面

控制面请求体小、要求认证和持久状态；媒体面是长连接/分段传输，由 MediaMTX 和反向代理承载。Sentinel
只签发受限播放信息并协调配置，不能把大视频流穿过普通 JSON handler。

## 3.2 Rust 状态模型

摄像头、operation、lease、Session、审计和加密 envelope 使用明确类型。外部 JSON 先严格反序列化和
业务校验，再进入事务。`Option` 表示真实缺失，不能把空字符串当成所有字段的通用“未设置”。

## 3.3 异步并不等于无限并发

HTTP、reconciler 和 companion 调用使用 Tokio，但数据库写、远端请求和任务数量均要有界。同一资源的
变更按稳定键串行，避免创建和删除乱序；不同资源可在全局预算内并行。

## 3.4 Web 的责任

Web 展示当前摄像头、operation 和播放状态，发出带 Session/CSRF 的意图。它不能解密 credential、决定
operation 最终事实、信任本地缓存代替 Server，也不能把远端 error body 直接呈现。

## 3.5 播放授权

浏览器取得短期、受资源/用途/时间绑定的授权，再访问代理公开的 WHEP/HLS 路径。授权过期是安全性质，
前端应重新取得而非长期保存。系统时钟偏差会直接影响播放，应纳入诊断。

## 3.6 RTSP、WHEP 与 HLS

- RTSP：摄像头到 MediaMTX 的常见输入。
- WHEP：偏低延迟的 WebRTC 播放入口。
- HLS：基于分段的浏览器播放，延迟较高但网络适应不同。

这些协议的传输失败不应改变控制面数据库中的 Secret 或伪造 operation 成功。

## 3.7 错误边界

用户可行动错误使用受限 code/message；内部 URL、credential、上游正文和栈只进入受保护日志且需脱敏。
外部 MediaMTX 返回的内容是不可信输入，必须限制大小、超时和解析。

## 3.8 时间与 ID

operation/camera/session 使用不可预测或规范 ID；租约和过期时间使用 UTC RFC 3339，调度等待使用单调
时钟。不能用浏览器时间决定服务端授权有效性。

## 3.9 动手追踪

从 Web 的“新增摄像头”动作追到 Rust route、事务、encrypted request、reconciler claim、MediaMTX
调用、终态和审计。分别标注 Secret、用户可见数据和内部诊断。
