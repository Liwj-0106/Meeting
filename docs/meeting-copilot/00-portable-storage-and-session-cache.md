# 00：D 盘便携存储与单会话缓存边界

## 这项能力解决什么问题

本工作区的 C 盘空间有限，Meetily 又会产生模型、录音、数据库、WebView2 用户数据、编译产物和多种工具缓存。若只迁移源码，依赖工具仍可能按各自默认规则写入用户目录，最终形成“项目在 D 盘、主要占用仍在 C 盘”的假便携状态。

本工作区因此把两类数据分开管理：

- Meetily 的长期运行数据、开发环境和构建产物放在 `<project-root>\portable\`。
- 当前 Codex 逻辑会话创建的测试、审查和一次性中间文件统一放在 `<session-cache>` 的子目录中。

“一个会话一个缓存目录”指整个持续开发会话只占用一个根目录；并行阶段只能创建其中的子目录，不能为每个阶段再创建同级日期目录。

## 当前目录职责

| 数据类型 | 位置 | 生命周期 |
| --- | --- | --- |
| 源码与长期技术文档 | `<project-root>\` | 随项目维护 |
| 录音、数据库、设置、模板和 WebView2 配置 | `portable\app-data\` | 用户长期数据 |
| 模型和推理缓存 | `portable\app-data\models\`、`portable\app-data\cache\` | 可重新下载，但可能很大 |
| Conda 运行环境 | `portable\conda-envs\` | 项目环境；MOSS 已安装在 `moss-td\` |
| Conda 包缓存 | `portable\conda-pkgs\` | 安装和修复环境时复用；不属于运行必需数据，当前已在环境验收后清理 |
| Rust、pnpm 与构建工具状态 | `portable\cargo*`、`portable\rustup`、`portable\pnpm-*` 等 | 项目开发环境 |
| 已暂存的便携程序 | `portable\runtime\` | 可运行产物 |
| 本次 Codex 会话临时文件 | `<session-cache>` | 会话结束后可按根目录整体清理 |

`portable\app-data\` 可能包含真实会议和密钥，因此它不是源码，也不是调试样本。开发和验证不得读取、展示或提交其中的业务内容。

## MOSS 当前占用

截至 2026 年 9 月 2 日，Stage 4.4 的本地 MOSS 运行环境、模型、受审查源码和已暂存程序都位于项目的 `portable\` 目录。下表不包含录音、数据库、普通应用缓存和 Conda 包缓存。

| 内容 | 位置 | 实际大小 |
| --- | --- | ---: |
| Python 与 CUDA 推理环境 | `portable\conda-envs\moss-td\` | 8,128,971,293 B |
| MOSS 模型目录 | `portable\app-data\models\moss-transcribe-diarize\` | 1,833,091,806 B |
| 已暂存程序 | `portable\runtime\` | 206,224,355 B |
| 固定版本的上游源码 | `portable\sources\MOSS-Transcribe-Diarize\` | 691,053 B |
| 合计 | 以上四项 | 10,168,978,507 B，约 10.17 GB |

源码固定在提交 `cb765f2b0fe6f7a298aa2002e2281ae693d1f3c3`，模型固定在修订 `902e98bcb3db33ac913d3496127b92a8d81f2daa`。主权重 `model-00000-of-00001.safetensors` 为 1,817,113,576 B，SHA-256 为 `9a0ceb4ab7330357db3ff583dba8d83625d5b733b00e1d55d6970e11b07026c4`。运行时会验证本地工件并使用 `local_files_only` 加载，不会在识别过程中联网补文件；首次安装或修复缺失依赖仍需要下载。环境的直接依赖已经固定版本并验证，完整 `pip freeze` 也已记录；传递依赖尚未全部用文件哈希锁定，因此需要复现当前环境时，保留已验证的 `moss-td` 目录最稳妥。

## 实际采用的方案

### 1. 应用启动先确定唯一数据根目录

Windows 版只接受两种情况：

1. 可执行文件位于本仓库的 `portable\runtime\` 或 `portable\cargo-target\` 中，程序自动推导相邻的 `portable\app-data\`；
2. 启动前显式设置绝对路径 `MEETILY_DATA_DIR`，且该路径不在 C 盘。

路径缺失、为相对路径、指向 C 盘、使用不受支持的设备命名空间，或通过 D 盘连接点最终落到 C 盘时，当前工作区构建会停止启动。它不会静默回退到 Windows 用户目录，也不会自动复制或删除旧数据。

### 2. 在创建窗口前重定向运行时目录

Rust 启动代码在 Tauri 创建窗口之前设置临时目录、WebView2 用户数据、XDG、Hugging Face、Torch、ONNX、CUDA、pip、uv、Conda、Python、Numba、Triton、Matplotlib 等常用路径。这样即使直接启动便携目录中的 `meetily.exe`，仍能获得与批处理启动器一致的运行数据位置。

### 3. 开发脚本不使用用户目录回退

便携启动、开发和构建脚本显式指定 Cargo、Rustup、pnpm、npm、Corepack、node-gyp、sccache 和编译输出目录。缺少便携工具链时脚本直接报错，避免悄悄调用 C 盘里的全局工具并留下缓存。

应用内更新只允许检查版本，不允许直接下载和安装。源码更新后由便携构建脚本重新生成 `portable\runtime\`，防止安装器把程序迁回系统安装目录。

### 4. Codex 临时文件与应用运行数据隔离

Codex 的测试目标、审查输出和辅助脚本临时文件只使用当前会话根目录的子目录。便携开发、构建脚本接受可选的 `MEETILY_SESSION_TEMP`；本次开发将它指向当前唯一会话根的子目录，脚本会拒绝 C 盘值。未设置该变量时，普通用户构建和 Meetily 自己运行时产生的临时文件仍放在 `portable\app-data\temp\`。两者职责不同：会话缓存可以整体删除，应用运行数据则可能包含恢复录音所需的状态。

### 5. 整体迁移保持相对布局

后续迁移时应先完全退出 Meetily，并确认没有构建、Conda 安装或 MOSS Python worker 正在运行，再移动整个 `<project-root>\`。不要只复制 `meetily.exe` 或单独搬模型；`portable\runtime\`、`portable\app-data\`、`portable\conda-envs\` 和 `portable\sources\` 的相对位置共同组成可运行布局。

迁移到新的非 C 盘目录后，从新目录的 `run-meetily.cmd` 启动，并检查录音保存位置。录音目录偏好是绝对路径，若它仍指向旧位置，需要在设置中重新选择。Conda 环境比模型更容易受路径变化影响；跨目录或跨盘迁移后应运行 `node scripts\setup-moss-portable.mjs --env-only` 校验环境。若迁移后的 Python 已无法启动，需要在新位置重建 `portable\conda-envs\moss-td\`。模型校验可运行 `node scripts\setup-moss-portable.mjs --model-only`，已验证的完整模型不会重复下载。

`portable\conda-pkgs\` 保存 Conda 下载和解包缓存。MOSS 环境安装成功且所有 Conda、Python、构建进程都已退出后，可以清理这个目录来回收空间；现有 `portable\conda-envs\moss-td\` 仍可运行，后续修复或重建环境时会重新下载所需包。不要把 `portable\conda-envs\moss-td\` 或 `portable\app-data\models\moss-transcribe-diarize\` 当作缓存删除。

本次 Stage 4.4 验收后执行了白名单清理：先逐个验证目标位于当前会话根或项目 `portable\` 根内，拒绝符号链接、连接点和重定向祖先，再删除 Cargo target、Conda 包缓存、MOSS 冒烟临时音频和已归档的阶段测试目录。共删除 35,633,747,495 B（约 33.19 GiB）可重建数据；保留了 `runtime`、`moss-td`、模型、固定源码、录音/数据库以及最终测试证据。随后 Stage 9 再次按同一白名单清理 23,901,049,190 B（约 22.26 GiB），累计回收 59,534,796,685 B（约 55.45 GiB）。清理后的当前会话缓存只保留 `stage8-eval-review\`，实测约 26 KB。

## 实现中遇到的问题与解决办法

| 问题 | 风险 | 处理方式 |
| --- | --- | --- |
| 直接双击可执行文件会跳过批处理环境变量 | WebView2、模型或临时文件回到 C 盘 | 根据可执行文件所在的便携目录推导数据根目录；无法推导时失败关闭 |
| FFmpeg 解码曾把临时 WAV 写在导入媒体旁边 | 选择 C 盘视频时会继续占用 C 盘 | 临时 WAV 改为写入已重定向的进程临时目录 |
| 外部 Ollama 服务不会继承 Meetily 的环境变量 | 模型可能仍下载到服务自己的 C 盘默认目录 | 便携模式默认禁止拉取；只有用户把外部服务模型目录改到 D 盘并显式启用后才允许下载 |
| 录音目录偏好可能保留旧的 C 盘路径或 D→C 连接点 | 新会议继续写入 C 盘 | 加载时回退到 D 盘默认目录；保存和创建目录前后都校验词法路径与已存在祖先的规范化位置 |
| 更新器安装包不保留仓库便携布局 | 更新后出现系统安装副本和第二份数据 | 权限层只开放版本检查，界面提示手动更新源码并重新构建 |
| 并行开发阶段各建一个缓存根目录 | 难以迁移和批量清理，目录越来越多 | 项目 `AGENTS.md` 固化“一个会话一个根目录，阶段只建子目录”规则 |

## 验证方式

自动化验证覆盖以下边界：

- 便携运行目录和便携 Cargo 目录能推导出同一个 `portable\app-data\`。
- C 盘数据目录和相对数据目录都会被拒绝。
- 普通安装路径不会被误认为便携目录。
- 录音偏好拒绝 C 盘路径。
- 便携模式下外部 Ollama 模型拉取默认关闭。
- 更新权限不包含下载和安装。
- FFmpeg 后备解码使用进程临时目录，不使用源文件目录。

便携运行时已经完成暂存和文件校验。删除 Cargo target 与 Conda 包缓存后，又从 `run-meetily.cmd` 成功启动最终 `portable\runtime\meetily.exe`；运行时没有监听旧开发端口 `3118`，说明成品不依赖 `localhost` 开发服务器。仍需人工验证：直接双击暂存程序、导入一个无敏感内容的短视频、切换录音目录，并确认新产生的 Meetily 文件都位于 D 盘。系统级写入不属于这项人工验收范围。

MOSS 生产 worker 还通过了真实 JSONL 进程和合成语音 GPU 冒烟验证：热启动语音样本的 RTF 为 0.314，进程 RSS 峰值约 1.851 GB，PyTorch CUDA reserved 峰值约 1.992 GB。静音输入会严格失败关闭，没有生成幻觉文本。这组结果用于证明本机链路可运行和估算资源，不代表真实会议准确率。

## 无法由项目完全控制的 C 盘内容

本方案约束的是 Meetily 和本次开发主动创建的文件，不会修改 Windows 或 Codex 宿主本身。下列内容仍可能由其所有者保留在 C 盘：

- Windows 系统文件、页面文件和系统日志；
- 已安装的 WebView2 Runtime、显卡驱动和证书/凭据存储；
- Visual Studio Build Tools 等已安装工具本体；
- Codex 桌面应用的账号、认证、全局配置、会话数据库和附件暂存。

这些内容不能通过修改 Meetily 仓库可靠迁移。当前承诺是：项目源码、运行数据、模型、环境、构建产物，以及本会话主动创建的测试临时文件均使用 D 盘；不会把系统宿主的既有状态冒充为已迁移。

## 当前限制

- 还没有扫描或删除 C 盘里可能存在的旧 Meetily 安装数据；自动迁移和自动删除都有误删风险，故意不做。
- 外部服务的目录必须在服务自身配置中修改，子进程环境变量不能改变已经运行的 Windows 服务。
- 启动和录音目录会解析最近存在的祖先，以拦截当时已经存在的 D→C 连接点；路径验证与实际写入之间仍存在很小的文件系统竞态，因此受支持路径应使用普通 D 盘目录，不应在应用运行时替换连接点。
- MOSS 当前用于完整音频窗口的异步多人校正，不能替代持续接收音频的流式主 ASR；悬浮字幕的低延迟原文仍由本地或在线流式 ASR 提供。
- 合成语音验证没有覆盖真实会议、噪声、口音和重叠说话，当前资源数据也只代表本次设备与样本。多人准确率和长期稳定性仍需按真实数据集评测。
- 活动录音 session 到持久化 meeting 的恢复绑定已经落库：录音开始前先写入受限目录哈希，保存会议时与会议、转写 revision 在同一事务绑定，重复保存会复用原 meeting ID。MOSS 尚未完成的是 session journal 导入 SQLite，以及重启后自动重新提交中断的模型 job；持久身份恢复完成不等于在途推理可以续跑。
- MOSS 查找环境、模型和受审查源码依赖项目内的 `portable` 相对布局。把 `MEETILY_DATA_DIR` 指向项目外部目录时，普通应用数据仍可使用该目录，但 MOSS 会失败关闭，不会静默回退到另一份模型。
