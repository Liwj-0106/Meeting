# 06：低延迟双语文字字幕

## 当前交付到哪里

这一阶段已经交付首个可运行的低延迟双语文字字幕切片。用户可以在中文设置页启用翻译、选择自动/日语/英语/韩语原文、配置 OpenAI 或 OpenAI 兼容服务，再在悬浮窗看到“原文在上、简体中文在下”的双语字幕。录音与 ASR 不等待翻译；网络变慢或失败时，原文仍继续出现。

它同时保留了上一切片建立的严格版本边界：日语或英语原文被 ASR 修订、MOSS 改写说话人、用户撤回一句话后，旧中文译文不能继续显示，也不能因网络响应迟到而重新出现。

当前可运行内容包括：

- Rust 中独立于供应商的翻译事件、错误、请求指纹和术语表版本类型；
- 纯内存 coordinator，支持 partial 防抖合并、final 优先、取消、代次门禁、撤回和有界队列回压；
- 确定性 fake provider，用合成文本验证乱序和迟到响应，不访问网络；
- SQLite 不可变翻译历史、最新投影和术语表版本表；
- TypeScript 纯 reducer，只显示与当前原文版本严格匹配的完整译文快照；
- Rust-only OpenAI-compatible HTTP adapter、HTTPS/回环地址策略、超时和安全错误；
- Rust-only 密钥读写命令，WebView 只接收 `has_api_key`；
- overlay-open 旁路服务：前端只提交不可变 `event_id`，Rust 从当前录音历史解析完整原文；
- 中文设置卡片，以及原文优先、待翻译和失败状态低干扰的双语悬浮字幕；
- SQLite 会话暂存：完整译文在会议保存前也会按原文版本落盘，悬浮窗重载后直接恢复，不重复请求模型；
- repository adapter：活动录音先在 Rust 中有界暂存完整译文；会议和原文版本保存后，再按可信 ASR session 事务绑定到翻译历史与 latest；
- Rust 与 Node 自动化测试。

当前限制需要先看清：HTTP adapter 读取的是一次完整 Chat Completions 响应，并非供应商 token 流；尚未接 DeepL、本地翻译模型、术语表编辑 UI 和自动重试面板。已经形成完整事件的译文会持久暂存并随会议绑定；只有整个 session 与其 pending 录音绑定都超过 7 天未更新的放弃保存会话，或同样过期且没有录音绑定的孤儿暂存，才会按 session 清理。进程退出时仍在等待供应商响应的网络请求没有完整译文可保存，重启后需要重新请求。API 密钥保存在本机 Meetily SQLite 中且不会返回 WebView，但尚未迁移到 Windows 凭据库。

这里的产品能力称为“低延迟双语文字字幕”。它先识别语音，再翻译识别文本；不生成译后语音，也不宣称语音同传。

## 怎样试用当前切片

1. 源码有变化时先用项目根目录的 `scripts/run-meetily-build-portable.cmd` 重建，再用 `run-meetily.cmd` 启动；运行数据继续进入 D 盘便携目录。
2. 打开“设置 → 转写 → 双语实时字幕”，开启在线翻译。
3. 原文语言选择“自动检测”或明确选择日语、英语、韩语；当前译文固定为简体中文。
4. 选择 OpenAI 或 OpenAI 兼容服务，填写模型；兼容服务还需填写 API 根地址。
5. 单独保存 API 密钥，再保存双语字幕设置。密钥输入框保存后会清空，只显示“已配置”。
6. 打开悬浮字幕并开始录制。观看外语视频时选择系统媒体音频；原文先出现，译文随后补在下方。

若只看到原文，先检查翻译开关、密钥状态和悬浮窗是否实际显示。自动化测试不会使用你的密钥，也没有替你访问真实云服务；首次真实请求需要你自行确认提供商计费与隐私条款。

## 字幕怎样流动

原文字幕优先出现，译文随后补上。网络翻译变慢或失败时，录音与原文 ASR 继续运行。

```text
麦克风 / 系统媒体
  -> 本地或在线流式 ASR
  -> revisioned TranscriptEvent
       |- 原文字幕立即显示
       `- TranslationSourceBinding
            -> SourceBinding + RequestFingerprint
            -> 防抖与有界队列
            -> Rust-only OpenAI-compatible adapter
            -> 完整 TranslationEvent 快照
            -> SQLite 会话暂存；保存会议后校验并事务写历史表
            -> 前端三元组与 source kind 校验
            -> 原文在上、中文译文在下的悬浮字幕
```

翻译永远是旁路消费者。它没有权限阻塞录音、修改原文或伪造说话人。

## 每条译文必须认准原文版本

`TranslationSourceBinding` 使用三个字段识别原文：

| 字段 | 解决的问题 |
| --- | --- |
| `event_id` | 防止供应商或重放链路复用一次投递身份 |
| `revision` | 识别同一 utterance 的 partial、final、correction 和 speaker update 顺序 |
| `text_hash` | 防止“修订号相同但原文内容不同”的异常响应混入 |

`meeting_id` 和 `utterance_id` 提供存储范围，不能替代上述三元组。Rust coordinator 和 TypeScript reducer 都要求三个字段同时匹配。只匹配 utterance ID 或 revision 会让迟到译文覆盖新原文，因此不被接受。

`source_kind` 不改变这组三元组的身份含义，它补充了同一 revision 内的原文新鲜度。优先级依次为 `partial < final < correction < speaker_update / language_update < retraction`。因此同为 revision 0 时，新的 final 事件可以替换先到的 partial 事件；随后迟到的 partial 不能再把 final 倒退回去。Rust、SQLite 和 TypeScript 使用同一顺序。

`text_hash` 是传给翻译器的完整 `source_text` UTF-8 字节的 SHA-256 小写十六进制值。Rust 不再只检查格式：`TranslationSourceEvent::validate` 会重新计算哈希并要求完全相等。上游若要折叠空格、换行或做 Unicode 归一化，必须先得到唯一的规范文本，再同时生成 `source_text` 和哈希；provider 与前端不能各自改写文本。

### 原文没变，为什么仍可能需要重译

原文三元组只回答“译的是哪一版原文”。翻译结果还受另一组输入影响，因此请求身份是：

```text
(TranslationSourceBinding, TranslationRequestFingerprint)
```

`TranslationRequestFingerprint` 包含源语言、目标语言、provider、model，以及可选的 `glossary_id + version + content_hash`。这些字段都采用已校验、可比较的结构化值，不把密钥、提示词正文或请求正文写进指纹。同一原文从术语表 v1 切到 v2，或更换 provider/model 时，会取消旧请求、增加 generation 并重新翻译；不能被判成重复原文。

### 为什么不保存 token delta

流式翻译供应商可能逐 token 返回内容。token delta 只适合连接内临时拼接：重连、重试和不同供应商会产生不同切分，直接持久化很难幂等恢复。

本阶段只允许三种完整事件：

- `snapshot`：一条完整 partial、final 或复用译文；
- `retraction`：与当前源事件绑定的撤回 tombstone；
- `error`：供应商无关、长度受限的安全错误。

未来 adapter 可在 Rust 内存中组装 token，只有形成完整快照后才写事件和数据库。

## 调度器如何保持实时又不丢 final

`TranslationCoordinator` 是同步、纯内存契约状态机，测试默认 partial 防抖为 350 毫秒；实际 overlay runtime 使用 300 毫秒 partial 防抖、最多 32 个活动 utterance 和 4 个并发 provider 请求。两者都遵守 final 优先、generation gate 和迟到响应丢弃规则。

| 输入 | coordinator 行为 | 用户可见结果 |
| --- | --- | --- |
| 连续 partial | 同一 utterance 只保留最新版本，并从最后一次更新重新计时 | 少请求、少闪烁 |
| final 或 correction | 放到所有等待 partial 之前；必要时取消正在处理的 partial | 稳定原文优先拿到稳定译文 |
| speaker update，原文哈希与完整请求指纹相同 | 复用已完成 final，同时生成绑定到新源三元组的 `reused` 事件 | 改说话人不会重复付费翻译 |
| 原文、语言方向、provider/model 或术语版本变化 | 增加 generation，旧请求即使返回也被丢弃 | 新配置会重译，旧译文不会复活 |
| retraction | 取消同 utterance 的等待或进行中请求，生成 retraction | 译文立即失效 |

取消供应商请求只能节省资源，不能作为正确性保证。HTTP/WebSocket 请求可能在取消后仍送达，因此 generation、原文三元组和完整请求指纹校验才是最终门禁。

### 有界队列的 final 回压

partial 可以被合并、丢弃或由 final 淘汰。队列全是 final 时，新 final 通过 `RetryFinal(source)` 把完整输入所有权交还调用者，不修改当前 source head，也不静默丢弃。持久化型会话接线必须先保存原始 TranscriptEvent，再在消费通道可写时重试该返回值。

这项设计把“不会丢 final”变成可观察的调用约束。当前 overlay runtime 已作为旁路消费者接入活动录音历史，但还不是可恢复的 durable queue；应用退出时尚未完成的在线请求不会恢复。

## 供应商边界

`TranslationProvider` trait 只有四个职责：报告 provider/model、启动完整请求、接受尽力取消。响应通过独立入口回到 coordinator。请求和响应的 `Debug` 输出只显示字符数与哈希占位符，不输出原文和译文正文。

`DeterministicFakeTranslationProvider` 记录启动与取消操作，并按指定请求产生可重复响应。它用于验证：

- partial 合并后只发送最新文本；
- final 取消 partial 后，迟到 partial 响应被 generation gate 拒绝；
- 响应篡改 event ID、revision、text hash、provider 或术语版本时整条失败；
- provider 启动失败只产生翻译错误，不影响 ASR 和录音。

首个真实 adapter 已实现 OpenAI Chat Completions 兼容协议。OpenAI 使用固定根地址并显式发送 `store: false`；自定义兼容服务不强塞这个扩展字段。自定义服务只允许 HTTPS，只有 `localhost`、`127.0.0.1` 和 `::1` 可使用 HTTP。URL 中的用户名、密码、查询参数和片段会被拒绝，HTTP redirects 被关闭，数据库中读出的配置还会在每次请求前重新校验。授权头被标为 sensitive，响应体上限为 256 KiB，连接、超时、HTTP 和 JSON 错误都转换为固定中文消息；供应商响应正文不会进入 WebView 错误或日志。

设置页明确提示计费、文本离开本机和供应商保留策略。只有“已启用翻译 + 悬浮字幕窗口实际可见”同时满足时，Rust 命令才接受请求。WebView 只提交 `event_id`，不能提供或篡改原文正文；完整文本由 Rust 从当前录音历史重新解析。密钥设置/删除命令只返回 `has_api_key`，序列化和 `Debug` 测试都验证不会泄漏密钥。

## SQLite 保存什么

迁移 `20260902060000_add_revision_bound_translations.sql` 新增四组翻译事件结构；`20260902065000_add_live_translation_settings.sql` 再增加单例 `live_translation_settings`，保存启用状态、语言方向、provider/model、endpoint 和可选密钥：

| 表 | 用途 |
| --- | --- |
| `translation_glossaries` | 术语表身份、语言方向和可选会议范围 |
| `translation_glossary_versions` | 不可变的完整术语数组与内容哈希 |
| `translation_revisions` | 每次完整译文、撤回或失败的不可变历史 |
| `translation_latest` | 悬浮字幕读取的当前投影 |
| `live_translation_staging` | 尚无会议 ID 时按可信 ASR session 保存完整译文，供重载恢复和会后绑定 |
| `live_translation_settings` | 在线翻译公开设置和 Rust-only 密钥列 |

`translation_revisions` 通过复合外键确认 `source_event_id + meeting_id + utterance_id + source_revision + source_kind` 指向同一条 `utterance_revisions` 记录。SQLite 负责哈希格式约束，Rust 在写入前负责 `SHA-256(source_text)` 的内容核对。表级约束还会拒绝：

- 非法状态与 event kind 组合；
- partial/final/reused 缺少完整译文或 provider；
- retraction/error 携带译文；
- 负 revision、generation 或 latency；
- 不完整的术语表版本绑定；
- 非 64 位小写十六进制哈希。

插入历史后，latest trigger 按以下顺序判断新鲜度：先比较 `source_revision`，同 revision 再比较 `source_kind` 优先级；source revision 与 kind 优先级都相同时，只有 `source_event_id + source_text_hash` 仍匹配当前原文，才比较 `generation`，最后比较 `translation_revision`。因此 revision 0 的 final event B 能替换先到的 partial event A，之后带更大 generation 的迟到 partial 仍不能回滚；“旧原文 revision 1 / translation revision 11”也不能覆盖“新原文 revision 2 / translation revision 10”。迟到或异常旧行仍留在历史中供审计，但不会倒退当前投影。

每个会议、utterance、目标语言范围内，generation 与 translation revision 都有独立唯一约束。`translation_latest` 使用包含 event ID 和完整 scope 的复合外键，不能手工指向另一个 utterance 的历史行。`reused_from_event_id` 还受 trigger 约束：只能引用同会议、同 utterance、同目标语言的已完成 `final/reused`，并且原文哈希、完整译文、provider/model 和术语版本一致；失败事件不能作为复用来源。

coordinator 为 request/event ID 生成 UUID，并提供 `restore_latest_source`。live repository 会在新请求前读取该 meeting/utterance/target 的 generation 与 translation revision 高水位，重启后继续递增，避免和已存历史发生唯一键冲突。契约恢复仍要求同时恢复最新 source head、当时的请求指纹和两个高水位；这样旧 source 重放会得到 `StaleSource`，provider 或术语配置已变化时，同一 source triple 仍会生成新 generation，而不是误判重复。

repository 写入前先按 `event_id + meeting_id + session_id + utterance_id + source_revision + source_kind` 查找规范原文，并重新计算原文 SHA-256。活动字幕只有 ASR session ID 时会返回 `SourceNotPersisted`，runtime 将 Rust provider 产生的完整事件同时放入有界内存队列和 `live_translation_staging`。暂存行保存 session、utterance、source event、revision、kind、文本哈希、目标语言和两个单调计数，并用受约束 JSON 保存完整事件；关键 JSON 路径必须存在且类型正确，同一 `translation_event_id` 的相同内容是幂等重放，内容冲突会被拒绝。逐字稿保存成功后，保存命令使用 Rust 准备好的可信 session ID；崩溃恢复时该身份来自持久化录音绑定，而不是 renderer 字段。数据库与内存中的暂存事件随后去重、改绑到新建的 meeting ID。历史写入和已消费暂存行删除处于同一个 SQLite transaction 中。

悬浮窗重载后仍只提交当前转写的不可变 `event_id`。Rust 重新解析当前 segment，并仅在 session、utterance、source event、revision、kind、文本哈希、语言方向、provider、model 和术语版本全部匹配时重放暂存快照。设置变化会产生新请求；旧 revision 即使计数更大，也无法命中新的 source 绑定，更不能覆盖 `translation_latest`。如果进程恰好在 meeting/session 事务提交后、译文提升前退出，启动、读取翻译设置或下一次排队时会从 `recording_session_meeting_bindings` 做可信数据库关联并幂等补偿，不采用 renderer 提供的映射。

同一 session 重复绑定同一 meeting 会返回幂等结果；改绑到另一个 meeting 会被拒绝。不存在的原文、session 不匹配、哈希不匹配，以及已经被 correction 或更高优先级事件替换的旧原文都不能落库。当前 `reused` speaker update 所依赖的旧完整快照是唯一例外：repository 只把它作为当前复用事件的外键前置行写入，latest 仍由当前 source 决定。内存绑定映射保留 60 分钟，因此在会议保存后才结束的最长 25 秒 provider 请求仍能走相同校验补写。持久暂存不会按这段内存 TTL 删除：成功绑定时在同一事务中消费；清理逻辑同时比较整个 session 的最后暂存时间与 pending 绑定更新时间，只删除两者都超过 7 天的放弃保存会话，并始终保护当前写入 session 与所有 bound 映射。恢复提升会逐 session 隔离事务，损坏会话留在暂存并汇总报错，排序靠后的正常会话仍会完成提升。

术语条目以完整 JSON 数组按版本保存。编辑术语会创建新版本，并改变后续请求的 glossary binding。即使原文哈希相同，使用不同术语版本也不能复用旧译文。当前恢复 API 尚未恢复“已完成译文的内存复用缓存”，所以重启后的 speaker-only update 可能多做一次翻译，但不会显示旧版本或倒退计数。

## 前端 reducer 怎样阻止旧字幕

`translation-events.ts` 不依赖 React 或悬浮窗。它先注册当前 transcript source，再接受 translation event：

1. 新 source revision 或同 revision 下更高优先级的 source kind 到达时，立即隐藏旧三元组的译文；迟到的低优先级 source 不会回滚当前原文。
2. translation event 必须与当前 source 的 event ID、revision、text hash、source kind 和 source language 一致，并携带自洽的 request fingerprint。
3. 同一 `translation_event_id` 重放相同 payload 是幂等；复用 ID 修改 payload 会得到 `event_id_conflict`。
4. reducer 为每个目标语言保存 generation 和 translation revision 高水位；新事件的两个数字都必须严格增大。source 更新会清掉可见译文，但不会清掉高水位。
5. source retraction 后，任何文字快照都得到 `unbound`；只有匹配当前撤回源的 translation retraction 可以落入投影。
6. 带 `delta` 或 `token_delta` 的对象会被判定为非法。

入口参数按 `unknown` 做运行时校验。`null`、缺失 `source`、`null error`、final 携带 `reused_from_event_id`、failed 缺 provider 等畸形对象都会返回 `invalid`，不会因属性访问抛出 TypeError。完整译文/失败事件的扁平 provider、model、语言和术语字段还必须与 request fingerprint 一致。

悬浮字幕已按这套 reducer 接线。每条可修订 transcript 出现后，界面立即显示原文并异步提交 `event_id`；Rust 返回权威 source snapshot 后才注册 source。若 provider 极快、translation event 先于 invoke 响应到达，前端会把最多 64 个 source 的未绑定事件短暂缓冲，每个 source 最多 4 个；source 注册后再走同一个 reducer，缓冲不会绕过校验。

渲染层只读取仍与当前原文 event ID 对齐的 display state。默认采用原文白色、译文青绿色且字号略小的两行结构；pending 只显示低对比度“正在翻译…”，失败显示一行琥珀色安全提示，不会把原文替换为空或把整个悬浮窗切成错误页。关闭翻译、清空字幕、开始新录音或更换 provider/model 时会清理前端翻译投影并重新建立绑定。

## 实现时遇到的问题

### “队列有界”和“final 不丢”不能靠丢弃策略同时成立

固定容量队列塞满 final 后，没有一种本地淘汰策略可以保留所有 final。实现选择显式回压：partial 可淘汰，final 返回完整所有权供调用者重试。下一阶段要把这个返回值接到可恢复的会话消费者，不能把它当普通失败忽略。

### 取消不能阻止迟到响应

不同网络库和供应商对取消的语义不同。实现给每个 utterance/目标语言组合维护单调 generation；source 更新时 generation 增加。响应先过 request ID、generation 和三元组门禁，取消只用于减少浪费。

### 改说话人不应重复翻译

speaker update 会创建新的源 event ID 和 revision，但正文可能完全相同。仅当 source kind 明确为 `speaker_update`、text hash 相同、完整请求指纹相同时，coordinator 复用已完成 final，并产生新的 revision-bound `reused` 事件。普通 correction、术语升级或 provider/model 变化不能走这条捷径。

### 前端不能只按 translation revision 排序

一个迟到响应可以带更大的 translation revision，却属于旧原文。reducer 先验证源三元组与 source kind，再要求 generation 和 translation revision 同时越过各自高水位；数据库 latest trigger 则先比较 source revision 和 source kind 优先级。测试既覆盖“旧 source revision 1 / translation revision 11”晚于“新 source revision 2 / translation revision 10”，也覆盖同为 revision 0 时 partial event A、final event B、迟到 partial 的顺序。

### 极快 provider 可能先发事件，后返回命令响应

translation event 与 Tauri invoke 响应是两条异步通道，不能假设先后顺序。若事件先到，reducer 会正确返回 `unbound`，但直接丢弃会让这条译文永远不可见。实现增加了严格有界的短暂缓冲：只按 source event ID 暂存，权威 source snapshot 注册后仍要重新通过完整 reducer；超出范围会淘汰最老缓冲，不会猜测绑定关系。

### 活动录音直到保存时才有规范 meeting ID

把 session ID 伪装成 meeting ID 会污染会议列表，也会绕开原文复合外键。实现把显示和持久化分开：活动 overlay 使用 session scope 做 generation gate，Rust runtime 只暂存自己从 provider 得到的完整事件；会议保存事务提交后，repository 用可信 session、事件 ID、utterance、revision、kind 和原文哈希重新核对，再写 `translation_revisions`。绑定失败只在保存响应的 `translation_binding` 安全状态中报告，不会把已经成功保存的逐字稿回滚。

### 窗口重载会重复付费翻译

问题：前端重载会丢失 reducer 状态。旧实现只能从 Rust 内存知道请求曾完成，queue 再次收到同一个 segment 时仍可能创建新 generation 并访问供应商；进程退出还会同时丢失未绑定的完整译文。

解决：完整 TranslationEvent 生成后先幂等写入 SQLite 会话暂存表。queue 在创建网络任务前以当前 Rust 转写 segment 和当前请求配置做完整匹配，命中后直接重放原事件。会议保存时从数据库恢复整个 session 的暂存事件，因此即使 runtime 内存已经清空，也能按 exact source 绑定；SQLite latest 的 source revision 优先级继续阻止旧译文倒退投影。

### 密钥不能回显，但当前还不是系统凭据存储

公开 settings DTO 从类型上只包含 `has_api_key`；密钥包装类型的 `Debug` 始终显示 `<redacted>`，provider 错误也不回传响应正文。当前数据库列仍属于本地应用数据，并非 Windows Credential Manager。设置页如实说明这一点，后续迁移凭据库时应保留同一公开命令契约。

## 验证结果

自动化验证只使用合成日文、中文和固定哈希，没有加载模型、没有读取会议数据、没有访问真实翻译服务：

- 12 个 Rust 定向测试通过，覆盖 SHA-256 内容校验、partial 防抖合并、final 抢占、取消后迟到响应、retraction、speaker-only 复用、有界队列 final 回压、request fingerprint 篡改和恢复后的 source head；
- Rust SQLite 测试在全新内存数据库执行完整 migration，复现旧 source 携带更大 translation revision、同 revision 的 partial A→final B→迟到 partial、重复 generation/revision、复用 failed、latest 跨 scope 指针等审查用例，并确认 schema 没有 token delta 列；
- 12 个 live runtime Rust 测试通过，除密钥、endpoint、provider、generation 和 fresh-DB exact-source 场景外，还覆盖活动 session 事务绑定、重复绑定幂等、错 session/meeting 拒绝后重试、被修订源和哈希篡改拒绝、TTL、整 session 容量淘汰，以及会议保存后迟到完成的译文；
- Node reducer 测试通过，覆盖三元组每个字段、source kind 匹配与同 revision 优先级、重复 ID、两个单调高水位、术语 v1→v2、畸形 null/缺字段、严格状态变体、token delta、speaker reuse 和撤回后防复活；
- 双语字幕展示组件测试通过，确认 DOM 中原文先于译文、pending/error 不覆盖原文、字号跟随悬浮窗；
- live translation service 测试通过，确认保存设置使用 Rust 命令要求的 `settings` 外层参数，公开读取/事件只有 `has_api_key`，密钥只出现在显式 set 命令，queue 只提交 `eventId`；
- Stage 6 前端文件的 strict TypeScript 检查通过；项目全量 `tsc --noEmit --incremental false` 也通过。

契约测试缓存位于单会话根的 `stage6-translation-contract` 子目录，runtime/UI 验证的 `TEMP`、`TMP` 和 Cargo target 都指向同一会话根下的 `stage6-translation-runtime` 子目录。实现没有创建 Conda 环境、安装依赖、下载模型或发送真实云请求。

## 下一步接线

首个用户可运行切片已经形成，后续按风险顺序补齐：

1. 为尚未形成完整 TranslationEvent 的供应商请求增加显式重试入口；如需更强的本地保密，再为 SQLite 译文暂存增加数据库加密。
2. 为 provider token delta 增加 Rust-only assembler、超时重试、速率限制和连接健康状态；只有完整快照可以跨 IPC 和落库。
3. 把密钥从本地数据库迁移到 Windows 凭据库，同时保持 WebView 只有 `has_api_key` 的公开契约。
4. 增加术语编辑与版本选择，覆盖姓名、产品名、代码符号和日中英混说。
5. 通过相同 trait 接 DeepL 与本地翻译模型，提供不离开设备的隐私选项。
6. 用用户明确授权的短样本做手工端到端验收，测量原文出现延迟、译文 p50/p95 延迟、撤回传播、术语命中率、数字/专名保持率、chrF/COMET 和每小时 API 成本。

在供应商请求重试、系统凭据库和真实授权样本验收完成前，Stage 06 应标记为“在线双语字幕、完整译文重载恢复与会后可信持久化可运行；进行中的网络请求恢复与生产验收待完成”。

## 主要文件

| 文件 | 职责 |
| --- | --- |
| `frontend/src-tauri/src/audio/transcription/translation.rs` | Rust 类型、provider trait、fake provider 与 coordinator |
| `frontend/src-tauri/src/audio/transcription/translation_runtime.rs` | 安全设置命令、OpenAI-compatible adapter、非阻塞 runtime、有界暂存与可信 session 绑定 |
| `frontend/src-tauri/src/database/repositories/translation.rs` | exact-source 事务写入、session→meeting 改绑、latest 投影与高水位恢复 |
| `frontend/src-tauri/src/api/api.rs` | 逐字稿保存后的非致命翻译绑定 hook 与安全状态返回 |
| `frontend/src-tauri/migrations/20260902060000_add_revision_bound_translations.sql` | 翻译历史、最新投影和术语表版本 |
| `frontend/src-tauri/migrations/20260902065000_add_live_translation_settings.sql` | 在线翻译公开设置和 Rust-only 密钥列 |
| `frontend/src-tauri/migrations/20260902100000_add_translation_staging.sql` | session 范围的完整译文暂存、幂等约束与 exact-source 索引 |
| `frontend/src/lib/translation-events.ts` | 浏览器侧严格绑定 reducer 与可见译文选择器 |
| `frontend/src/services/liveTranslationService.ts` | 类型化命令与安全事件 service |
| `frontend/src/components/TranslationSettingsCard.tsx` | 中文在线翻译设置与隐私提示 |
| `frontend/src/components/BilingualCaptionLines.tsx` | 原文优先的双语字幕呈现 |
| `frontend/src/app/caption-overlay/page.tsx` | 活动字幕旁路提交、source 注册和 reducer 接线 |
| `frontend/tests/lib/translation-events.test.mjs` | 乱序、重复、撤回和旧译文拒绝测试 |
| `frontend/tests/components/bilingual-caption-lines.test.mjs` | 原文/译文顺序、pending/error 与字号测试 |
| `frontend/tests/lib/live-translation-service.test.mjs` | IPC 命令参数与公开密钥状态边界测试 |
