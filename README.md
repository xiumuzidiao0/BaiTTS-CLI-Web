# BaiTTS-CLI-Web

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0.html)
[![Rust](https://img.shields.io/badge/rust-1.89.0-orange.svg)](https://www.rust-lang.org/)

BaiTTS-CLI-Web 是一个基于 MultiTTS API 的小说转有声书工具，支持 `txt` / `epub`，可以通过命令行或 WebUI 将文本按章节转换为音频。当前版本重点强化了 WebUI 工作流：AI 说话人分析、角色声音分配表、分析/合成拆分、任务阶段管理、AI API 使用统计与费用估算。

输出默认按书名建立目录，每章生成一个 `.wav` 文件；启用字幕时会额外生成同名 `.lrc` 文件。

## 主要能力

### 文本转音频

- 支持单个 `txt` / `epub` 文件转换。
- 支持目录批量转换，并可保持原目录结构。
- 按章节输出 `.wav`，自动跳过已存在且非空的章节音频。
- 支持旁白声音与对话声音分别设置。
- 支持音量、语速、音调独立调整。
- 支持 LRC 字幕生成，按标点进行更自然的断句。
- 支持忽略正则和黑名单词库过滤。

### AI 对话识别与角色声音分配

- 支持 OpenAI 兼容的 Chat Completions API。
- 支持章节级 AI 对话分析，优先整章识别，减少逐句请求。
- 章节识别失败或缺失时会分块/逐句兜底。
- 支持 10 类声音池：男童、女童、少男、少女、男青年、女青年、男中年、女中年、男老年、女老年。
- 支持 AI 根据声音列表自动建议声音池分类。
- 每本书保存独立角色声音分配表。
- 分配表支持试听、修改声音、别名、音量、语速、音调、锁定和删除。
- 手动编辑的角色会默认锁定，避免后续 AI 自动覆盖。
- 会拦截常见 AI 乱码角色名，避免乱码进入分配表。

### 分析与合成拆分

- 可开启“转换时仅分析”，只执行 AI 说话人分析和分配表写入，不生成音频。
- 任务列表支持“开始分析”“暂停分析”“开始合成”“暂停合成”。
- 从分析任务开始合成时，会复用同一个任务并显示分析/合成两个阶段进度。
- 如果合成追上当前已分析章节，会暂停并提示是否继续 AI 分析并同步合成后续章节。
- 已保存的分析结果位于 `data/dialogue_analysis`，分配表位于 `data/allocations`。

### 任务管理与文件管理

- WebUI 任务列表实时显示状态、章节进度、ETA 和输出大小。
- 支持取消、暂停、恢复、重试、批量删除任务。
- 支持删除任务时选择是否同时删除分配表、输出文件、源文件。
- 支持自动检测 `book` 目录新增文件并加入转换队列。
- 文件管理器支持上传、删除、下载、播放音频，以及从文件列表直接开始转换。

### AI 使用统计

- 记录总请求、章节请求、逐句请求、失败请求、429/重试次数。
- 按小说统计请求量、已分析章节、平均每章请求、429 次数。
- 如果模型接口返回 usage，会记录真实 prompt/completion/total tokens。
- 如果接口不返回 usage，会按文本长度估算 tokens，并在看板中标注估算值。
- 可配置输入/输出每百万 tokens 单价，显示总费用估算。
- 可删除指定小说的 AI 统计数据。
- 支持设置 429 冷却秒数，遇到 RPM/限流错误后全局暂停后续 AI 请求，避免请求雪崩。

## 运行前准备

你需要先准备一个可用的 MultiTTS API 服务地址，例如：

```text
http://127.0.0.1:8774
```

如果要使用 AI 说话人识别，还需要一个 OpenAI 兼容的接口：

```text
https://api.example.com/v1/chat/completions
```

AI API Key 支持多个 Key 用 `@@` 分隔，程序会取第一个非空 Key。

## 安装与构建

### 使用预构建二进制

从 Releases 下载对应平台的二进制文件：

[https://github.com/Doraemonsan/BaiTTS-CLI-rs/releases](https://github.com/Doraemonsan/BaiTTS-CLI-rs/releases)

下载后解压运行，或放入系统 PATH。

### 从源码构建

建议使用稳定版 Rust。本项目当前使用 Rust 2024 edition。

```shell
git clone https://github.com/Doraemonsan/BaiTTS-CLI-rs
cd BaiTTS-CLI-rs
cargo build --release
```

构建产物位于：

```text
target/release/baitts-cli-rs
```

## WebUI 使用

启动 WebUI：

```shell
baitts-cli-rs --web
```

默认访问地址：

```text
http://localhost:5688
```

WebUI 首次使用建议流程：

1. 打开设置页，填写 MultiTTS API 地址并获取声音列表。
2. 选择旁白声音、对话默认声音和基础参数。
3. 如需 AI 识别，启用“AI 说话人识别”，填写 AI API 地址、Key、模型名。
4. 配置声音池，或点击 AI 建议自动分配 10 类声音池。
5. 在文件管理中上传/选择 `txt` 或 `epub`。
6. 根据需要选择直接转换，或开启“转换时仅分析”先分析后合成。

### 分析优先工作流

适合长篇小说或想先检查角色分配的场景：

1. 设置页开启“转换时仅分析”。
2. 在文件管理或主页开始转换。
3. 任务完成或暂停后，在任务列表查看分析进度。
4. 打开角色声音分配表，确认或修改角色声音。
5. 点击任务列表中的“开始合成”。
6. 合成阶段会使用已保存的分析结果和分配表。

如果只分析了一部分章节就开始合成，合成会在分析边界暂停，并提示是否继续 AI 分析并同步合成。

### 角色声音分配表

分配表是每本小说的角色到声音映射，保存在 `data/allocations`。

可编辑字段包括：

- 角色别名
- 声音
- 音量
- 语速
- 音调
- 是否锁定
- 是否待确认

说明：

- 手动修改声音、别名或参数后，该条目会自动标记为手动编辑并锁定。
- 锁定条目不会被 AI 重新分配覆盖。
- 保存前 WebUI 会同步当前表格控件内容，避免未触发 change 事件导致保存旧值。
- 保存后后端会重新读取落盘文件并返回实际保存结果。

### AI 参数建议

说话人识别不是创作任务，建议使用较低随机性：

| 参数 | 推荐值 | 说明 |
| --- | --- | --- |
| Temperature | `0.05` | 保持稳定输出 |
| Top P | `0.3` | 降低发散 |
| 最大 Tokens | `120` 逐句 / 章节自动放大 | 逐句识别可小，章节识别会按对白数量自动提高 |
| 上下文长度 | `1800` 左右 | 对多数对话归属足够，复杂章节可增大 |
| 429 冷却秒数 | `60` | RPM 紧张时可增大 |

## Docker 使用

### 构建镜像

```shell
docker build -t baitts-cli-web .
```

### 启动 WebUI

```shell
docker run -d \
  -p 5688:5688 \
  --name baitts \
  baitts-cli-web
```

### 推荐挂载方式

```shell
docker run -d \
  -p 5688:5688 \
  -e "API_URL=http://192.168.1.10:8774" \
  -e "DEFAULT_VOLUME=50" \
  -e "DEFAULT_SPEED=50" \
  -e "DEFAULT_PITCH=50" \
  -v /path/to/books:/book \
  -v /path/to/output:/output \
  -v /path/to/data:/data \
  --name baitts \
  baitts-cli-web
```

挂载目录说明：

| 容器目录 | 用途 |
| --- | --- |
| `/book` | 待处理的 `txt` / `epub` |
| `/output` | 输出音频和 LRC |
| `/data` | 配置、任务记录、分配表、分析结果、AI 统计 |

### 环境变量

| 变量 | 说明 | 默认值 |
| --- | --- | --- |
| `API_URL` | MultiTTS API 地址 | 空 |
| `DEFAULT_VOLUME` | 默认音量 | `50` |
| `DEFAULT_SPEED` | 默认语速 | `50` |
| `DEFAULT_PITCH` | 默认音调 | `50` |
| `AUTORUN` | 是否启动时开启自动检测 | `false` |

## 命令行使用

CLI 模式适合简单转换。AI 对话识别、分配表编辑、分析/合成拆分主要通过 WebUI 使用。

### 查看声音列表

```shell
baitts-cli-rs --api http://127.0.0.1:8774 --list
```

### 转换单个文件

```shell
baitts-cli-rs \
  --api http://127.0.0.1:8774 \
  --file ./book/example.epub \
  --out ./output \
  --voice bytedance_BV123_24k \
  --voice-dialogue bytedance_BV120_24k \
  --sub 25
```

### 批量转换目录

```shell
baitts-cli-rs \
  --api http://127.0.0.1:8774 \
  --dir ./book \
  --out ./output \
  --concurrency 4 \
  --preserve-structure
```

### 常用参数

| 参数 | 缩写 | 说明 | 默认值 |
| --- | --- | --- | --- |
| `--web` |  | 启动 WebUI |  |
| `--list` | `-l` | 列出 MultiTTS 声音 |  |
| `--file <PATH>` | `-f` | 转换单个 `txt` / `epub` |  |
| `--dir <PATH>` | `-d` | 转换目录内文件 |  |
| `--api <URL>` |  | MultiTTS API 地址，CLI 模式必填 |  |
| `--out <DIR>` | `-o` | 输出目录 | `output` |
| `--voice <ID>` |  | 旁白声音 | API 默认 |
| `--voice-dialogue <ID>` |  | 对话默认声音 | 同旁白 |
| `--volume <0-100>` |  | 旁白音量 | `50` |
| `--speed <0-100>` |  | 旁白语速 | `50` |
| `--pitch <0-100>` |  | 旁白音调 | `50` |
| `--volume-dialogue <0-100>` |  | 对话音量 | 同旁白 |
| `--speed-dialogue <0-100>` |  | 对话语速 | 同旁白 |
| `--pitch-dialogue <0-100>` |  | 对话音调 | 同旁白 |
| `--sub <CHARS>` | `-s` | LRC 每行字符数，`0` 为禁用 | `15` |
| `--ignore-regex <REGEX>` |  | 忽略内容正则 | `\*{3,}|#{2,}` |
| `--blacklist <PATH_OR_URL>` | `-b` | 黑名单词库 |  |
| `--concurrency <NUM>` |  | 目录处理并发数 | `4` |
| `--preserve-structure` |  | 批量转换时保持目录结构 | `false` |

## 数据目录

默认使用项目目录下的 `data`；Docker 环境中如果存在 `/data`，则优先使用 `/data`。

| 路径 | 说明 |
| --- | --- |
| `data/config.json` | WebUI 基础设置 |
| `data/ai_dialogue_config.json` | AI 对话识别与声音池配置 |
| `data/baitts_tasks.json` | 任务记录 |
| `data/allocations/*.json` | 角色声音分配表 |
| `data/dialogue_analysis/*.json` | AI 对话分析结果 |
| `data/ai_usage_stats.json` | AI 请求、tokens、费用估算统计 |

## 常见问题

### 为什么 AI 请求很多，甚至出现 429？

长篇小说对白多，如果逐句识别会产生大量请求。建议开启章节级 AI 对话分析，并适当降低并发或提高 429 冷却秒数。

### 为什么合成到某一章后暂停？

如果你只分析了前几章就开始合成，合成到分析边界会暂停。这是为了避免跳过已暂停的分析状态并重新同步分析合成。任务列表会提示是否继续 AI 分析并同步合成后续章节。

### 为什么分配表里有“待确认”？

AI 识别置信度较低或缺少足够上下文时，会标记为待确认。你可以在分配表中修改、试听并确认。

### 修改分配表后为什么建议锁定？

锁定用于防止后续 AI 自动覆盖人工选择。当前版本手动编辑声音、别名或参数后会默认锁定该角色。

### AI 统计里的 tokens 和费用一定准确吗？

如果模型接口返回 usage，tokens 使用接口返回值；如果不返回 usage，程序会按文本长度估算，并在 UI 中标注估算。费用也基于 tokens 和你设置的单价估算，仅供参考。

## 开发检查

常用检查命令：

```shell
cargo fmt
cargo check
cargo test
```

WebUI 是内嵌静态文件，修改 `src/static/index.html` 后需要重启 WebUI 才能看到变化。

## 许可证

本项目采用 [GPLv3](https://www.gnu.org/licenses/gpl-3.0.html) 许可证。

## 问题反馈

如果遇到问题，请通过 GitHub Issues 提交问题报告，并尽量附上：

- 使用方式：WebUI / CLI / Docker
- 输入文件类型：txt / epub
- 相关日志
- AI 配置是否启用
- 任务状态截图或错误提示
