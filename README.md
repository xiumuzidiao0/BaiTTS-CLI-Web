# BaiTTS-CLI-Web

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0.html)
[![Rust](https://img.shields.io/badge/rust-1.89.0-orange.svg)](https://www.rust-lang.org/)

一个基于 [MultiTTS](https://t.me/MultiTTS) API 的命令行工具和 Web 服务，用于将文本文档（.txt, .epub）转换为带元数据的有声书音频（.mp3）。

## ✨ 功能特性

- **MP3有声书**: 将单个文件或整个目录的文件（支持 .txt, .epub）按章节转换为独立的 .mp3 音频文件。
- **ID3 元数据**: 自动将书籍信息（封面、书名、章节）、声音（艺术家）和LRC歌词写入 MP3 文件中。
- **Web 用户界面**: 提供简单易用的 WebUI，支持所有核心功能，并增加了批量转换模块。
- **Docker 支持**: 提供 Dockerfile，支持容器化部署和批量转换。
- **内嵌LRC歌词**: 在生成音频的同时，创建同步歌词并内嵌到 MP3 文件中。
- **角色分离**: 支持通过正则自动区分旁白和对话，并为对话部分单独设置发音人、语速、音量和音调。
- **交互体验**: WebUI 新增深色模式、参数记忆、一键重置及文本试听下载功能。
- **参数可调**: 支持自定义声音、音量、语速和音高。
- **编码处理**: 自动检测非 UTF-8 编码的文件，并提示用户进行转换。
- **任务管理**: 实时追踪批量转换进度，支持任务取消、重试、状态持久化及详细配置查看。
- **正则过滤**: 支持自定义正则表达式以忽略文本中的特定内容（如分隔符），并提供 WebUI 测试工具。
- **黑名单词汇**: 支持通过本地文件或 URL 加载黑名单词库，以跳过特定词语的发音。

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
| `--voice <ID>`      |              | 指定要使用的声音 ID。                                        | API 默认 |
| `--volume <0-100>`  |              | 指定音量。                                                   | API 默认 |
| `--speed <0-100>`   |              | 指定语速。                                                   | API 默认 |
| `--pitch <0-100>`   |              | 指定音高。                                                   | API 默认 |
| `--sub [CHARS]`     | `-s [CHARS]` | 生成 LRC 歌词，并可选设置每行最大字符数 (10-100)。           | `15`     |
| `--ignore-regex <RE>`|             | 指定要忽略的内容的正则表达式。                               | `\*{3,}|#{2,}` |
| `--blacklist <SRC>` | `-b <SRC>`   | 指定黑名单词库的来源 (本地路径或 URL)。多个字词使用管道符分割，支持正则，当输入为文件时，每行视为一个参数 | -        |
| `--help`            | `-h`         | 显示帮助信息。                                               | -        |
| `--version`         | `-V`         | 显示版本信息。                                               | -        |

## 📄 许可证

本项目采用 [GPLv3](https://www.gnu.org/licenses/gpl-3.0.html) 许可证。

## 问题反馈
如果您遇到任何问题，请通过 GitHub Issues 页面提交问题报告。
