# 02：双音轨采集、统一时钟与故障隔离

## 本阶段要解决什么

会议助手不能只保存一条无法解释来源的混合音频。麦克风代表近端参会者，系统媒体代表远端会议、视频或共享内容；两者既要能一起转写，也要能独立追溯，并且其中一路掉线时不能拖垮另一条仍然健康的录音。

本阶段建立三条逻辑轨道和一个统一的 48 kHz 媒体时钟：

- `microphone`：麦克风原始轨，用于近端说话人线索、回放和会后校正。
- `system_audio`：系统媒体原始轨，用于远端会议、视频字幕和远端说话人分析。
- `mixed`：由前两轨按统一窗口派生的兼容混合轨，继续输出为现有消费者使用的 `audio.mp4`。

同时把设备启动、健康、掉线、重连、同步缺口和停止定义为结构化时间线事件，使 UI、诊断和后续评测读取同一份事实。

## 当前状态

已完成，完成日期为 2026-09-01。

三轨保存、50 ms 同步器、尾部排空、单路故障隔离、同步缺口聚合、自动设备事件调度、前端健康状态和事件协议均已实现并通过自动化回归。真实 USB/蓝牙热插拔与 Windows 系统回环录音仍需在目标设备上人工验收，作为外部验收项保留，不影响本阶段代码完成状态。

## 采集拓扑

| 用户选择 | 实际输入 | 保存文件 | ASR 输入 |
| --- | --- | --- | --- |
| 麦克风 + 系统媒体 | 两路 | `microphone.mp4`、`system-audio.mp4`、`audio.mp4` | 对齐后的 mixed |
| 仅麦克风 | 麦克风 | `microphone.mp4`、`audio.mp4` | microphone |
| 仅系统媒体 | 系统媒体 | `system-audio.mp4`、`audio.mp4` | system |
| 两路均禁用 | 无 | 不允许开始 | 无 |

设置页使用明确的 `disabled` 协议值表示“不创建该路”，`null` 仍表示使用系统默认设备。这样可以区分“默认设备”和“不要录制”，也支持看视频时只采集系统媒体，避免把环境声音混入字幕。

## 统一时间契约

### 48 kHz 单调帧时钟

所有音源进入同步器前转换为单声道 48 kHz。时间范围采用半开区间：

```text
[start_frame, end_frame)
timestamp_seconds = start_frame / 48000
```

`AudioChunk` 同时携带 `start_frame` 与 `end_frame`；旧调用方仍可省略，兼容层才使用秒级 `timestamp` 回退。后续转写、说话人、翻译和摘要均可以映射回同一帧轴。

### 初始偏移和重连偏移

麦克风和系统流是顺序打开的。如果两路都从 frame 0 开始，后打开的系统声音会被错误地提前到会议开头。新流的起始帧取以下两者较大值：

1. 已经提交的 `media_end_frame`；
2. 从录音开始到当前时刻、扣除暂停时长后的 wall-clock 帧。

因此初次顺序启动会保留真实的几十毫秒偏移，重连也不会回到 frame 0 或覆盖旧音频。暂停期间捕获回调不推进 source cursor，所以媒体时间轴不包含暂停空洞。

### 50 ms 对齐窗口

同步器每次提交 2,400 帧，即 50 ms：

- 双路模式先等待对应时间范围的两路数据。
- 只有一路领先严格超过 150 ms 缓存预算时，才为缺失路补零并提交。
- 单路模式不等待根本未启用的另一轨。
- 已提交范围的迟到 chunk 会被丢弃，不能回写历史音频。
- 停止时 `drain_final` 会保留不足 50 ms 的最后尾音。

`mixed` 在只有一路样本时直接使用该路；两路同时存在时各留一半增益空间，避免简单相加削波。

## 数据流

```text
CPAL / 系统音频捕获
  -> 单声道、必要时重采样到 48 kHz
  -> 带 source start/end frame 的 AudioChunk
  -> AudioSynchronizer（50 ms 窗口，最大 150 ms skew）
       |- microphone 对齐窗口 -> microphone saver
       |- system 对齐窗口     -> system-audio saver
       |- mixed 对齐窗口      -> audio saver + VAD
  -> VAD（自然停顿或最长约 2.5 s）
  -> 本地/在线 ASR
```

旧实现中的 600 ms ring buffer 和“任意一路够长就立即 OR 拆分”的混音路径已退出主链路。它不仅造成明显字幕延迟，还会把不同时间到达的两路 chunk 错当成同一时刻。

## 保存与恢复

### 文件布局

```text
Meeting .../
  audio.mp4
  microphone.mp4
  system-audio.mp4
  metadata.json
  transcripts.json
  transcript-events.ndjson
  audio-events.ndjson
  .checkpoints/
    mixed/
    microphone/
    system/
```

只有实际启用且允许保存的轨道才创建 saver。三轨共享一个入口 channel，接收任务根据 `DeviceType` 路由到相互隔离的 `IncrementalAudioSaver`。

### 停止顺序

1. 停止捕获，确保不再产生新 chunk。
2. 同步器排空不足一窗的尾部。
3. VAD 排空最后语音片段，等待 ASR worker 完成。
4. pipeline 丢弃最后一个 recording sender。
5. saver 的 receiver 一直读取到 EOF，主流程 `await` accumulation `JoinHandle`。
6. 分别 finalize 三条音轨，再落盘转写和 metadata。

这套 EOF 协议替代了旧的 `is_saving = false` 加固定 `sleep(200ms)`。停止速度不再依赖机器快慢，也不会因为队列尚未处理完而截掉尾音。

停止完成事件保留原有的 `folder_path` 和 `meeting_name`，并增加三个可直接判断的字段：

- `status`：全部完成时为 `completed`；停止、排空、转写等待或保存出现问题时为 `recoverable_error`。
- `save_status`：持久化完成时为 `completed`；保存失败或超时时为 `recoverable_error`。
- `warnings`：列出本次关闭过程中实际发生的问题。

捕获流或 pipeline 停止失败不会再提前返回并把已经半停止的 manager 放回活动槽位。后端会关闭录音标志、结束健康会话、等待或中止转写任务、卸载模型并尝试保存；保存失败也会释放内存状态，同时保留未被证明已安全落盘的 checkpoint。前端仍会收到 `recording-stopped`，并额外收到 `recording-error`，因此可以结束录音界面、保存已到达的转写，并提示用户执行恢复。

### Metadata 2.0

`metadata.json` 新增：

- `audio_tracks[]`：轨道名、文件名、是否启用、完成状态和独立错误。
- `timeline.duration_frames`：权威媒体长度。
- `timeline.sample_rate`：当前为 48,000。
- `timeline.timebase`：当前为 `1/48000`。

旧 v1 metadata 缺少这些字段时通过 `serde(default)` 正常读取。原始轨 finalize 失败只标记对应轨道，mixed 和转写仍可完成；只有兼容 mixed 轨失败才把整场音频状态标为错误。

新版崩溃恢复优先读取 `.checkpoints/mixed`，不存在时回退旧版扁平 `.checkpoints`，不破坏已有会议。

## 音源健康事件

### 协议

低频事件保存到 `audio-events.ndjson`。每条事件包含：

- `session_id`、全局单调 `sequence` 和幂等 `event_id`；
- `track`、`kind`、`state`、`severity` 与稳定错误码；
- `at_frame`、可选 `end_frame`、`gap_frames`；
- `generation` 与 `attempt`，区分普通连续数据和真正的流重建；
- 可恢复标记、设备标签和诊断详情。

枚举采用前向兼容字符串，旧前端遇到未来新增的 track、kind 或 state 不会反序列化失败。

### 启动顺序

前端在 `recording-started` 时清空上一次快照，因此后端严格使用：

```text
recording-started
  -> session_started
  -> 每个实际启动音源 track_configured / healthy
  -> 每个已选但启动失败音源 stream_interrupted / degraded
  -> mixed healthy
```

事件先持久化成功再发给 UI；同一会话只存在一个时间线写入器，sequence 不会因多线程各自计数而冲突。

### UI 呈现

`RecordingStateContext` 独立重放健康事件，按 `event_id` 去重，并拒绝同音源旧 sequence、旧 generation 和跨会话迟到事件。状态栏使用中文低打扰徽标，例如“麦克风正常”“媒体重连中”；普通降级不弹窗，只有 fatal 使用无障碍紧急提示。健康 mixed 在 mic/system 已显示时自动隐藏，避免重复信息。

## 单路故障状态机

```text
healthy
  -> 连续缺失达到设备阈值
  -> device_lost / interrupted
  -> 只停止故障 route，健康 route 继续
  -> 设备重新出现
  -> stream_restart_scheduled / recovering
  -> 只重建目标 route
     |- 成功：generation + 1，device_recovered / healthy
     `- 失败：退避重试；最终保持 degraded，允许手动重试
```

有线设备连续缺失 2 个轮询周期才视为断开；蓝牙设备使用 3 个周期，容忍短暂抖动。健康时 5 秒轮询，检测到缺失后切到 2 秒。只有真正发过 disconnect 的设备恢复时才发布 reconnect；一次未达阈值的抖动只清零计数，不会无谓重建健康流。

初始启动也采用“至少一路成功即可继续”。例如麦克风权限异常但系统音频正常时，录制会降级为媒体单路，而不是整场启动失败。

## 主要代码

| 文件 | 实际职责 |
| --- | --- |
| `audio/synchronizer.rs` | 48 kHz 帧对齐、50 ms 窗口、skew 缺口、迟到丢弃和尾部 drain |
| `audio/recording_state.rs` | 会话帧、暂停压缩 wall clock、设备/错误状态 |
| `audio/pipeline.rs` | 捕获 chunk 入同步器、三轨分发、mixed VAD/ASR |
| `audio/incremental_saver.rs` | 可参数化 checkpoint 子目录和最终文件名 |
| `audio/recording_saver.rs` | 三轨路由、EOF drain、逐轨 finalize、metadata 2.0 |
| `audio/stream.rs` | mic/system 独立启动、停止和重建 |
| `audio/device_monitor.rs` | 抖动阈值、动态轮询和断开/恢复状态机 |
| `audio/timeline_event.rs` | 音频事件协议、单写入器、原子 NDJSON 和健康快照 |
| `audio/recording_commands.rs` | 会话边界、设备事件消费、自动重连和 Tauri 事件发射 |
| `frontend/src/lib/audio-source-health.ts` | 前端纯 reducer、降级/fatal 派生和中文呈现 |
| `frontend/src/contexts/RecordingStateContext.tsx` | 监听并隔离每次录音会话的健康快照 |
| `frontend/src/components/RecordingStatusBar.tsx` | 低打扰音源健康徽标 |
| `frontend/src/lib/recording-preferences.ts` | 设备偏好的串行保存、版本闸门和失败恢复 |
| `frontend/src/contexts/ConfigContext.tsx` | 保存成功后立即更新下一次录音使用的设备 |
| `frontend/src/components/DeviceSelection.tsx` | 默认/关闭选择、设备刷新和已消失设备提示 |

## 遇到的问题与解决办法

### 设置页保存了设备，但下一次录音仍使用旧设备

问题：录音设置组件只把偏好写入后端，`ConfigContext` 却只在应用挂载时读取一次。`useRecordingStart` 使用 Context 中的缓存值，因此同一应用会话内修改麦克风或系统音频后，必须重启才能可靠生效。首页设备弹窗则相反：它只改 Context，不持久化，重启后选择丢失。慢速的启动读取还可能在用户刚保存后返回旧值并覆盖新状态；两个快速保存也可能乱序完成。

解决：所有设备保存统一经过 `RecordingPreferencesCoordinator`。它在请求发起时分配单调 revision，将磁盘写入严格串行化，只允许最新请求更新 Context；启动读取带同一个版本闸门。Rust 保存命令返回实际写入的归一化 DTO，空白设备名归一为 `null`，任意大小写的 `disabled` 归一为小写协议值。设置页和首页弹窗都只在该返回成功后更新 Context；失败时恢复最后一次成功写入的后端值。这样 `null` 继续表示系统默认、`disabled` 继续表示关闭该路，“不使用麦克风”可稳定用于仅系统音频字幕。

设备刷新不会静默改变或保存偏好。若已保存设备暂时消失，下拉框保留原值并标记“当前不可用”，由用户选择系统默认或其他设备；避免一次蓝牙断连永久抹掉偏好。

### 注释写 50 ms，实际窗口却是 600 ms

问题：旧 ring buffer 的变量仍声称 50 ms，实际常量是 600 ms，最大缓存又乘 8 达到 4.8 秒。VAD 和 ASR 之前先平白增加至少 600 ms，用户看到的字幕明显滞后。

解决：主链路换成经过单测的固定 50 ms 同步器，双路最大等待 150 ms；实时 VAD 最长连续语音片段约 2.5 秒，短停顿可更早提交。

### “任意一路够长就混音”破坏时间关系

问题：旧逻辑只要 mic 或 system 达到窗口长度就取数据，不足的另一边从队头补零；它没有时间戳概念，两个不同时间的 chunk 可能被拼成同一窗口。

解决：每个 chunk 带明确帧区间；同步器只按相同 frame range 收集两路数据，等待预算耗尽后才产生带原因的 gap。

### 两个顺序打开的流都从 frame 0 开始

问题：即使引入 source cursor，如果 mic 和 system 构造时都固定为 0，后打开音源的第一帧会被错误提前。重连也可能覆盖旧时间范围。

解决：新 route 起点取已提交帧和暂停压缩 wall clock 的最大值；同步器继续拒绝落在已提交范围内的迟到数据。

### Mixed 音频被伪装成麦克风

问题：旧 pipeline 发给 ASR 和 saver 的混合 chunk 标记为 `Microphone`，后续无法判断用户声音、系统媒体或混合来源。

解决：增加真实 `DeviceType::Mixed`；三轨按类型保存。ASR 在双路时标记 mixed，单路时保留真实来源。

### 停止布尔值先于队列尾部生效

问题：saver loop 依赖 `is_saving`，stop 先清布尔值再 sleep 200 ms。慢盘或长队列时 receiver 尚有数据，循环却已退出。

解决：不再猜等待时间；保留接收任务句柄，只有所有 sender 被释放、receiver 返回 EOF 后才 finalize。

### 停止和保存错误只写日志，界面仍显示成功

问题：旧的 stream、pipeline 和 saver 错误只写入日志，方法仍返回 `Ok(())`。顶层命令遇到停止失败还会把已经关闭部分资源的 manager 放回全局槽位，最终日志无条件宣称“零丢失”；用户既无法判断文件是否完整，也可能得到一个无法继续工作的伪活动会话。

解决：stream 与 pipeline 的错误现在全部聚合返回，保存方法在完成必要 cleanup 后返回 saver 的原始失败。顶层关闭流程只保留 manager 的本地所有权，不再恢复半停止对象；它收集关闭警告、标记 `completed` 或 `recoverable_error`，通过 `recording-error` 和结构化 `recording-stopped` payload 同时通知前端。只有没有任何关闭警告且保存成功时才记录完成日志，不再作无法证明的“零丢失”承诺。

### CPAL stream 被不安全地跨 Tokio 线程移动

问题：`cpal::Stream` 是否可跨线程取决于平台后端。旧实现通过 `unsafe impl Send` 绕过编译器约束，再让 Tokio worker 创建、控制和释放 stream；在部分 Windows 音频驱动上可能造成线程亲和性违规、驱动崩溃或无法稳定释放设备。

解决：每条 CPAL route 都由专用 OS 线程完整持有。设备查找、stream 创建、`play`、`pause` 和 `drop` 全部发生在同一线程；异步层只发送控制命令并等待线程退出。所有人为的 `unsafe impl Send` 已移除，并用不可 `Send` 的模拟资源验证创建和析构线程一致。

### 启动失败留下半运行的录音任务

问题：硬件流已经开始后，会议目录、track saver 或 VAD 初始化仍可能失败。旧流程会吞掉 saver 初始化错误，或让 sender、pipeline、设备引用和全局录音状态停在不同阶段，下一次开始录音时既无法判断真实状态，也可能遗留未说明的 checkpoint。

解决：开始录音现在是 fail-closed 事务。会议名、目录、metadata 和所有必需 track saver 先验证；pipeline/VAD 启动失败时依次停止 state、streams 和 pipeline，关闭所有 ingress sender，等待 accumulation receiver 到 EOF，再把 metadata 写成 `error/interrupted`。失败目录和 checkpoint 保留，错误返回给调用方，不安装全局活动 manager。

### 一个 saver 无法保留原始轨

问题：旧保存器只接收 mixed，无法会后重新做说话人分离，也不能证明一句话来自本机还是远端。

解决：按 `DeviceType` 路由三套 saver，并保持 `audio.mp4` 名称兼容已有播放器和导入代码。

### 三个 saver 各自删除 checkpoint 会相互影响

问题：如果每条轨 finalize 后都清理共同 `.checkpoints`，先完成的轨道会删除其他轨尚未使用或故障恢复需要的数据。

解决：每条 saver 只负责自己的结果；上层只有在全部启用轨成功、`transcripts.json`、`transcript-events.ndjson` 和最终 metadata 均已提交后，才删除每条已知 track 的精确 checkpoint 子目录。部分失败或文本/metadata 写入失败都会保留全部恢复来源。

### 恢复命令可能误删未验证数据

问题：旧恢复结果只描述 mixed 输出，前端可能在部分恢复或旧版模糊响应下清空全部 checkpoint；后端若直接信任传入的 meeting path，还存在把递归清理作用到 recordings 根目录之外的风险。

解决：恢复结果改为 mixed、microphone、system_audio 逐轨结构；只有每条已存在轨道都完成且输出通过路径、普通文件和非空验证时，才允许精确清理已知子目录。后端重新规范化并校验 recordings 根、meeting folder 和 track path，前端也执行第二层严格清理守卫。`failed`、`partial`、`none`、未知旧版状态或任一未验证输出一律保留 checkpoint。

### 原始轨失败会提前中断整场保存

问题：逐轨使用 `?` 会让 microphone finalize 失败时直接返回，mixed、转写和 metadata 都可能来不及落盘。

解决：收集每轨 outcome；raw 错误只写入对应 metadata，继续完成 mixed 和文本。这样诊断能力降级，但核心会议内容仍可用。

### 初始任一路失败导致两路全部失败

问题：旧 `start_streams` 在麦克风创建失败时立即返回，已经可以工作的系统媒体也不会启动。

解决：分别尝试两个 route，只要至少一个成功就继续；失败路保留设备标签和错误详情，并以 `audio_stream_start_failed` 健康事件向 UI 暴露。同步器和 saver 只按实际成功路建立拓扑，不会伪造一条全静音原始轨。

### 重连会停止健康的另一音源

问题：旧 `attempt_device_reconnect` 先调用 `stop_streams()`，再同时重启两路。一次蓝牙麦克风掉线会切断正常的系统媒体。

解决：stream manager 提供每路独立 stop/rebuild；disconnect 只释放目标 route，reconnect 只创建目标 route。

### 设备监控计算了新间隔却从未应用

问题：旧 monitor 的 `check_interval` 不可变，只打印 `next_interval`，健康和故障时都一直使用 2 秒。

解决：把间隔改为可变状态，实际在每轮结尾更新为健康 5 秒或缺失 2 秒，并用纯函数测试。

### 短暂抖动被误报为“已重连”

问题：设备只缺失一个周期、尚未达到断开阈值就恢复时，旧代码仍发送 reconnect。消费者会无谓重建一条从未断开的健康流。

解决：增加显式 `is_disconnected` 状态。只有先发布过 disconnect，恢复才发布一次 reconnect；短抖动只清零计数。

### 健康事件存在但前端没有消费

问题：旧后端只提供手动轮询命令，实际前端没有调用，用户无法知道媒体已丢失。

解决：录制期间建立唯一后台消费者，结构化事件同时写 NDJSON 并通过 `audio-source-health` 推送；前端独立 reducer 显示，不把临时状态塞回旧 `get_recording_state`。

### 前端 started 会清空先到达的健康事件

问题：若后端先发 healthy、再发 `recording-started`，Context 会按新录音边界清空刚收到的快照。

解决：明确并测试 `recording-started -> session_started -> track health` 顺序；跨 session 只有显式 session_started 才能切换。WebView 重载时采用“先安装 listener、再读取当前 session snapshot、最后按同一幂等 reducer 合并期间缓存的 live 事件”，避免 snapshot 与实时事件之间的竞态。停止时按 microphone、system、mixed 顺序补齐终止状态，旧健康徽标不会残留为“正常”。

### 开发缓存和运行数据可能回落 C 盘

问题：Windows 默认 TEMP、Cargo、pnpm、WebView2 和 Tauri app-data 都可能落到用户目录，即使录音文件已经在 D 盘。

解决：便携启动、开发和构建脚本显式设置项目 `portable/` 路径；本次 Codex 会话的所有临时文件统一到 `<session-cache>`，不建立第二个会话 cache。项目不读取这些路径作为运行依赖。

## 验证记录

- `AudioSynchronizer`：5 个测试通过，覆盖不同 chunk 边界、两种单路拓扑、严格 skew 阈值、迟到丢弃和 final drain。
- `RecordingSaver`：15 个测试通过，覆盖三轨路由、EOF 尾部 drain、启动回滚、v1 metadata 兼容和受限恢复路径。
- `IncrementalAudioSaver`：9 个测试通过，覆盖自定义布局、路径约束、逐轨验证、部分失败保留和精确清理。
- `DeviceMonitor`：6 个测试通过，覆盖蓝牙阈值、动态间隔、短抖动、单次 disconnect/reconnect 以及同名错误方向设备。
- `AudioStreamManager`：3 个无硬件测试通过，覆盖 route 隔离、控制对象的安全 `Send` 边界和不可 `Send` 资源的线程归属。
- `RecordingState`：2 个测试通过，覆盖起始 frame 不回退和新会话状态完整复位。
- `AudioTimeline`：8 个测试通过，覆盖协议、幂等、单调 sequence、未知枚举、唯一写入器、reload snapshot、待发送队列和损坏尾行修复。
- 音源健康协议：5 个测试通过，覆盖双路稳定顺序、禁用路不误报、启动失败路降级和停止终态；另有 2 个停止结果协议测试。
- 停止结果协议：2 个纯逻辑测试覆盖兼容字段、成功状态和可恢复错误状态；manager 错误聚合测试确认 stream 与 pipeline 的失败不会互相覆盖。
- Rust 音频模块集中回归：154 个通过、0 个失败、2 个既有硬件测试按标记忽略。
- 便携存储定位：5 个测试通过，覆盖显式 D 盘路径、staged runtime、portable cargo target 与拒绝相对路径回退。
- 前端转写事件、健康 reducer、恢复清理守卫与状态栏 SSR：4 个测试文件通过。
- 设备偏好即时应用：2 个 Node 测试文件通过，覆盖慢启动读取、两次快速保存、A 成功/B 失败恢复、设备仅修改时保留其他字段、服务 IPC 契约、首页弹窗持久化和设备消失提示。
- TypeScript 全量 `tsc --noEmit --incremental false` 通过，0 个错误。
- Rust 设备偏好归一化测试已加入，覆盖 `null`、空串、大小写 `disabled` 和设备名去除首尾空白；本轮定向执行被并行开发中的 live-summary 测试编译错误阻断，尚未计为通过。
- Rust `cargo check -p meetily --lib` 通过；没有新增编译错误。

Rust 与 Node 验证均使用 D 盘工具链；测试 TEMP/TMP 固定为本会话唯一 cache 的 `temp` 子目录。

## 尚未包含和已知限制

- 尚未执行真实 USB、蓝牙热插拔以及 Windows 系统回环录音的自动重连验收。
- 某一路在会话开始前就无法打开时会正确降级并提示，但该失败路不会加入本次会话的设备监控；修复权限或设备占用后需要重新开始录音。会中已经成功启动的路发生掉线，则支持自动重连。
- 尚未完成 1 小时以上三轨 FFmpeg 集成与强制结束进程后的 checkpoint 恢复演练。
- 恢复输出目前只验证“位于允许目录、普通文件、非空”，尚未逐个做完整媒体解码或与 checkpoint 音频内容的哈希等价证明；阶段 08 会加入解码级验收。
- 当前自动重连由设备枚举的 disconnect/reconnect 事件触发。如果驱动回调报告 stream failure、但设备仍持续出现在枚举列表中，本阶段只记录中断，尚未自动重建该 route。
- 不同硬件各自有微小采样时钟漂移。当前同步器会显式产生 gap，尚未用动态重采样做 ppm 级长期漂移补偿；该指标会进入阶段 08 长稳评测。
- 当前 saver ingress 仍是无界 channel。正常实时处理不会堆积，但极慢磁盘的内存上限和背压策略需要在阶段 08 压测后收口。
- 本阶段只区分音源，不把 microphone/system 当作真实说话人；多人识别属于阶段 04。
- 三轨同时编码比只保存 mixed 增加 CPU 和磁盘开销。阶段 08 会给出资源曲线和可选“只保 mixed / 保留原始轨”的产品档位。
