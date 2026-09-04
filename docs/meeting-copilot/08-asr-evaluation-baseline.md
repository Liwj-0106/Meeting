# 08：ASR、多人识别与实时性评测基线

## 这一阶段解决什么

“听起来还行”不能说明会议转写可用。Meetily 需要同时回答四个问题：文字错了多少、说话人是否串线、字幕多久出现、模型是否拖慢录音。仓库现已提供一个无第三方依赖的 Node.js 评测入口，用同一份版本化输入生成可比较的 JSON 报告。

当前切片已经实现 CER、WER、置换无关的多人 `cpCER`、RTF 加权值与 p50/p95、首个 partial 延迟、final 延迟以及峰值 RAM/VRAM 的计算与单元测试。它没有下载公开语料，也没有把近似指标包装成标准 DER；多人分离的正式 DER/JER 仍应由 `dscore` 等公认工具复核。

## 为什么不能只看一个准确率

| 指标 | 回答的问题 | 适用场景 |
| --- | --- | --- |
| CER | 字符插入、删除、替换占参考字符的比例 | 中文、日文及未分词文本 |
| WER | 单词插入、删除、替换占参考词数的比例 | 英文等以空格分词的语言 |
| cpCER | 先寻找参考人与匿名 speaker 的最优一一映射，再统计字符错误 | 多人会议，避免 `S01/S02` 标签交换造成假错误 |
| DER/JER | 谁在什么时候讲话、是否漏人/多人串线 | 正式说话人分离评测 |
| RTF | 处理耗时 ÷ 音频时长 | 判断本地模型是否跟得上实时音频 |
| partial/final 延迟 | 用户多久看到第一版、多久看到稳定版 | 悬浮字幕体验 |
| RAM/VRAM 峰值 | 该设备能否长期运行 | 本地 Whisper、Parakeet、MOSS 对比 |

准确率指标越低越好。RTF 小于 1 只代表平均处理速度快于音频播放速度，并不保证字幕低延迟；一个模型可能积攒 30 秒音频后用 5 秒处理完，RTF 很好，但用户仍会等待 35 秒。因此必须同时报告延迟分位数。

## 输入格式

输入是 UTF-8 JSON，`schema` 固定为 `1`。单人样本可直接给字符串；多人样本用按时间排列的话语数组。

```json
{
  "schema": 1,
  "run_metadata": {
    "run_id": "rtx4060-deepgram-001",
    "engine": "deepgram",
    "model": "nova-3",
    "model_revision": "2026-08-15",
    "device": "RTX 4060 Laptop",
    "configuration_fingerprint": "sha256:config-v1",
    "audio_manifest_revision": "consented-meetings-v2",
    "partial_latency_origin": "audio_enqueue_to_first_partial",
    "final_latency_origin": "vad_commit_to_final",
    "clock": "monotonic"
  },
  "samples": [
    {
      "id": "meeting-001",
      "scenario": "overlap-meeting",
      "audio_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
      "reference_revision": "human-r3",
      "language": "zh-CN",
      "reference": [
        { "speaker": "张三", "start_ms": 0, "text": "先确认接口边界" },
        { "speaker": "李四", "start_ms": 1800, "text": "我补充失败隔离方案" }
      ],
      "hypothesis": [
        { "speaker": "S02", "start_ms": 0, "text": "先确认接口边界" },
        { "speaker": "S01", "start_ms": 1800, "text": "我补充失败隔离方案" }
      ],
      "audio_duration_ms": 60000,
      "processing_duration_ms": 24000,
      "first_partial_latency_ms": 420,
      "final_latency_ms": 1200,
      "peak_ram_mb": 2300,
      "peak_vram_mb": 4100
    }
  ]
}
```

运行命令：

```text
node scripts/evaluation/asr_metrics.mjs input.json --output report.json
```

在 Codex 开发会话内，`input.json` 和 `report.json` 应放到当前唯一会话缓存根的子目录；产品长期评测语料则应放到明确的数据目录，不能提交含真实会议或个人声音的文件。

## 规范化与可比性

当前 CER 使用 Unicode NFKC、统一小写，并忽略空白、Unicode 标点和符号。这样可以避免全角字符或标点风格主导结果，但也意味着它不评估标点质量。英文 WER 使用空白分词，并同样忽略 Unicode 标点和符号；中文、日文、韩文及未声明语言的样本不在没有外部分词器时报告 WER，防止生成看似精确但没有意义的数字。

编辑距离只保留前后两行动态规划状态，不随参考稿总长度保存整张矩阵。脚本同时限制输入文件字节数、每段话语数、单次 token 数、单次与整份报告的动态规划预算，以及每个样本的 speaker 数；超长会议应按连续时间段切成多个样本，再用 corpus 计数汇总，避免一次错误输入耗尽内存或长时间占满 CPU。

`cp_cer_coverage` 表示具备完整参考 speaker 标注、因而可以计算 cpCER 的样本比例。参考 speaker 缺失时该样本明确标为不可评测；系统输出缺失 speaker 时不会被丢弃，而是合并为一个匿名输出流参与匹配，因此“没有做人声分离”不会得到虚假的 0 错误。多人数量不等时仍以空流补齐方阵；额外系统 speaker 会列在 `unmatched_hypothesis_speakers` 中。

空参考稿没有可作为 CER 分母的字符，因此错误率会明确为 `null`，但插入错误不能随之消失。`silence_stress` 会单独保留静音样本数、幻觉字符总数，以及具备音频时长时的每分钟幻觉字符数，用来发现“谢谢观看”等静音幻觉。

报告中的 p50/p95 使用线性插值 R7，并同时给出有效值数量和缺失数量。实时门槛应在固定窗口长度上比较；不同长度的整场会议样本 p95 只能作为粗筛，不能替代逐字幕延迟分布。

每次对比都必须固定：

- 同一音频文件及其校验值；
- 同一人工参考稿和说话人边界版本；
- 模型名称、精确 revision、量化方式与运行设备；
- VAD、窗口、语言、热词、endpointing 和并发参数；
- 冷启动或热启动条件；
- 网络 ASR 的区域、时间和供应商 API 版本。

这些条件应写入输入的 `run_metadata`，而不是只记在人脑或临时文件名里。脚本只接受固定字段，未知字段会直接报错，避免拼写错误悄悄丢失。每条样本使用 `scenario` 进入 `scenario_summaries` 分组；缺省场景会明确归为 `unclassified`。`audio_sha256` 绑定音频内容，`reference_revision` 绑定人工参考稿版本。报告会原样保留这些已验证字段，便于两次运行在比较前先检查模型、设备、配置、语料和延迟计时口径是否一致。

## 推荐数据集与采样方法

公开基线可使用 [AISHELL-4](https://www.openslr.org/111/)、[AliMeeting](https://www.openslr.org/119/)、[AMI](https://groups.inf.ed.ac.uk/ami/corpus/) 等多人会议语料。公开集不能代替产品场景，仍需建立经过同意且脱敏的内部小集，覆盖普通话、英语、日语、口音、重叠发言、远场麦克风、系统音频、专业术语、静音和噪声。

每个核心场景至少保留三类切片：干净单人用于识别模型上限，真实会议用于端到端表现，压力音频用于抢话、断网、设备切换和长时稳定性。报告必须按场景分组，不能只公布一个被简单样本稀释的总平均值。

## DER/JER 的边界

本地脚本故意不实现“近似 DER”。标准评测涉及 collar、重叠语音处理、参考与系统 speaker 映射等约定，任一默认值变化都会改变结果。正式多人验收应把 RTTM 交给固定版本的 [`dscore`](https://github.com/nryant/dscore)，同时在报告中记录 collar 和是否忽略 overlap。`cpCER` 只能说明“按人拼接后的文字是否归到正确的人”，不能代替时间边界质量。

## 建议验收门槛

门槛必须通过真实设备基线后冻结，不能把设计目标写成当前承诺。首轮可用以下工程目标做筛选：

- 在线字幕首个 partial 延迟 p95 不高于 1 秒，稳定 final 延迟 p95 不高于 3 秒；
- 本地实时模型在目标设备上 p95 RTF 不高于 0.7，给 UI、录音与重试留余量；
- MOSS 60 秒滚动窗口实验 p95 RTF 不高于 0.5，否则默认只做会后校正；
- 连续 2 小时测试录音不得丢帧，网络/模型失败不能终止双音轨录音；
- 任何优化都不得只提升总 CER，却显著恶化术语、数字、否定词或 speaker 归属。

这些数值是待验证的项目门槛。实际发布前应根据 RTX 4060 Laptop、CPU-only 设备和在线 ASR 三条配置分别建立基线。

## 已遇到的问题与解决方式

### 中文 WER 容易制造假精度

没有固定分词器时，中文 WER 会随分词策略剧烈变化。当前方案对 CJK 只给 CER，把 WER 留空；未来若引入分词器，必须固定版本并在报告中记录。

### 多人匿名标签会交换

参考稿的“张三”不可能天然等于模型的 `S01`。当前方案用 Hungarian 最小代价匹配寻找 speaker 一一映射，再计算 cpCER，并把映射写入每条样本报告，便于排查串人。

### RTF 好看但字幕仍然慢

批处理模型可拥有很低 RTF，却必须等到窗口结束才返回。当前方案把 RTF、首个 partial 和 final 延迟分开统计，并输出 p50/p95。

### 指标输入可能失控

脚本拒绝不完整的时长对、负数/非有限数、空样本、超量样本和过大文本，避免坏数据生成表面正常的报告。

## 尚未完成

- 公开数据集下载、许可证核对和统一转换器；
- RTTM 导出与固定版本 `dscore` 容器/Conda 环境；
- 从 Meetily 事件日志自动生成 hypothesis；
- GPU/RAM 采样器和音频回放时钟；
- Deepgram、OpenAI、本地 Whisper/Parakeet 与 MOSS 的同机实测报告；
- 摘要事实一致性、翻译质量和长时稳定性评测。
