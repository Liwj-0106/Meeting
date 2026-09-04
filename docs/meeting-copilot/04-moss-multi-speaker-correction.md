# 04：MOSS 多人转写校正与跨窗口说话人稳定

## 本阶段要解决什么

会议字幕首先要“尽快出现”，多人纪要还要求“文本准确、谁说的稳定、后续可纠正”。这三个目标不能强塞给同一条实时链路：在线 ASR 适合低延迟字幕，MOSS-Transcribe-Diarize 适合拿到一段完整音频后联合转写和说话人分离。本阶段已经把本地 MOSS 模型接入独立校正旁路。实时 ASR 仍负责先显示字幕；MOSS 在拿到一个完整音频窗口后异步修正文案并补充说话人标签。这样即使模型较慢、崩溃或显存不足，录音和首屏字幕仍能继续。

当前已经实现协议、进程、模型运行时、存储与录音会话旁路：

- `TranscriptEvent` 新增独立 `DiarizationMetadata`，ASR 的 provider/model/confidence/latency 保持原值；
- SQLite 为当前稿和不可变修订历史增加可选 diarization provenance，旧会议全部兼容；
- 定义无供应商进程依赖的 MOSS worker JSONL 请求/响应协议，并对路径、范围、长度、顺序、重叠和字符串做严格校验；
- 实现与具体 Python 模型适配器解耦的 worker supervisor，覆盖启动握手、有界队列、单任务执行、超时、取消、崩溃检测、指数退避、熔断和优雅停止；
- 实现真实 stdin/stdout JSONL 子进程 transport，并限制单行大小、管道队列、可执行文件、脚本、模型、音频和临时目录；Windows 子进程隐藏启动，stderr 只进入进程内有界私密缓冲区；
- 增加生产 Python worker，加载固定 revision 的 MOSS-Transcribe-Diarize，并在加载前验证模型 manifest、主权重大小与 SHA-256、受审源码 revision 和 helper 哈希；运行时强制 `local_files_only`，不会从网络补文件；
- 增加 `diarization_jobs`、不可变结果修订和结果 segments 持久化，可恢复排队/执行中的作业，并保证迟到的旧结果不会回滚最新状态；
- 实现重叠窗口内的确定性 speaker 映射，保留用户确认、重命名和合并结果；
- 增加 Rust 会话级 diarization actor，把本地 ASR、Deepgram 和 OpenAI Realtime 的可信 stable final，以及同一 canonical frame 上的 VAD 音频块接入有界旁路；
- MOSS 结果由 Rust 生成 canonical `correction` 或 `speaker_update`，先写录音目录的 transcript event history，再把同一事件通知主界面与悬浮字幕；renderer 没有构造或回写校正事件的入口；
- 活动录音尚无可信 `meeting_id` 时，用 Rust `session_id` 和会议目录内的 append-only `.diarization/diarization-jobs.ndjson` 绑定窗口任务，不伪造数据库外键；
- 每个模型窗口只短暂创建 16 kHz PCM WAV，任务成功、失败或取消完成后精确删除；actor 同时只允许一个在途窗口，并只合并保留最新待处理窗口，避免队列已满后仍持续写 WAV；
- 启动时只在当前会议的精确 `.diarization/audio` 子目录内清理 Meetily 命名、超过 24 小时的崩溃孤儿 WAV；扫描、删除数量都有上限，不递归也不触碰用户录音；
- 录音 JSON、保存 API 和 SQLite 映射保留新增字段，旧 JSON 缺字段时仍能读取；
- 确定性单元测试只用合成数据；真实 JSONL smoke 使用 Windows SAPI 生成两种英文声音的轮流发言以及独立静音样本，没有读取用户录音。

本机已经安装可迁移的 Conda 环境、固定模型和生产推理适配器，并完成 RTX 4060 Laptop 上的真实进程 smoke。它证明了 worker 能握手、加载、推理、解析、返回两个说话人并安全退出；SAPI 合成样本不代表真实会议准确率。录音最终保存后的 `session_id -> meeting_id` 持久恢复绑定已经完成；MOSS session journal 导入 SQLite、重启后重新提交未完成 job、真实中文/日文/重叠说话评测和生产滚动节奏仍未完成，因此当前定位是“可运行的异步多人校正基线”，还不能宣传成飞书会议同等级的实时多人纪要。

## 为什么 MOSS 不能直接替代实时 ASR

[MOSS-Transcribe-Diarize](https://github.com/OpenMOSS/MOSS-Transcribe-Diarize) 的官方推理入口接收完整音频文件或数组，再自回归生成带时间和说话人标记的文本。仓库中的 `TranscriptStreamParser` 是对生成 token 的增量解析，不等于能够持续接收 PCM 的低延迟 WebSocket ASR。

因此推荐两层链路：

```text
麦克风 / 系统媒体
  -> Meetily 48 kHz canonical frame 与三轨录音（权威数据，最高优先级）
  -> 在线或本地 ASR
       `- 立即产生 partial/final 字幕，写 asr_* provenance
  -> 独立 diarization 会话 actor（已接入，缺模型或校验失败时 fail-fast）
       `- 当前：stable final 触发、最多 90 s VAD 音频窗、15 s 文本重叠
            -> D 盘临时 WAV snapshot
            -> Rust worker supervisor
            -> 常驻 Python worker stdin/stdout JSONL（固定模型、本地离线加载）
            -> MOSS 转写 + 窗口内 S01/S02 标签
            -> Rust speaker stabilizer
            -> Rust-only speaker_update/correction revision
            -> 先写会议目录 transcript history，再通知字幕与悬浮窗
  -> 字幕、纪要与人工校正消费同一份 revisioned TranscriptEvent
```

默认策略是“实时 ASR + 异步 MOSS 校正”。真实模型 warm smoke 的 RTF 小于 1，但第一次加载曾测得 RTF 大于 1；滚动窗口模式仍需完成真实会议、窗口长度与积压评测后再默认开启。

## 字段边界：不要把 ASR、说话人和作业状态混在一起

### 三种 revision

| 字段 | 含义 | 例子 |
| --- | --- | --- |
| `TranscriptEvent.revision` | 同一 utterance 的不可变修订号 | partial 0 → final 1 → speaker correction 2 |
| `DiarizationMetadata.revision` | 独立 diarization pass 的版本 | 第一次滚动校正 3，最终全量校正 4 |
| `model_revision` | 可复现的模型工件版本 | 固定 commit SHA 或发布 revision |

这三者不能互相代替。SQLite 的 `(meeting_id, utterance_id, TranscriptEvent.revision)` 是不可变键；同一修订的 `pending → running → resolved` 不能原地覆盖。本阶段新增独立 `diarization_jobs`、`diarization_result_revisions` 和 `diarization_result_segments`，持久化作业生命周期及不可变结果版本。`TranscriptEvent.diarization_status` 仍只表示“这个 utterance 修订采用的 diarization 结果状态快照”。

### 独立 provenance

`DiarizationMetadata` 包含：

- `provider`、`model`、`model_revision`；
- `revision`、`window_id`；
- `window_start_frame`、`window_end_frame`；
- `status`、`latency_ms`。

窗口范围统一使用 Meetily 的 48 kHz canonical frame。worker 可以在内部把 snapshot 重采样成 MOSS 要求的 16 kHz，但响应必须换算回 48 kHz 坐标。这样字幕、录音和在线 ASR 仍共享单调时间轴。

SQLite migration 为 `transcripts` 和 `utterance_revisions` 各加九个 nullable 列。旧数据迁移后保持 `NULL`，不会猜测 provider，也不会把既有 Deepgram speaker 字段回填成 MOSS provenance。当前修订写入时，`asr_provider/asr_model` 与 `diarization_provider/diarization_model` 分开 bind，并有测试断言两组值同时保留。

### MOSS 没有 speaker confidence

MOSS 输出局部标签与文本，但官方格式没有说话人置信度。新 assignment 的 `SpeakerMetadata.confidence` 因此固定为 `None`。稳定器内部的 IoU/文本匹配分数只用于窗口映射，字段名为 `match_score`，不能伪装成模型置信度、写入 `speaker_confidence` 或显示成“识别可信度”。

## JSONL worker 协议

Rust 会启动会话级常驻 Python 子进程，通过 stdin/stdout 逐行传输 JSON；worker 不监听端口，也不把 prompt、会议文本或文件路径写日志。启动时先交换 handshake 并加载本地固定模型，之后支持 `execute`、`cancel` 和 `shutdown` 控制消息。handshake 返回实际 backend、device 和 dtype，Rust 会严格校验 schema、worker revision、model revision 与单任务上限。

### 请求

```json
{
  "schema": 1,
  "job": "job-0001",
  "session": "session-0001",
  "window_start_frame": 48000,
  "window_end_frame": 2928000,
  "audio_path": "D:\\...\\window-0001.wav",
  "prompt": "技术评审会议；专有名词 Meetily",
  "model_revision": "固定的模型 revision"
}
```

`prompt` 可选，最多 4096 个 Unicode 字符；它是转写上下文，不是任意 Python 指令。请求 `Debug` 只显示字符数，路径固定显示 `<redacted>`。`job/session` 只接受有限 ASCII 标识符，`model_revision` 也不能含空白、控制符或 shell 片段。

`audio_path` 必须是配置根目录内的绝对 `.wav`/`.flac` 路径，拒绝 `.`、`..` 和越界路径。请求进入 supervisor 时先做词法检查；真正派发前再 canonicalize 已存在文件并检查 canonical 根边界，Python 打开前还会重复同等约束，防止 junction/symlink 绕过。

### 成功响应

```json
{
  "schema": 1,
  "job": "job-0001",
  "session": "session-0001",
  "window_start_frame": 48000,
  "window_end_frame": 2928000,
  "segments": [
    {
      "start_frame": 96000,
      "end_frame": 192000,
      "speaker": "S01",
      "text": "先确认接口边界。"
    }
  ]
}
```

响应必须与请求的 schema/job/session/window 完全一致。校验器还会拒绝：

- 超过 8 MiB 的单行、物理多行 JSON、未知字段；
- 空窗口、超过默认 90 秒的窗口、越界或零长度 segment；
- 超过 4096 段、单段超过 16384 字符、总文本超过 1048576 字符；
- 空 speaker/text、控制字符、乱序 segment；
- 同一局部 speaker 的时间自重叠。

不同 speaker 的时间重叠合法，因为真实会议允许抢话和重叠语音。响应 `Debug` 只暴露段数和帧范围，不输出会议正文。worker 失败响应只接受固定错误码；supervisor 和作业表都不会保存 Python 原始异常、stderr、路径或任意错误字符串。

### 子进程边界

`MossJsonlProcessFactory` 在启动前解析并检查五类路径：生产 Python 可执行文件和 executable root 都必须位于 canonical portable root，worker 脚本必须位于指定 source root，模型、音频和临时目录必须位于同一个非 C 盘 portable root。portable root 在 canonicalization 前后都会执行 Windows 存储策略，拒绝 C 盘、verbatim C 盘和 device namespace；D 盘 junction 若最终解析到 C 盘也会被拒绝。音频文件在 supervisor 派发前和 Python 打开前各做一次 canonical containment 检查。

子进程环境先清空，再只传入运行所需的系统路径、CUDA 路径和 D 盘缓存变量。stdout 只允许最大 8 MiB 的单行协议消息，并通过有界队列传给 transport；没有换行符或超长消息会使该 worker 被回收。stderr 由独立任务持续排空，最多保留 16 KiB 私密诊断，不进入日志、数据库、IPC 或调用方错误。

Windows 使用 `CREATE_NO_WINDOW`，避免每次窗口校正弹出控制台。子进程意外退出会关闭 stdout，transport 随即把存活状态设为失败，既有 supervisor 再执行退避、重启和熔断。

握手、写入、取消和停止使用较短的 control I/O timeout。`execute` 写入完成后等待结果时不再套用这个短超时，而由 supervisor 的 `job_timeout` 统一限制整次推理；超时后 supervisor 会丢弃结果等待 future，再走有界取消与进程回收。这样几秒以上的正常推理不会被误判成管道故障。

## Worker supervisor 如何隔离失败

worker supervisor 是一个独立 Tokio actor。调用方只通过有界队列提交经过协议校验的请求，process transport 负责 Python 进程与管道细节。队列默认容纳两个等待作业，同时只允许一个作业进入模型；录音线程和实时 ASR 不需要等待这个队列。

```text
Starting -- ready ----------> Ready -> Busy -> Ready
    |                           |       |
    |                           `-------+-> Backoff -> Starting
    |                                         `-> CircuitOpen -> Starting
    `-- missing dependency --> Unavailable（停止自动重启）

任意状态 -> Stopping -> Stopped
```

| 能力 | 当前行为 |
| --- | --- |
| 启动握手 | 校验 schema、worker revision、model revision，并强制 `max_in_flight = 1` |
| 静态不可用 | 缺环境、缺模型或工件校验失败时停在 `Unavailable`，不进入重启风暴；修复后需重建 supervisor |
| 有界队列 | 满时立即返回 `QueueFull`，不会无限占用内存 |
| 超时 | `job_timeout` 限制完整推理；超时后发送有界取消，回收当前 transport，再按退避策略创建新 worker |
| 主动取消 | 取消正在执行的单窗任务；transport 无法恢复空闲时自动回收 |
| 崩溃检测 | 执行错误会立即检测；空闲期间通过非阻塞存活状态定时发现退出 |
| 退避和熔断 | 连续失败采用有上限的指数退避；达到阈值后打开熔断器并拒绝积压作业 |
| 停止 | 停止接收新作业，取消当前作业，逐个返回等待作业的结构化停止结果，再限时关闭 transport |
| 错误脱敏 | 对外只返回错误码和 `retryable`，不携带 prompt、正文、路径或 Python 原始 stderr |

请求入队时仍执行既有的词法路径检查。真正派发前，supervisor 还要求快照已经存在，解析音频根目录与文件的 canonical path，再次检查文件位于根目录内，并把 canonical path 交给 transport。这一步能够阻止 junction 或 symlink 指向根目录外。文件在检查完成后、worker 打开前仍可能被同权限进程替换；未来 snapshot 生成器应使用私有会话目录、只写一次后关闭并限制其他写入，真实 transport 也要在打开文件时执行同等检查。

supervisor 的实时队列仍在内存中，SQLite repository 则能为已绑定会议保存独立 job。活动录音目前没有可信 `meeting_id`，运行时不会为满足外键而伪造 ID，而是把 job/result revision 追加到当前会议目录的会话日志；最终保存后把这些记录受控绑定到 SQLite、以及重启后重新提交未完成 job，仍是下一切片。数据库层已经能列出 `queued/running`、恢复中断状态并阻止迟到旧结果回滚 latest。

job 创建和 result 写入都支持“提交成功但确认丢失”后的安全重试。完全相同的不可变 job payload，或完全相同的 result header 与有序 segments，会作为幂等成功处理；相同 ID 携带不同内容，或不同 result ID 复用同一个 `(job_id, revision)`，会返回明确的 immutable conflict，并且不会改变 latest 指针。这里没有使用未经核对的 `INSERT OR IGNORE`。

结果表只允许固定 `status` 与固定 `error_code`，没有保存 stderr 或任意异常文本的列。成功结果的 segment 必须落在 job 的 48 kHz 窗口中；失败和取消结果不能携带文本。job、meeting、window 与 model revision 不一致时，数据库 trigger 会拒绝整笔事务。

## 跨窗口 speaker 稳定

MOSS 的 `S01`、`S02` 只在一次音频输入内有效。下一窗口的 `S01` 可能是另一个人，不能直接当全局身份。本阶段稳定器只在相邻窗口重叠区域做保守映射：

1. 按当前 local label 和既有稳定 speaker ID 分组。
2. 仅比较时间实际相交的 segment。
3. 计算 `0.7 × 时间 IoU + 0.3 × Unicode 字符文本相似度`。
4. 达到阈值的候选按分数降序，再按 local label 和 speaker ID 排序。
5. 做确定性一对一匹配；未匹配标签生成新的 `speaker-NNN`，状态为 `provisional`。

文本相似度采用忽略空白、大小写的字符级 Levenshtein，能直接处理中文和日文，不依赖英文分词。时间不相交时，即使出现相同短句也不会强行认作同一人。

稳定器不修改旧 observation。匹配到以下状态时，继续携带原 speaker ID、显示名和状态：

- `user_confirmed`：用户已确认；
- `renamed`：用户已重命名；
- `merged`：用户已合并身份。

这能防止自动校正把“张三”改回“S01”。不过它只能解决相邻重叠窗口的标签漂移，不能证明相隔十分钟再次出现的声音属于同一人。真正的长间隔身份关联需要独立 speaker embedding/voiceprint、用户授权与相应评测；MOSS 本身没有提供稳定 speaker embedding，本阶段绝不伪造。

当前一对一选择是确定性 greedy matching，不是全局 Hungarian 最优解。发言人数较多、抢话密集、候选分数接近时可能出现局部次优；后续可在保持相同输入输出契约的前提下替换为 Hungarian/min-cost matching。

## 多人会议的轨道策略

| 场景 | 首选输入 | 说明 |
| --- | --- | --- |
| 线下会议 | `mixed` 或会议室麦克风轨 | 能分离不同声音，但远场、混响和重叠讲话是主要难点 |
| 在线会议 | `system_audio` + `mic` 分开处理 | 本机用户可由 mic 轨确定；远端多人再对 system 轨 diarize，避免把本机回声当新人 |
| 外语视频 | `system_audio` | 通常只有主持人与嘉宾；不用麦克风可避免环境声干扰 |
| 导入录音 | 原文件转 canonical snapshot | 保留原文件，校正结果写新 revision，不覆盖原始录音 |

“支持多人”应解释为“为不同声音生成并稳定 speaker ID”，不等于知道真实姓名。真实姓名来自用户重命名、会议参会人映射或未来经授权的 voiceprint，不能从 `S01` 猜出来。

## 失败隔离与资源策略

MOSS 队列永远是录音和实时 ASR 的旁路。录音、ASR 和 renderer 通知都不等待 worker；当前会话 actor 只运行一个窗口，并把繁忙期间到来的 stable final 合并为最新待处理窗口。只有获得执行槽位后才渲染和写入 WAV，因此高频 final 不会先制造一串注定入不了队的派生音频。

- 新窗口到达时 worker 仍忙：用最新窗口替换尚未执行的旧候选，保留完整录音供会后补算；
- worker 超时/崩溃：标记失败并重启 worker，不能停止录音；
- 响应校验失败：整窗拒绝，不写半份 speaker correction；
- 单窗失败：不覆盖 ASR 文本、不修改用户确认的 speaker；任务完成后只删除本任务派生的临时 WAV，原始录音保持不变；下次启动还会对精确命名的过期孤儿 WAV 做非递归、有界清理；
- 停止会议：先安全落盘音频与 transcript history，再决定是否等待校正；
- 校正积压持续增长：自动退出实验滚动模式，转为会后处理。

当前接线由每个 stable final 触发，并取最多 15 秒相邻文本上下文；它是为了验证契约和失败隔离，不是已经调优的生产 cadence。后续滚动实验仍建议从 60 秒窗口、45 秒步长、15 秒重叠开始，对应 48 kHz frame：

| 参数 | frame 数 |
| --- | ---: |
| 60 秒窗口 | 2,880,000 |
| 45 秒步长 | 2,160,000 |
| 15 秒重叠 | 720,000 |
| 协议默认最大 90 秒 | 4,320,000 |

2026-09-02 在 RTX 4060 Laptop 8 GB、约 31.6 GiB 内存的本机上，生产 JSONL worker 使用 `cuda:0`、BF16、Transformers 后端完成了两种 Windows SAPI 英文声音的 17.803 秒轮流发言。以下是实测进程数据；它是功能与性能 smoke，不是准确率基准。

| 场景 | 模型握手与加载 | 推理耗时 | RTF | worker 峰值 RSS | PyTorch 峰值 CUDA allocated / reserved |
| --- | ---: | ---: | ---: | ---: | ---: |
| 第一次冷启动 | 48.586 s | 21.656 s | 1.216 | 2.609 GB | 本轮未记录 |
| 后续验证运行 | 7.960 s | 5.585 s | 0.314 | 1.851 GB | 1.897 / 1.992 GB |

后续验证返回 4 段、2 个 speaker，已知 SAPI 文本的 CER、WER、cpCER 都为 0。这个结果只说明固定的干净合成样本通过；远场中文、日文、噪声、口音、抢话与真正多人会议仍可能显著变差。Windows WDDM 下 `nvidia-smi` 没有提供该进程的显存数字，因此报告只保留 PyTorch 实际采样值，不填猜测值。

独立 10 秒静音请求没有生成 segment 或凭空文本，但官方推理路径返回了固定的 `invalid_result_unparseable_text` 失败。worker 对它保持 fail-closed，不把任意推理异常吞成成功，也不会触发无限重试。生产链路的 VAD 通常不会提交纯静音窗口，后续仍应为这个已确认的官方空结果形态增加更精确的归一化。

第一次冷启动的 RTF 大于 1，意味着窗口到达速度可能快于校正速度；即使 warm RTF 已小于 1，也不能把 MOSS 当作逐字实时主 ASR。Windows 基线继续使用原生 PyTorch + Transformers + SDPA，默认一次执行一个异步校正窗口。启用稳定滚动模式前还要测 30/60/90 秒窗口、真实会议积压与长会显存稳定性。

## D 盘便携布局

模型与环境安装必须延续 Meetily 可整体搬迁的 `portable` 目录，不写用户级 Conda、Hugging Face、Torch 或 Python 缓存到 C 盘：

```text
portable/
  conda-envs/moss-td/
  conda-pkgs/
  sources/MOSS-Transcribe-Diarize/
  runtime/workers/moss_runtime_source/
  app-data/
    models/moss-transcribe-diarize/
    cache/models/huggingface/
    cache/torch-inductor/
    cache/numba/
    temp/moss-td/<session-id>/
```

根启动器和三个便携开发/构建启动器已经把 Conda、Mamba、Hugging Face、Transformers、Python bytecode、Numba、TorchInductor、Torch extensions、Triton 与 Matplotlib 路径指向 `portable`。直接启动 exe 时，`storage.rs` 也会为模型与 Python/Torch 缓存设置 D 盘兜底路径。

当前本机安装占用如下。字节数来自 2026-09-02 完成后的目录遍历，构建缓存不计入长期运行必需空间。

| 内容 | 路径 | 大小 |
| --- | --- | ---: |
| MOSS Conda 环境 | `portable\conda-envs\moss-td` | 8,128,971,293 B |
| 固定模型 | `portable\app-data\models\moss-transcribe-diarize` | 1,833,091,806 B |
| 已暂存 Meetily runtime | `portable\runtime` | 206,224,355 B |
| 固定官方源码 | `portable\sources\MOSS-Transcribe-Diarize` | 691,053 B |

安装或复核使用根目录脚本。自动化会话必须把 `MEETILY_SESSION_TEMP` 指向它在 `<session-cache>\` 下唯一会话目录；普通用户不设置时，安装临时文件也会留在项目 `portable` 内。

```bat
set "MEETILY_SESSION_TEMP=<session-cache>"
node scripts\setup-moss-portable.mjs
```

安装器从官方 GitHub 获取 source commit `cb765f2b0fe6f7a298aa2002e2281ae693d1f3c3`，模型固定到 Hugging Face revision `902e98bcb3db33ac913d3496127b92a8d81f2daa`，并校验 1,817,113,576 字节主权重的 SHA-256 `9a0ceb4ab7330357db3ff583dba8d83625d5b733b00e1d55d6970e11b07026c4`。直接依赖版本会逐项验证；`av==15.0.0` 的 Windows CPython 3.12 wheel 还会在安装前验证 SHA-256 `383f1b57520d790069d85fc75f43cfa32fca07f5fb3fb842be37bd596638602c`。完整解析后的环境 freeze 有哈希记录，但传递依赖尚未全部按 wheel hash 锁定。

运行包自带受审 helper 与 Apache-2.0 LICENSE，不依赖源码 checkout 的绝对路径，也不会复制第二份模型权重。整体迁移时移动完整 `meetily` 目录并继续用 `run-meetily.cmd`；MOSS 所需环境、模型、源码和 worker 的相对位置会一起保留。当前 MOSS 路径要求项目自己的 `portable\app-data`；自定义外部 `MEETILY_DATA_DIR` 与这套布局组合时会 fail-closed，避免静默回退到其他盘。

## 怎么评测 ASR 与多人识别

不要只看一段“听起来不错”的演示。建立版本化评测清单，并同时保存 ASR 基线、MOSS 输出和人工 gold：

### 数据集与自有场景

- 中文多人公开集：[AISHELL-4](https://www.openslr.org/111/)；
- 中文会议公开集：[AliMeeting](https://www.openslr.org/119/)；
- 英文会议公开集：[AMI Meeting Corpus](https://groups.inf.ed.ac.uk/ami/corpus/)；
- 自建授权集：线下远场、飞书/浏览器会议、中文/日文/中英混说、抢话、静音、噪声、同音术语、同一人离开再回来。

公开集只用于可复现比较；最终上线门槛必须以用户实际设备、麦克风、系统媒体和目标语言为准。

### 指标

| 目标 | 指标 | 解释 |
| --- | --- | --- |
| 中文/日文文本 | CER | `(替换 + 删除 + 插入) / gold 字符数` |
| 英文文本 | WER | 与 CER 同理，但按词计算 |
| 多人联合正确性 | cpCER/cpWER | 在 speaker permutation 后评价“文字 + 归属” |
| 说话人分离 | DER、JER | 漏检、误检和 speaker confusion；可用 [dscore](https://github.com/nryant/dscore) |
| 人数估计 | speaker count error | 预测人数与真实人数的差 |
| 可运行性 | RTF | 处理耗时 / 音频时长；RTF < 1 才可能不持续积压 |
| 交互延迟 | p50/p95 correction latency | 一个窗口结束到 correction 可见的时间 |
| 资源 | 峰值 VRAM/RAM、D 盘临时空间 | 分 30/60/90 秒与不同 speaker 数记录 |
| 稳定性 | speaker flip rate | 同一真实说话人在相邻窗口被换 ID 的比例 |

实验滚动模式可先把 `p95 RTF ≤ 0.5` 当工程目标，因为 60/45 窗口需要为重试、UI 与总结留余量；这是待实测的门槛，不是当前性能承诺。另设三条不可妥协的安全门槛：无录音丢失、无用户确认 speaker 被自动覆盖、非法/错窗 worker 响应 100% 拒绝。

评测报告至少按语言、设备、轨道、speaker 数、是否重叠讲话和窗口长度分组，不能只报全局平均值。MOSS 校正还要与“在线 ASR 原文本 + 无校正 speaker”基线对比，分别报告 CER/WER 变化与 DER/cpCER 变化，避免为了 speaker 更好而悄悄把文本变差。

## 外语字幕与同声传译

MOSS-Transcribe-Diarize 是转写与 diarization 模型，不是翻译模型。外语视频的合理链路是：

```text
system_audio -> 低延迟 ASR 原文字幕 -> 独立流式翻译 -> 双语悬浮字幕
                              `-> MOSS 延迟校正 speaker / 原文
```

翻译结果必须引用源 utterance ID 和 revision。源文本被 MOSS/ASR 修订后，翻译再产生自己的新 revision，不能直接覆盖原文；用户确认的人名与术语表同样优先。翻译 API、延迟控制、双语排版和总结联动不在本阶段实现范围。

## 主要代码与验证

| 文件 | 实际职责 |
| --- | --- |
| `audio/transcription/event.rs` | 独立 diarization metadata/status 与旧 JSON 兼容 normalization |
| `audio/diarization/moss_protocol.rs` | secret-safe JSONL 编解码、严格校验和 fixture 测试 |
| `audio/diarization/worker_supervisor.rs` | transport 抽象、握手、背压、单任务、取消、超时、崩溃恢复、退避熔断与安全停止 |
| `audio/diarization/process_transport.rs` | JSONL 子进程、路径根、隐藏窗口、有界 stdout/stderr、崩溃探测和控制消息 |
| `workers/moss_worker.py` | 生产 fail-closed worker、固定工件信任链、本地模型加载、48 kHz frame 换算与安全 JSONL 输出；合成 fixture 只能由测试显式启用 |
| `workers/moss_runtime_source/` | 随 runtime 分发的两个受审上游 helper、source manifest 与 Apache-2.0 LICENSE，使搬迁后的 worker 不依赖 checkout 绝对路径 |
| `audio/diarization/speaker_stabilizer.rs` | 时间 IoU + Unicode 文本相似度的一对一 speaker 稳定 |
| `audio/diarization/session_runtime.rs` | 会话级有界 ingress、VAD 音频窗、短期 WAV、session journal、supervisor 提交与 Rust-only correction/speaker update 回注 |
| `audio/transcription/worker.rs` | 本地可信 ASR 直接持久化，并把音频窗与 stable final 旁路给 diarization actor |
| `audio/recording_commands.rs` | Deepgram/OpenAI 可信输出直接持久化、在线录音音频旁路、会话启动/停止；公开 `transcript-update` 只做 UI 通知 |
| `audio/recording_saver.rs` | 录音 transcript JSON 保留 nested metadata 与一致的 flat mirror |
| `frontend/src/types/index.ts`、`frontend/src/lib/transcript-events.ts` | 无 UI 的前端事件类型、normalization 与修订去重保留 provenance |
| `database/models.rs` | 当前稿与历史修订的 diarization 列映射 |
| `database/repositories/transcript_event.rs` | 事务写入、不可变冲突、范围校验与独立 ASR/diarization 断言 |
| `migrations/20260901020000_add_diarization_metadata.sql` | 旧数据库兼容的 nullable schema 迁移 |
| `migrations/20260902080000_add_diarization_jobs.sql` | durable job、不可变结果修订、latest 防回滚与固定错误码约束 |
| `database/repositories/diarization_job.rs` | job 创建/执行/恢复、核对 payload 的幂等重试、原子结果写入与 latest result 读取 |
| `api/api.rs`、`database/repositories/meeting.rs`、`transcript.rs` | 保存/读取 API 的可选字段贯通 |
| `scripts/setup-moss-portable.mjs` | D 盘幂等环境安装、固定 source provisioning、直接依赖核验与环境 manifest |
| `scripts/download-moss-model.py` | 固定模型下载、跨进程独占锁、Range 断点续传、durable checkpoint、完整 SHA 校验与原子落盘 |
| `scripts/smoke-moss-runtime.mjs` | 启动生产 JSONL worker，执行 SAPI 双声和静音，采样冷启动、RTF、RSS、CUDA 资源并调用统一 ASR 指标脚本 |
| `scripts/run-meetily-build-portable.cmd`、`stage-meetily-runtime.cmd` | 只接受 `tauri build --debug --no-bundle` 产物，绑定 exe marker，核对资源后同卷原子替换 runtime |

本阶段验证命令全部使用 D 盘 portable 工具链和本会话唯一 TEMP/TMP。最终相关结果为：

- 整个 diarization Rust 模块 33 个默认测试通过，5 个真实 Python 集成测试保持 ignored；这 5 个测试再用 `portable\conda-envs\moss-td\python.exe` 显式执行，5/5 通过；
- 会话 actor 8/8 重点测试通过，覆盖单在途/latest-coalesce、不可用时零 WAV、迟到结果抑制、精确回收与过期孤儿 WAV 的有界非递归清理；
- Python worker 11/11 单元测试通过，覆盖模型 manifest/revision/主权重 hash 损坏 fail-closed、跨说话人重叠、同 speaker 重叠拒绝、取消迟到抑制，以及完全缺少 portable source checkout 时仍能使用随 runtime 分发的受审 helper；
- 下载器 4/4 安全测试通过，覆盖第二安装器独占锁、payload fsync 后才推进 checkpoint、最终 hash 失败重置区间自愈，以及 429/5xx/timeout 一类瞬态失败重试；
- `cargo check --locked --lib` 通过；`tauri build --debug --no-bundle --ci` 完成，13/13 个前端页面生成成功；原子 staging 核对 exe、FFmpeg、llama、两个 ORT DLL、模板、worker、helper、manifest 与 LICENSE 后成功替换 `portable\runtime`；
- `run-meetily.cmd` 启动返回 0，进程保持运行，且没有监听旧开发端口 `localhost:3118`；这证明运行包已包含前端，不再依赖开发服务器；
- 生产 JSONL worker 的 SAPI 双声、独立静音、schema/bounds、shutdown 与资源采样结果记录在本会话 D 盘验证目录。没有使用用户会议录音。

重新执行真实 smoke 时，先设置一个非 C 盘验证目录。脚本会覆盖该目录中的三份固定报告；静音当前按 fail-closed 记录为失败，因此整次 runner 返回非零是已知严格验收结果，不能当作语音样本推理失败。

```bat
set "MEETILY_SESSION_TEMP=<session-cache>"
node scripts\smoke-moss-runtime.mjs
```

## 仍有限制

1. 录音最终保存后的 `session_id -> meeting_id` 受控绑定已经完成；session journal 导入 SQLite，以及重启后恢复并重新提交未完成 job 仍未完成。
2. stable-final 触发的接线已有单在途和 latest-coalesce，但窗口 cadence、长会积压降级和会后全量 pass 尚未经过真实会议调优。
3. 全局 Hungarian 或 speaker embedding identity、人工 speaker 编辑界面与参会人映射尚未实现；当前标签只在相邻重叠窗口内保守稳定。
4. 仍缺真实中文、日文、混合语言、远场、噪声和重叠讲话数据的 CER/WER/cpCER/DER 验收，以及 MOSS correction 对总结和翻译延迟的端到端验收。
5. 纯静音会安全失败且不产生幻觉，但尚未按官方固定空输出单独归一化为 `success + []`。
6. 自定义外部 `MEETILY_DATA_DIR` 暂不与项目内 MOSS runtime 混用；当前会明确不可用，不会静默改用 C 盘或任意模型目录。

下一安全切片应先补齐 session journal 的数据库导入与中断 job 重提，再用公开、许可明确的多人会议集与授权自有样本建立分语言、分设备的基线。只有真实会后模式通过数据不丢失、资源上限与评测门槛后，才把滚动校正作为默认能力。

## 官方参考

- [MOSS-Transcribe-Diarize GitHub](https://github.com/OpenMOSS/MOSS-Transcribe-Diarize)
- [MOSS-Transcribe-Diarize Hugging Face](https://huggingface.co/OpenMOSS-Team/MOSS-Transcribe-Diarize)
- [论文：MOSS-Transcribe-Diarize](https://arxiv.org/abs/2601.01554)
- [FlashAttention 安装说明](https://github.com/Dao-AILab/flash-attention)
- [vLLM GPU 安装说明](https://docs.vllm.ai/en/latest/getting_started/installation/gpu/)
