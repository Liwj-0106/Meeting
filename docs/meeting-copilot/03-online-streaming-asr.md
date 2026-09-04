# 03：在线流式 ASR、断线重放与安全配置

## 本阶段要解决什么

本地 Whisper/Parakeet 适合离线和隐私优先场景，但它们仍按 VAD 片段批量推理，低配置设备上很难同时做到低延迟、高准确率和多人会议稳定运行。本阶段增加在线流式 ASR 主链路，同时保留本地录音为最高优先级：网络、服务限流、鉴权失败或云端处理变慢，只能影响在线字幕，不能阻塞或终止录音。

首批接入两个供应商：

| 供应商 | 音频协议 | 实时结果 | 时间信息 | 多人信息 |
| --- | --- | --- | --- | --- |
| Deepgram Nova | 16 kHz 单声道 PCM16LE WebSocket | partial + final | 句级与词级供应商时间 | 可选流式 diarization |
| OpenAI Realtime transcription | 24 kHz 单声道 PCM16LE，经 Base64 append | delta + completed | 当前事件不提供精确词时间 | 当前不提供 speaker |

两者都从 Meetily 的 48 kHz 单调媒体时钟接收音频。供应商缺少的字段保持为空；尤其不会把 ASR 置信度冒充说话人置信度，也不会把本地估算范围标成供应商时间。

协议实现以官方文档为准：

- [Deepgram Streaming Speech-to-Text](https://developers.deepgram.com/reference/speech-to-text/listen-streaming)
- [Deepgram Endpointing and Interim Results](https://developers.deepgram.com/docs/understand-endpointing-interim-results)
- [Deepgram Diarization](https://developers.deepgram.com/docs/diarization)
- [OpenAI Realtime transcription](https://developers.openai.com/api/docs/guides/realtime-transcription)
- [OpenAI Realtime WebSocket](https://developers.openai.com/api/docs/guides/realtime-websocket)

## 当前状态

Deepgram 与 OpenAI Realtime 的设置、连接测试、录音会话接入、断线重连、重放、修订结果、健康事件与停止冲刷都已形成可运行代码。两条在线链路共用同一条非阻塞录音 ingress，但各自保留与供应商协议相符的时间和说话人语义。真实服务验收仍需要用户自己的 API 密钥和网络环境；本仓库自动化测试不会上传真实会议音频。

“断线时自动切换本地模型”的配置结构已经预留，但运行时热切换尚未实现。设置页明确标记为开发中并禁用开关，避免把“计划能力”显示成“已经可用”。当前断线策略是保留完整本地录音、持续消费有界音频队列、自动重连并从稳定边界补传。

## 数据流与背压隔离

```text
麦克风 / 系统媒体
  -> 48 kHz、50 ms 对齐窗口
  -> 本地录音与三轨 saver（权威路径，不能被云端阻塞）
  -> try_send 到容量 640 的在线 ASR 命令队列
       |- Audio：带 sequence、origin_frame、source 的 48 kHz canonical frame
       |- Commit：本地 VAD 自然停顿或最长语音片段边界
       |- Flush：停止录音时要求供应商提交剩余结果
       `- Stop：停止会话；若队列已满，sender EOF 仍能终止 actor
  -> provider supervisor
       |- 30 秒本地 ring
       |- 持续重采样与编码
       |- WebSocket 发送/接收
       |- partial/final 标准化
       `- transcript-update / asr-health-update
```

录音热路径只使用 `try_send`。队列满时丢弃的是云端副本，并记录明确的 canonical frame 缺口；录音、原始音轨和本地 VAD 不等待网络。640 个槽位可以容纳约 30 秒的 50 ms 窗口、VAD commit 和最终生命周期命令。

在线模式下，旧 pipeline 仍会产生供本地模型使用的无界 `AudioChunk`。如果没有消费者，它会在长会议中持续占用内存。在线会话任务因此同时运行一个只负责 drain 的消费者；本地自动回退真正接通前，它不会假装执行本地识别。

## 重连、缓存与重复控制

### 30 秒 ring

supervisor 始终把收到的 canonical frame 写入固定 30 秒 ring。写入要求 sequence 单调、frame 区间连续。pipeline 因在线队列满而丢失窗口时，下一帧会暴露不连续区间；supervisor 发布 `buffer_overflow` 健康事件、清空无法证明连续的历史，再从当前帧建立新 ring。它不会用静音偷偷掩盖未知丢失。

### 退避期间继续消费

连接失败采用 0.5、1、2、4、8 秒上限的指数退避，并加入 ±20% 抖动。等待重连时 supervisor 继续从有界队列接收音频并更新 ring，避免录音线程因为网络等待而堆满队列。鉴权、非法配置、TLS 和不可恢复协议错误 fail-closed；临时断网、服务不可用和限流进入可恢复退避。

### 稳定边界前 1.5 秒重放

重连后从“最后一个稳定转写结束 frame 的前 1.5 秒”开始补传；如果 ring 已淘汰所需历史，事件显式标记 `history_truncated`。Deepgram 返回的重放结果按时间处理：

1. 完全位于稳定边界之前的结果丢弃。
2. 跨越边界且带词时间时，只保留边界之后的词并收缩时间范围。
3. 没有词时间时不切割文本，不伪造词级时间。

同一 provider item/utterance 的 partial 与 final 继续映射到同一个 `utterance_id`，revision 递增并通过 `replaces_event_id` 建立替换链。供应商事件重复到达时由幂等键拒绝，不会追加重复字幕。

## 供应商适配

### Deepgram

连接 URL 由 builder 统一管理参数：模型、语言、PCM 编码、16 kHz、单声道、interim results、endpointing、utterance end、diarization 和关键词。endpoint override 必须是无用户信息、无 fragment、无密钥参数的 `wss://` 地址，也不能覆盖由客户端管理的参数。

`auto` 语言映射为 Nova 多语言模式。延迟档位映射到 100、200、300、700 ms endpointing，本地 VAD commit 也会发送 Finalize。连接每 4 秒发送 KeepAlive；停止时只在需要时 Finalize，最多等待 2 秒接收尾部 final，再发送 CloseStream。

Deepgram 逐词 speaker 编号规范化为 `speaker-N`。只有所有词属于同一说话人时才给整段写一个 speaker；混合说话人结果保留逐词元数据，后续 MOSS 校正阶段再拆分。音频来源取实际 capture topology：仅麦克风为 `mic`，仅系统媒体为 `system`，双路为 `mixed`。

### OpenAI Realtime

默认连接 `wss://api.openai.com/v1/realtime?intent=transcription`，使用 Bearer header；握手后发送当前官方 `session.update`。连接器兼容当前 `session.*` 和历史 `transcription_session.*` 生命周期事件，按 `(item_id, content_index)` 独立累积 delta，允许多个 item 的 completed 乱序到达。

OpenAI 当前 delta/completed 不提供精确词时间、置信度或说话人，因此 capability 明确声明 `provider_timestamps=false`、`word_timestamps=false`、`diarization=false`。会话层只能保存 Meetily 本地 canonical frame 范围，且必须在字段语义中区分“本地范围”和“供应商时间”。

## 密钥与连接测试

WebView 只接收：

```text
provider / model / streamingConfig / hasApiKey / apiKeyConfigured
```

已保存密钥没有读取、显示或回传命令。设置页的密钥框始终为空，只能保存新值或删除旧值。Rust 内部 secret 类型不实现 `Debug`、`Display` 或 `Serialize`；连接配置的手写 `Debug` 固定输出 `[REDACTED]`。密钥放在 Authorization header，不能进入 URL、健康事件、日志或错误字符串。

“测试连接”支持临时密钥和已保存密钥，两者严格互斥。它执行最多 8 秒的安全握手：Deepgram 只发送 KeepAlive，OpenAI 等待 `session.updated`/兼容确认，然后关闭连接；不会上传音频。网络、鉴权、限流、TLS 和协议错误映射为稳定 code 与中文安全消息，不把供应商响应体或 header 返回前端。

当前密钥仍保存在 D 盘 SQLite 中，虽然不会跨 WebView 暴露，但尚未做操作系统级或 DPAPI 静态加密。这是明确的剩余安全项；不能把“IPC 不泄露”描述成“磁盘已加密”。

## ASR 健康事件

在线 ASR 健康与麦克风/系统音频健康是两个独立故障域。`asr-events.ndjson` 与 `asr-health-update` 记录：

- session starting / connecting / streaming；
- reconnect scheduled / backoff；
- replay started / history truncated；
- buffer overflow / degraded；
- provider error / failed；
- session stopped。

每条事件有幂等 `event_id`、会话内全局单调 sequence、provider、frame、attempt、dropped frames、recoverable 与稳定 code。`SharedAsrHealthState` 保存完整事件前缀；WebView 重载时先安装 listener，再读取 snapshot，最后用同一 reducer 合并期间缓存事件。

后端严格在 `recording-started` 之后启动在线 supervisor，避免前端收到 session event 后又被新录音边界清空。停止完成后才清理内存快照；NDJSON 仍保留在对应会议目录。

## 主要代码

| 文件 | 实际职责 |
| --- | --- |
| `audio/transcription/streaming/protocol.rs` | provider-neutral 音频命令、结果与 capability |
| `audio/transcription/streaming/pcm.rs` | 持续 48 kHz → 16/24 kHz 重采样与 PCM16LE |
| `audio/transcription/streaming/ring_buffer.rs` | 30 秒 canonical ring 与 stable-1.5s replay |
| `audio/transcription/streaming/normalizer.rs` | provider partial/final → revisioned `TranscriptUpdate` |
| `audio/transcription/streaming/health.rs` | ASR 健康事件、幂等 reducer 与快照 |
| `audio/transcription/streaming/deepgram.rs` | Deepgram URL、鉴权、wire codec、decoder 与连接 |
| `audio/transcription/streaming/openai_realtime.rs` | OpenAI Realtime URL、session、audio append 与 item decoder |
| `audio/transcription/streaming/supervisor.rs` | Deepgram 重连、重放、去重、冲刷与健康状态 |
| `audio/transcription/streaming/openai_supervisor.rs` | OpenAI item 时间映射、重连重放、revision、冲刷与健康状态 |
| `audio/pipeline.rs` | 非阻塞在线 ingress、本地 VAD commit、停止 Flush/Stop |
| `audio/recording_commands.rs` | provider 选择、会话生命周期、输出转发与 NDJSON |
| `api/api.rs` | 安全公共配置、密钥写删与无音频连接测试 |
| `frontend/src/lib/asr-health.ts` | ASR 健康 reducer 与中文状态文案 |
| `frontend/src/components/TranscriptSettings.tsx` | 中文在线供应商、密钥、延迟、语言、关键词和测试连接 UI |

## 遇到的问题与解决办法

### 在线 provider 被本地模型校验提前拒绝

问题：旧开始录音流程无条件调用 Whisper/Parakeet model validation，选择 Deepgram/OpenAI 后会在打开音频前报“不支持本地 provider”。

解决：后端先读取 Rust-only runtime config，按 provider 构造 `PreparedTranscriptionSession`。本地模式才加载模型；在线模式校验 WSS、模型和密钥并安装 streaming ingress。前端的录音启动门禁也同步按 provider 分流：本地模式检查所选模型是否已下载，在线模式只读取不含密钥内容的 `apiKeyConfigured` 状态，真正的 endpoint、模型和密钥校验仍由 Rust 在打开音频设备前完成。这样不会再把 Deepgram/OpenAI 误报成“本地模型未下载”。

### 网络发送可能阻塞录音线程

问题：如果 WebSocket send 直接发生在 50 ms 音频处理循环，弱网或服务停顿会拖慢录音，最终丢失权威音频。

解决：录音热路径只 `try_send` 到有界队列。socket、重采样、重连和持久化全部在独立 actor 中。

### 在线模式仍产生本地 VAD chunk

问题：pipeline 的本地 `AudioChunk` channel 是无界的。只启动云端 actor、不读取这个 channel，会让长会议内存持续增长。

解决：在线任务并行 drain 该 receiver。真正启用本地 fallback 时再把所有权切换给本地 worker。

### 重连后供应商会重复返回旧句子

问题：补传 1.5 秒能保护被网络切断的词，但也会让已经 final 的文字再次出现。

解决：以 last stable frame 为边界，完整旧结果丢弃、跨界词按真实 word timing 裁剪；无 timing 时保持字段缺失，不用猜测字符与时间的对应关系。

### ASR confidence 被误当作 speaker confidence

问题：句级识别置信度与说话人归属置信度是不同指标，错误复用会让后续 speaker UI 看似非常确定。

解决：两者使用独立可选字段。Deepgram 没有整句 speaker confidence 时保持 `None`；逐词 speaker confidence 只保留在 word 元数据中。

### 停止时 lifecycle command 可能遇到满队列

问题：`Flush` 或 `Stop` 使用非阻塞发送时也可能被满队列拒绝。

解决：队列容量为 30 秒音频额外预留 commit/生命周期空间；pipeline 最后释放所有 sender，receiver EOF 是 Stop 的第二条可靠终止路径。supervisor 对 Stop 或 EOF 都执行最终 commit/finalize、2 秒 drain 和连接关闭。

### 设置在录音中途变化导致卸载错误模型

问题：旧停止流程重新读取当前设置决定卸载哪个本地模型。如果用户在录音中把 provider 改成另一个值，停止可能卸载未参与本场会议的模型；在线 provider 还会错误默认卸载 Whisper。

解决：开始成功时记录本场实际 provider，停止时只使用该值。Deepgram/OpenAI 不卸载本地模型。

### UI 把预留 fallback 当成已实现能力

问题：仅保存 fallback 配置并不等于运行时可以无缝切换；开关可用会误导用户。

解决：设置页显示“开发中”并禁用开关和输入；文档只描述当前自动重连和录音保留，不声称已经本地回退。

### 为什么不能在 Fatal 后直接把 receiver 交给本地模型

问题：在线分支目前持续 drain 旧的本地 `AudioChunk` receiver，而本地 worker 启动时又读取整份当前转写配置；此时配置仍是 Deepgram/OpenAI，无法表达一个独立的本地 fallback 模型。与此同时，在线 supervisor 的退出结果只带错误码，真正可提交的 `last_stable_frame` 留在 actor 内部。若在 Fatal 后简单切换消费者，断线前尚未稳定的 chunk 可能丢失，也可能和供应商 ring replay 产生重复，无法证明字幕边界正确。

解决方案不是打开现有开关，而是先补齐一个完整切片：定义独立 `LocalEngineSpec` 并在录音开始时预热；让 supervisor 退出值显式携带 `StableBoundary`；按 48 kHz canonical frame 保存有界 VAD chunk journal；从稳定边界裁切后执行本场会话单向 fallback，并持久化、广播 `FallbackActivated` 事件。第一版采用“切到本地后本场不自动切回云端”，避免双 provider 来回切换造成难以裁决的 revision；边界裁切、重复控制、停止冲刷和 fallback 启动失败都必须有定向测试。上述前置条件未完成前继续禁用 UI，属于防止数据损坏的有意限制。

## 验证记录

- Deepgram connector：8 个专项测试通过。
- Deepgram supervisor：7 个专项测试通过。
- OpenAI Realtime connector：10 个专项测试通过。
- OpenAI supervisor：12 个专项测试通过。
- streaming core 集中回归：54 个测试通过。
- 在线连接测试 API：6 个纯配置/安全协议测试通过；未访问真实服务。
- pipeline streaming ingress：覆盖队列满只丢云端帧、closed receiver、Flush/Stop 顺序与 VAD commit 顺序。
- 前端 `transcription-config` 与 `asr-health` 两个测试文件通过。
- 录音启动门禁已覆盖本地模型与在线密钥两类前置条件；补齐既有 Bun 测试的本地类型声明后，全量 TypeScript 检查通过。
- `cargo check -p meetily --lib` 通过，包含 Deepgram 与 OpenAI 实际录音启动分支。

所有 Rust、Node、TEMP/TMP 测试缓存均位于本次唯一会话根 `<session-cache>` 的子目录；Meetily 的环境、模型和运行数据位于项目 `portable/`。

## 尚未包含和已知限制

- 未使用真实 Deepgram/OpenAI 密钥做联网、计费、代理、限流和长时会话验收。
- 自动本地 fallback 尚未实现；当前只保证录音继续、健康状态可见和自动重连。
- OpenAI Realtime 不提供本阶段需要的精确词时间和 speaker，不能替代 Deepgram diarization 或后续 MOSS 校正。
- Deepgram 流式 speaker 仍是供应商临时编号；跨重连、跨窗口和整场人物身份稳定属于阶段 04。
- 自定义 WSS gateway 可解决部分网络/代理环境，但客户端尚未实现显式 HTTP/SOCKS proxy 配置。
- SQLite 中的转写 API 密钥尚未做静态加密。
- 供应商 API 可能产生费用；Meetily 源码免费不代表第三方在线模型免费。
- 本阶段没有在 UI 中加入用量/费用上限、分钟预算或供应商账单估算。
