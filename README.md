# BaiTTS-CLI-Web

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0.html)
[![Rust](https://img.shields.io/badge/rust-1.89.0-orange.svg)](https://www.rust-lang.org/)

一个基于 [MultiTTS](https://t.me/MultiTTS) API 的命令行工具和 Web 服务，用于将文本文档（.txt, .epub）转换为带元数据的有声书音频（.mp3）。

## ✨ 功能特性

### 核心转换
- **多格式支持**: 将单文件或整个目录的 .txt / .epub 按章节转换为有声书音频。
- **ID3 元数据**: 自动写入书籍信息（封面、书名、章节）和声音信息到音频文件中。
- **内嵌 LRC 歌词**: 生成同步歌词文件，支持智能断句（在标点处分割，避免行首标点）。
- **角色分离**: 正则自动区分旁白和对话，对话部分可独立设置发音人、语速、音量、音调。

### AI 对话分配 (New)
- **AI 说话人识别**: 对接 OpenAI 兼容 API（支持 DeepSeek 等推理模型），自动识别对白说话人
- **10 类声音池**: 男童/女童/少男/少女/男青年/女青年/男中年/女中年/男老年/女老年，每个分类可分配多个声音
- **AI 智能建议**: 一键调用 AI 分析声音列表，自动将声音分配到各分类
- **角色声音分配表**: 每本小说生成独立的角色→声音分配表，支持手动编辑、试听、锁定
- **轮询分配**: 同分类多角色自动轮询不同声音（唐三→声音1, 林动→声音2）
- **自动回退**: AI 失败或角色未匹配时，按性别+年龄推断分类自动分配，最终回退默认对话声音

### Web 用户界面
- **独立设置页面**: 设置与主页分离，互不干扰
- **深色模式**: CSS 变量驱动的语义化深色/浅色切换
- **声音试听**: 所有声音下拉框旁一键试听
- **参数记忆**: API 地址、AI 配置、UI 偏好设置全部服务端持久化
- **一键重置**: 恢复默认参数设置

### 批量转换 & 任务管理
- **Docker 支持**: Dockerfile 容器化部署，支持卷挂载批量转换
- **自动检测模式**: 监听 /book 目录，自动处理新增文件
- **实时进度**: SSE 推送日志和任务进度（章节/总数、ETA）
- **任务管理**: 取消、重试、全部重试、清空历史、状态持久化
- **正则过滤**: 自定义忽略内容（默认跳过分隔符），WebUI 正则测试工具
- **黑名单词汇**: 支持本地文件或 URL，多词管道符分割，正则匹配

## ⚙️ 安装
安装方法二选一即可，推荐直接使用预构建的二进制文件，如果预构建的二进制文件不能满足你的运行平台，则建议自行编译安装。

### 1. 使用预构建二进制文件
根据你的系统下载最新的预构建二进制文件 [https://github.com/Doraemonsan/BaiTTS-CLI-rs/releases](https://github.com/Doraemonsan/BaiTTS-CLI-rs/releases) ，解压并赋予可执行权限直接执行，或者拷贝到你的系统路径 (如 `/usr/local/bin` ）下以全局使用

预构建的二进制文件现已支持:
  + Linux (glibc-x64, glibc-Arm64)
  + Windows (x64)
  + MacOS 15+ (Arm64)


首先，你需要安装 Rust 开发环境。推荐使用 `rustup` 进行安装。本项目在 `rustc 1.89.0` 版本下进行开发和测试，建议使用的 `rustc` 版本不低于本项目开发环境

```Shell
# 安装 rustup (如果尚未安装)
pacman -Sy rustup
# 安装最新的稳定版 rust 开发环境
rustup install stable
# 设置稳定版为默认环境
rustup default stable
```

使用源码进行构建

```Shell
# 1. 克隆本仓库
git clone https://github.com/Doraemonsan/BaiTTS-CLI-rs

# 2. 进入项目目录
cd BaiTTS-CLI-rs

# 3. 使用 Cargo 进行编译，如需交叉编译请自行安装对应平台工具链
# 使用 --target 来生成目标平台的二进制文件(如 --target x86_64-pc-windows-gnu)
cargo build --release

# 编译后的可执行文件位于 ./target/release/baitts-cli-rs
# 你可以将其复制到你的系统路径下（如 /usr/local/bin）以便全局使用
# sudo cp ./target/release/baitts-cli-rs /usr/local/bin
```

## 🐳 Docker 使用

你可以使用 Docker 来容器化运行本应用，特别推荐使用此方式来进行批量转换。

### 1. 构建镜像

在项目根目录下执行以下命令：
```Shell
docker build -t baitts-cli-rs .
```

### 2. 运行 WebUI

构建完成后，使用以下命令启动 WebUI 服务：
```Shell
docker run -d -p 5688:5688 --name baitts baitts-cli-rs
```
服务将在 `http://localhost:5688` 上可用。

### 3. 预设 API 地址 (推荐)

通过环境变量 `API_URL` 来预设 MultiTTS 的地址，这样 WebUI 启动后会自动加载声音列表。
```Shell
docker run -d -p 5688:5688 -e "API_URL=http://192.168.1.10:8774" --name baitts baitts-cli-rs
```

### 4. 使用批量转换功能

使用 Docker 的卷挂载功能来进行批量转换是最高效的方式。
- 将包含源文件（.txt, .epub）的目录挂载到容器的 `/book` 目录。
- 将用于存放输出音频的目录挂载到容器的 `/output` 目录。

```Shell
docker run -d -p 5688:5688 \
  -e "API_URL=http://192.168.1.10:8774" \
  -e "DEFAULT_VOLUME=50" \
  -e "DEFAULT_SPEED=50" \
  -e "DEFAULT_PITCH=50" \
  -v /path/to/your/books:/book \
  -v /path/to/your/output:/output \
  -v /path/to/your/data:/data \
  --name baitts \
  baitts-cli-rs
```
启动后，访问 WebUI，在“批量转换”功能区点击按钮即可开始任务。

## 💻 使用方法

### 1. WebUI 使用

使用 `--web` 参数启动 Web 服务：
```Shell
baitts-cli-rs --web
```
应用将在默认端口 `5688` 启动，并自动尝试在浏览器中打开 `http://localhost:5688`。WebUI 提供了所有核心功能的图形化界面，包括批量转换。

### 2. 命令行使用

**重要提示**: 所有命令行操作都需要通过 `--api` 参数指定 `MultiTTS` 服务的 URL。

#### 查看可用的声音列表

```Shell
baitts-cli-rs --api http://127.0.0.1:8774 --list
```

#### 转换单个文本文件

```Shell
baitts-cli-rs --api http://127.0.0.1:8774 --file /path/to/your/book.txt
```

#### 批量转换目录下的所有文本文件

程序会自动查找并处理指定目录下的所有支持格式文件。
```Shell
baitts-cli-rs --api http://127.0.0.1:8774 --dir /path/to/your/books/
```

#### 使用高级选项 (生成LRC、指定声音等)

```Shell
baitts-cli-rs \
  --api http://127.0.0.1:8774 \
  --file story.txt \
  --out ./audiobooks \
  --voice "zh-CN-XiaoxiaoNeural" \
  --speed 85 \
  --sub 25 \
  --blacklist ./my_blacklist.txt
```
此命令会将 `story.txt` 转换为音频，保存在 `./audiobooks` 目录，并使用指定的声音、语速和LRC设置。

## 📚 命令行参数

| 参数                | 缩写         | 描述                                                         | 默认值   |
| ------------------- | ------------ | ------------------------------------------------------------ | -------- |
| `--web`             |              | 启动 WebUI 服务界面。                                        | -        |
| `--list`            | `-l`         | 列出当前 API 所有可用的声音。                                | -        |
| `--file <PATH>`     | `-f <PATH>`  | 指定要处理的单个文件。                                       | -        |
| `--dir <PATH>`      | `-d <PATH>`  | 指定要处理的包含多个文件的目录。                             | -        |
| `--api <URL>`       |              | **[必需]** MultiTTS API 的基础 URL。                         | -        |
| `--out <DIR>`       | `-o <DIR>`   | 指定输出目录。                                               | `output` |
| `--concurrency <NUM>`|             | 指定并发任务数 (用于目录处理)。                              | `4`      |
| `--voice <ID>`      |              | 指定旁白使用的声音 ID。                                      | API 默认 |
| `--voice-dialogue <ID>`|           | 指定对话部分使用的声音 ID。                                  | 同 voice |
| `--volume <0-100>`  |              | 指定旁白音量。                                               | `50`     |
| `--speed <0-100>`   |              | 指定旁白语速。                                               | `50`     |
| `--pitch <0-100>`   |              | 指定旁白音高。                                               | `50`     |
| `--volume-dialogue <0-100>`|       | 指定对话音量。                                               | 同 volume |
| `--speed-dialogue <0-100>` |        | 指定对话语速。                                               | 同 speed |
| `--pitch-dialogue <0-100>` |        | 指定对话音高。                                               | 同 pitch |
| `--sub [CHARS]`     | `-s [CHARS]` | 生成 LRC 歌词，可选每行最大字符数 (10-100，0=禁用)。        | `15`     |
| `--ignore-regex <RE>`|             | 指定忽略内容的正则表达式。                                   | `\*{3,}|#{2,}` |
| `--blacklist <SRC>` | `-b <SRC>`   | 黑名单词库来源 (本地路径或 URL)。                             | -        |
| `--preserve-structure`|            | 保持输出目录结构 (处理目录时有效)。                          | `false`  |
| `--help`            | `-h`         | 显示帮助信息。                                               | -        |
| `--version`         | `-V`         | 显示版本信息。                                               | -        |

## 🎛️ WebUI 功能详解

### AI 对话分配设置
在设置页面启用 AI 对话分配后：
1. 填写大模型 API 地址、密钥、模型名（支持 OpenAI 兼容接口和 DeepSeek 等推理模型）
2. 点击「AI 建议」自动将声音列表分配到 10 个分类
3. 在角色表中添加角色并指定分类（可选覆盖声音）
4. 预览识别可测试 AI 说话人识别效果

### 角色声音分配表
在主页选择小说文件后：
1. 点击「重新生成」→ 系统根据声音池和角色表预分配每个角色的声音
2. 每条支持手动编辑声音（下拉带试听 ▶ 按钮）
3. 锁定（🔒）的条目在重新生成时不会被覆盖
4. 保存后，批量转换时会自动按分配表使用对应声音

### 声音池（10 个分类）
| 男声 | 女声 |
|------|------|
| 男童 | 女童 |
| 少男 | 少女 |
| 男青年 | 女青年 |
| 男中年 | 女中年 |
| 男老年 | 女老年 |

每个分类可分配 1-N 个声音，同分类多角色自动轮询分配。

## 📄 许可证

本项目采用 [GPLv3](https://www.gnu.org/licenses/gpl-3.0.html) 许可证。

## 问题反馈
如果您遇到任何问题，请通过 GitHub Issues 页面提交问题报告。
