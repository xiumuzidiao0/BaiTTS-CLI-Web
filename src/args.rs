// src/args.rs

use clap::Parser;
use std::path::PathBuf;
use serde::{Serialize, Deserialize};

/// BaiTTS-CLI-rs: 基于 MultiTTS API 的文本转有声书命令行工具 (支持 txt, epub)
#[derive(Parser, Debug, Clone, Serialize, Deserialize)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// [操作模式] 列出当前 API 所有可用的声音
    #[arg(short, long, group = "mode")]
    pub list: bool,

    /// [操作模式] 指定要处理的单个文件 (支持 .txt, .epub)
    #[arg(short, long, value_name = "FILE_PATH", group = "mode")]
    pub file: Option<PathBuf>,

    /// [操作模式] 指定要处理的包含多个支持格式文件的目录
    #[arg(short, long, value_name = "DIR_PATH", group = "mode")]
    pub dir: Option<PathBuf>,

    /// [操作模式] 启动 WebUI 界面
    #[arg(long, group = "mode")]
    pub web: bool,

    /// [必需] MultiTTS API 的基础 URL [示例： http://127.0.0.1:8774]
    #[arg(long)]
    pub api: Option<String>,

    /// [可选] 指定输出目录
    #[arg(short, long, value_name = "OUTPUT_DIR", default_value = "output")]
    pub out: PathBuf,

    /// [可选] 指定要使用的声音 ID
    #[arg(long)]
    pub voice: Option<String>,

    /// [可选] 指定对话部分使用的声音 ID (若不指定则与 voice 相同)
    #[arg(long)]
    pub voice_dialogue: Option<String>,

    /// [可选] 指定对话部分的音量 (0-100)，若不指定则使用默认音量
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub volume_dialogue: Option<u8>,

    /// [可选] 指定对话部分的语速 (0-100)，若不指定则使用默认语速
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub speed_dialogue: Option<u8>,

    /// [可选] 指定对话部分的音调 (0-100)，若不指定则使用默认音调
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub pitch_dialogue: Option<u8>,

    /// [可选] 指定音量 (0-100)
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub volume: u8,

    /// [可选] 指定语速 (0-100)
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub speed: u8,

    /// [可选] 指定音高 (0-100)
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub pitch: u8,

    /// [可选] 生成LRC歌词时每行的最大字符数 [默认: 15, 范围: 10-100, 0=禁用]
    #[arg(short, long, value_name = "CHARS_PER_LINE", default_value_t = 15, value_parser = parse_sub_range)]
    pub sub: usize,

    /// [可选] 指定忽略内容的正则表达式 [默认: \*{3,}|#{2,}]
    #[arg(long, default_value = r"\*{3,}|#{2,}")]
    pub ignore_regex: String,

    /// [可选] 指定黑名单词库的来源 (本地路径或 URL)
    #[arg(short = 'b', long, value_name = "SOURCE")]
    pub blacklist: Option<String>,

    /// [可选] 指定并发任务数 (默认为 4)
    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,

    /// [可选] 保持目录结构 (仅在处理目录时有效)
    #[arg(long, default_value_t = false)]
    #[serde(default)]
    pub preserve_structure: bool,
}

// 验证函数
fn parse_sub_range(s: &str) -> Result<usize, String> {
    let value: usize = s
        .parse()
        .map_err(|_| format!("'{}' 不是一个有效的数字", s))?;
    if value == 0 || (10..=100).contains(&value) {
        Ok(value)
    } else {
        Err("LRC 歌词字符数必须在 10 到 100 之间, 或使用 0 禁用".to_string())
    }
}
