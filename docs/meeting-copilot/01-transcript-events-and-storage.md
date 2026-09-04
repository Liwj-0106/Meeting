# 01：可修订转写事件与存储

## 本阶段要解决什么

原有实时转写事件只依靠 `sequence_id` 追加文本。在线 ASR 会多次修改同一句 partial，MOSS 也会在更大上下文中校正文本和说话人；如果继续追加，就会产生重复字幕、错误摘要和无法解释的数据库记录。

本阶段把一句话定义为稳定的 `utterance_id`，每次修改使用递增的 `revision`。前端、悬浮窗、恢复存储和 SQLite 都按照同一规则处理事件：新版本覆盖当前视图，历史版本仍可审计和回放。

## 当前状态

已完成，完成日期为 2026-09-01。

本阶段已经覆盖实时录音、页面刷新恢复、崩溃恢复、SQLite 保存、音频导入和重新转写六条写入路径。这里的“完成”指可修订事件基座已经可运行，不表示在线流式 ASR 或多人识别已经接入。

## 数据契约

统一事件至少包含：

- 身份：`schema_version`、`event_id`、`meeting_id`、`session_id`、`utterance_id`、`revision`。
- 语义：`event_kind`、`is_stable`、`text`、`language`。
- 时间：`start_ms`、`end_ms`、`created_at`，并暂时保留原有秒级字段。
- 来源：`audio_source` 与旧字段 `source`。它表示麦克风或系统媒体，不表示说话人。
- 说话人：`speaker_id` 与说话人元数据。它表示真实或匿名发言人。
- ASR：provider、model、confidence、latency，以及供应商事件 ID。
- 追踪：`replaces_event_id` 与 `trace_id`。

旧版事件没有上述字段时，归一化层会生成确定性的兼容身份，保持已有录音和页面恢复可用。

## 合并与回放规则

1. 不同 `utterance_id` 分别保留。
2. 同一句话优先接受更高 `revision`；同一 revision 允许 partial、final、correction 等不同不可变投递共存。
3. 完全重复的事件是幂等操作，不重复写入或显示。
4. 同一 `event_id` 内容不一致属于供应商协议冲突，第一次交付生效；不同 event ID 的同 revision 事件保留在审计历史中。
5. 同一 revision 依次比较 stable、事件类型、`created_at` 和 `event_id`，保证乱序输入得到相同结果。
6. 当前视图按单调音频时间排序，`sequence_id` 仅作为旧版回退顺序。
7. 历史表保留每次不可变投递，当前表只保存按上述顺序选出的最新物化结果。
8. `event_id` 在整条事件流中全局幂等；供应商复用旧 ID 发新 revision 时，所有层都保留第一次交付并拒绝后续错误事件。

## 向后兼容策略

- 保留原有 `TranscriptUpdate` 字段和 `transcript-update` 事件名，避免一次性重写所有消费者。
- 新字段先以可选字段进入 TypeScript；Rust 构造器始终发出完整事件。
- 旧数据库中的 `transcripts.speaker` 实际存储音源。本阶段保留该列，只把数据回填到新增 `audio_source`；真正说话人写入独立的 `speaker_id`。
- `transcripts.json` 继续生成，便于现有导入、导出和崩溃恢复；修订历史另行保存。

## 实现记录

### 数据流

```text
本地/在线 ASR
  -> TranscriptUpdate 兼容事件
  -> Rust / TypeScript 归一化
  -> 当前字幕物化视图
  -> IndexedDB v3 崩溃恢复
  -> transcripts.json + transcript-events.ndjson
  -> 停止录音时写入 SQLite
       |- transcripts：每句话的最新 revision
       `- utterance_revisions：不可变 revision 历史
```

UI 不再是事实来源。React 主界面与悬浮字幕只负责重放事件；录音恢复文件和 SQLite 保存同一份身份、时间、说话人及 ASR 元数据。

### 主要代码

| 层 | 文件 | 实际职责 |
| --- | --- | --- |
| Rust 事件协议 | `frontend/src-tauri/src/audio/transcription/event.rs` | 兼容旧事件、生成 v1 事件、保留未知枚举值、幂等重放和稳定冲突决胜 |
| 本地 ASR 发射 | `frontend/src-tauri/src/audio/transcription/worker.rs` | 为每个 VAD 完整分段生成 session、utterance、event、时间与 ASR 元数据 |
| 前端重放 | `frontend/src/lib/transcript-events.ts` | 统一主界面、刷新恢复和悬浮字幕的 revision 合并规则 |
| 浏览器恢复 | `frontend/src/services/indexedDBService.ts` | IndexedDB v3 保存完整 revision，并按 event ID 与 revision 幂等 |
| 页面状态 | `frontend/src/contexts/TranscriptContext.tsx` | 录音期间接收事件，刷新后合并后端、IndexedDB 与刷新期间新到事件 |
| 悬浮字幕 | `frontend/src/app/caption-overlay/page.tsx` | 使用相同历史门控和物化规则，不再只凭 sequence 去重 |
| 录音恢复 | `frontend/src-tauri/src/audio/recording_saver.rs` | 独立持久化句柄、原子写当前视图和 NDJSON 历史、停止时最终落盘 |
| SQLite 迁移 | `20260901000000_add_revisioned_transcript_events.sql`、`20260902050000_allow_same_revision_transcript_events.sql` | 扩展当前表、建立历史表，并兼容升级为“同 revision 多 event ID”审计模型 |
| SQLite Repository | `frontend/src-tauri/src/database/repositories/transcript_event.rs` | 事务内追加 revision、幂等检测、过期保护和当前视图更新 |
| 保存入口 | `frontend/src-tauri/src/database/repositories/transcript.rs` | 创建会议并保存全部 revision；同批错误复用的 event ID 不再回滚整场会议 |
| 导入与重转写 | `audio/import.rs`、`audio/retranscription.rs` | 统一生成 canonical final 事件；重转写在同一事务替换历史表和当前表 |

### 存储格式

- `transcripts.json`：格式版本 2.0，只保存每个 `utterance_id` 的当前版本，兼容已有恢复和导出流程。
- `transcript-events.ndjson`：保存所有唯一 event ID 的到达顺序，包括同 revision 的 partial/final 替换链。录音热路径逐条追加并同步数据，避免流式 partial 造成 O(n²) 全文件重写；停止录音时再用临时文件原子替换为干净版本。崩溃导致的末尾半行会作为 warning 跳过，不影响此前有效事件。
- IndexedDB：数据库版本 3；`eventKey` 对应 meeting、utterance、revision，`eventId` 索引用于全局交付幂等。
- SQLite `transcripts`：供原有会议详情、搜索、分页接口读取的最新物化表。
- SQLite `utterance_revisions`：审计、修订传播和后续实时摘要的历史事实表。

### 导入和重转写策略

- 导入音频的每个完成片段写成 revision 0、`final`、stable，音源为 `imported`。
- 重新转写采用“替换基线”策略：在一个事务中同时删除该会议两张转写表的数据，再用新的 canonical final 事件重建；任一步失败则整体回滚。
- 后续如果产品需要保留重转写前后的对照，应新增独立转写版本或 correction session，不能把两套基线混写到当前表。

## 遇到的问题与解决办法

### 旧 `speaker` 字段语义错误

问题：旧迁移把 `speaker` 描述为“microphone/system”来源。如果直接复用为多人识别结果，历史数据会把设备误当作发言人。

解决：保留旧列兼容已安装数据库，新增 `audio_source` 并回填；新建真实 `speaker_id`。API 中也明确区分两个概念。

### 只按 `sequence_id` 去重无法支持修订

问题：当前 React 上下文和悬浮字幕发现相同序号就丢弃，新 partial 无法被 final 替换。

解决：改为按 `utterance_id` 查找当前版本，再比较 revision 和稳定状态；旧事件才退回 sequence 追加逻辑。

### 本地 Whisper 的短分段被永久标成 partial

问题：Whisper 根据音频是否短于 15 秒判断 partial，但实时 VAD 强制约 2.5 秒切段，因此本地转写几乎全部成为 revision 0 的不稳定事件，而且不会再产生 final。未来只消费 stable 的实时摘要会完全没有输入。

解决：明确“partial 是供应商持续修订同一句的协议状态”，不能由音频时长推断。本地 Whisper 每次处理的是一个已经闭合的 VAD 或导入分段，因此完成推理后直接发 final；真正流式供应商以后自行声明 partial/final。

### 停止录音时尾部事件到得太晚

问题：旧停止流程先把 `RecordingManager` 移出全局并移除 Tauri 监听器，再等待 ASR 队列排空。force flush 产生的最后几段能显示在前端，却无法进入恢复文件。

解决：监听器改为持有独立、可克隆的 `TranscriptPersistenceSink`，不再依赖全局 manager；它会一直保留到 worker 完成。写文件增加串行锁，最终保存也获取同一锁，避免两个原子替换互相覆盖。force flush 失败时把 manager 放回全局，让用户可以重试停止而不是进入半关闭状态。

### 刷新页面后 revision 历史和新事件互相覆盖

问题：刷新不会再次触发 `recording-started`，所以 meeting ID 丢失；同时，从后端和 IndexedDB `await` 快照期间可能又到一条 final，随后旧快照会覆盖这条新事件。

解决：用 sessionStorage 恢复当前 recovery meeting ID。提交刷新结果前，同步合并 IndexedDB 历史、后端历史和此刻的 live ref，其中 live 事件最后进入 reducer；悬浮字幕采用相同方法。

### WebView 崩溃会漏掉 Rust 已落盘的尾部字幕

问题：旧恢复流程只读 IndexedDB。若前端 WebView 先崩溃，而 Rust 录音线程仍把最后几条事件写入 NDJSON，这些尾部事件虽然在 D 盘，却不会进入恢复后的 SQLite。

解决：新增受限的原生恢复命令。它只接受录音文件夹，canonicalize 后必须位于当前 recordings 根目录内，并且只读取固定文件名；优先逐行回放 `transcript-events.ndjson`，缺失或没有有效事件时回退 `transcripts.json`。重复 event ID、损坏尾行转为带行号的非致命 warning。前端先加载该日志，再与 IndexedDB 历史用同一 reducer 合并，因此任一侧独有的尾部都能恢复。

### 高频 partial 写盘导致延迟随会议增长

问题：每到一个事件都同步重写完整 `transcripts.json` 和完整 NDJSON，事件数为 n 时累计写入量接近 O(n²)。接入在线流式 ASR 后，partial 频率会明显放大字幕延迟和磁盘写入。

解决：NDJSON 热路径改为单事件追加，稳定事件才刷新兼容用的当前快照；停止时在同一持久化锁下原子重写两份最终文件。恢复读取器已显式容忍追加文件的损坏末行。

### retraction 会显示成空白墓碑，旧事件还能把它复活

问题：事件类型虽然声明了 `retraction`，但各层把它当成最高优先级普通文本。若简单从 UI 删除当前行，又会因物化状态丢失墓碑 revision，让迟到旧事件重新显示。

解决：Rust 和前端回放都保留 retraction 作为最新内部版本，只在输出当前可见快照时过滤；SQLite 将 retraction 保留在历史表并删除当前表记录，随后按 revision、稳定性、事件类型、创建时间和 event ID 计算历史 head。只有在该顺序中更高的事件才能显式恢复该句。

### `event_id` 复用会让 SQLite 整场回滚

问题：一个错误供应商可能用旧 event ID 发送更高 revision。历史表的 event ID 是主键；如果前端保留两条，停止保存会触发唯一约束并回滚整场会议。

解决：Rust 重放、恢复文件、TypeScript 历史、悬浮字幕和 IndexedDB 都统一为“同 event ID 第一次交付生效”。SQLite 保存入口再做一次同批去重作为最后防线，并记录诊断警告。

### 已执行的迁移不能直接改写

问题：最早的历史表迁移曾用 `(meeting_id, utterance_id, revision)` 唯一约束。直接编辑该迁移虽然能让全新测试数据库通过，却会让已经运行过 Meetily 的便携数据库出现 sqlx 校验和不一致，而且旧数据库中的约束仍然存在。

解决：保留原迁移内容不变，新增按顺序执行的 `20260902050000_allow_same_revision_transcript_events.sql`。新迁移在事务内重建历史表、复制全部既有事件并恢复索引，只移除旧的三列唯一约束；event ID 主键和会议外键继续保留。这样现有 `portable` 数据可以原地升级，不需要删除数据库。

### 导入和重转写绕过 revision Repository

问题：两个路径仍直接写旧版 `transcripts` 七列。导入没有历史；重新转写只删当前表，会留下与新文本不一致的旧 revision。

解决：导入统一调用 `TranscriptEventsRepository`。重新转写在单事务内清理并重建两张表，任何错误都不会产生一半新、一半旧的数据。

### 跨语言时间字段出现小数毫秒或负秒

问题：TypeScript 直接把秒乘以 1000，可能得到 1234.5；Rust API 需要整数毫秒，整场 JSON 反序列化会失败。负秒虽然被 `start_ms` 截为零，旧 `audio_start_time` 仍会让数据库校验失败。

解决：前后端都使用非负、四舍五入的整数毫秒，再从 canonical 毫秒反算兼容秒字段；ASR latency 也采用相同规范。

### 新供应商枚举值被 Rust 改写成 unknown

问题：早期 Rust `serde(other)` 会吞掉未来的事件类型或说话人状态，而 TypeScript 会保留原字符串。

解决：Rust 枚举使用自定义序列化；已知值有明确类型，未知值携带原字符串并可无损往返。当前已对齐 correction、speaker update、language update、retraction 及说话人状态集合。

### 构建工具意外触碰 C 盘缓存

问题：阶段早期有一次系统 Cargo 被误调用，在 C 盘 Cargo registry/git 缓存新增或更新约 106 MB。只读审计确认范围后，自动精确删除被安全策略拦截，因此没有强行绕过删除。

解决：之后的 Cargo、Rustup、TEMP、LLVM 和 ONNX Runtime 均显式指向 `portable/` 下的 D 盘路径。此缓存不是 Meetily 运行依赖；交付时会给出只针对审计范围的人工清理说明。

### 调试日志泄露会议正文

问题：旧保存路径会把 renderer 传入的首段完整 JSON、转写文本片段、会议标题和录音目录写入调试日志；即使日志位于 D 盘，这仍然扩大了敏感会议内容的副本数量。

解决：保存链路和前端录音停止诊断只记录段数、字符数、时间范围以及路径是否存在，不再记录正文、标题或真实目录。转写事件仍只写入用户明确选择的会议数据与恢复文件。

## 验证记录

- Rust 事件协议：9 个测试通过，覆盖旧事件、revision 0 到 final、乱序、重复、同版本冲突、fallback event ID 和未知枚举无损往返。
- RecordingSaver：9 个测试通过，覆盖重复、过期 revision、同版本 final、旧 sequence 兼容、retraction、独立 sink 尾部持久化、NDJSON 损坏尾行、JSON 回退和越界路径拒绝。
- SQLite：真实迁移与 Repository 的 8 个独立 D 盘测试通过 revision 0→1→2、同 revision partial→final、重复 event ID、乱序、同优先级事件与交付顺序无关、retraction 防复活、旧数据回填和说话人元数据。
- 导入与重新转写：各 1 个内存 SQLite 测试通过；重转写测试确认双表原子替换。
- 前端事件测试：`node tests/lib/transcript-events.test.mjs` 通过，覆盖毫秒规范、负时间、刷新期间 live final、说话人修订、三事件 event ID 复用，以及 retraction 与更高 revision 恢复。
- 为既有 Bun 测试补充了仓库内最小 `bun:test` 类型声明；TypeScript 全量检查现已通过，且没有新增包或下载依赖。
- Rust 相关文件通过 `rustfmt --check`；编译只报告项目已有 warning。

所有 Rust 验证均显式使用 `<project-root>\portable\` 下的工具链、TEMP 和 ONNX Runtime。

## 尚未包含

- 本阶段不改变音频采集拓扑，麦克风与系统音频分轨属于阶段 02。
- 本阶段不连接云端 ASR，也不下载 MOSS 模型。
- 本阶段只建立说话人字段和修订机制，不宣称已经完成多人分离。
- 同一 revision 的不同 event ID 会在 SQLite 审计历史中共存，但当前前端历史数组仍只保留确定性胜者；阶段 03 的供应商适配器必须生成应用级唯一 event ID，不能依赖供应商不稳定的交付 ID。
- 内部保存边界会同时提供 nested 与 flat 说话人/ASR 字段；未来如果开放第三方直接调用保存 API，需要在入口补 nested fallback 校验。
