use crate::api::ApiClient;
use crate::args::Cli;
use crate::extractor::{self, Book, Chapter};
use crate::lrc;
use crate::utils;
use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use hound::WavWriter;
use id3::frame::{Picture, PictureType};
use id3::TagLike;
use regex::Regex;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub enum ProcessEvent {
    Log(String),
    Progress { current: usize, total: usize },
    Success { size: u64, output_path: String },
}

async fn process_chapter<F, C, Fut>(
    idx: usize,
    total_chapters: usize,
    book: &Book,
    chapter: Chapter,
    book_dir: &Path,
    args: &Cli,
    client: &ApiClient,
    blacklist: &Option<Regex>,
    callback: F,
    check_cancel: C,
) -> Result<()>
where
    F: Fn(ProcessEvent) + Send + Sync + 'static + Clone,
    C: Fn() -> Fut + Send + Sync + 'static + Clone,
    Fut: std::future::Future<Output = bool> + Send,
{
    let clean_title = utils::sanitize_filename(&chapter.title);
    let chapter_wav_filename = format!("{}_{}.wav", idx + 1, clean_title);
    let output_path = book_dir.join(&chapter_wav_filename);

    if output_path.exists() && output_path.metadata()?.len() > 0 {
        callback(ProcessEvent::Log(format!("跳过已存在的章节: {}", chapter_wav_filename)));
        callback(ProcessEvent::Progress { current: idx + 1, total: total_chapters });
        return Ok(());
    }

    callback(ProcessEvent::Log(format!("正在处理章节 [{}/{}]: {}", idx + 1, total_chapters, chapter.title)));

    let lines: Vec<&str> = chapter.content.lines().collect();
    if lines.is_empty() {
        callback(ProcessEvent::Log(format!("章节内容为空，跳过: {}", chapter.title)));
        callback(ProcessEvent::Progress { current: idx + 1, total: total_chapters });
        return Ok(());
    }

    let mut all_wav_samples: Vec<i16> = Vec::new();
    let mut wav_spec: Option<hound::WavSpec> = None;
    let mut lyrics_data: Vec<(u32, String)> = Vec::new();
    let mut current_timestamp = Duration::from_secs(0);

    struct BatchData {
        text: String,
        lines: Vec<String>,
        is_dialogue: bool,
    }

    let mut batches: Vec<BatchData> = Vec::new();
    const MAX_BATCH_CHARS: usize = 300;
    
    let dialogue_regex = Regex::new(r"“[^”]*”|「[^」]*」").unwrap();
    let ignore_regex = Regex::new(&args.ignore_regex).unwrap_or_else(|_| Regex::new(r"\*{3,}|#{2,}").unwrap());
    
    let mut current_batch = BatchData {
        text: String::new(),
        lines: Vec::new(),
        is_dialogue: false,
    };
    let mut is_batch_empty = true;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        let processed_line = if let Some(bl_regex) = blacklist {
            utils::apply_blacklist(trimmed, bl_regex)
        } else {
            trimmed.to_string()
        };

        let processed_line = ignore_regex.replace_all(&processed_line, "").to_string();
        if processed_line.trim().is_empty() { continue; }

        // Split line into segments (Narrator / Dialogue)
        let mut last_end = 0;
        let mut segments = Vec::new();

        for mat in dialogue_regex.find_iter(&processed_line) {
            if mat.start() > last_end {
                segments.push((&processed_line[last_end..mat.start()], false)); // Narrator
            }
            segments.push((mat.as_str(), true)); // Dialogue
            last_end = mat.end();
        }
        if last_end < processed_line.len() {
            segments.push((&processed_line[last_end..], false)); // Narrator
        }

        for (seg_text, is_dialogue) in segments {
            if seg_text.trim().is_empty() { continue; }

            // If current batch is not empty and type differs, flush it
            if !is_batch_empty && current_batch.is_dialogue != is_dialogue {
                batches.push(current_batch);
                current_batch = BatchData {
                    text: String::new(),
                    lines: Vec::new(),
                    is_dialogue,
                };
            } else if is_batch_empty {
                current_batch.is_dialogue = is_dialogue;
            }

            // Check size limit
            if !current_batch.text.is_empty() && (current_batch.text.len() + seg_text.len() > MAX_BATCH_CHARS) {
                batches.push(current_batch);
                current_batch = BatchData {
                    text: String::new(),
                    lines: Vec::new(),
                    is_dialogue,
                };
            }

            if !current_batch.text.is_empty() {
                current_batch.text.push('\n');
            }
            current_batch.text.push_str(seg_text);
            current_batch.lines.push(seg_text.to_string());
            is_batch_empty = false;
        }
    }
    if !is_batch_empty {
        batches.push(current_batch);
    }

    for (i, batch) in batches.iter().enumerate() {
        if check_cancel().await {
            return Err(anyhow::anyhow!("任务已取消"));
        }
        if i % 5 == 0 && i > 0 {
            callback(ProcessEvent::Log(format!("正在处理章节 '{}': 批次 {}/{}", chapter.title, i + 1, batches.len())));
        }
        let (target_voice, volume, speed, pitch) = if batch.is_dialogue {
            (
                args.voice_dialogue.as_ref().or(args.voice.as_ref()).cloned(),
                Some(args.volume_dialogue.unwrap_or(args.volume)),
                Some(args.speed_dialogue.unwrap_or(args.speed)),
                Some(args.pitch_dialogue.unwrap_or(args.pitch)),
            )
        } else {
            (args.voice.as_ref().cloned(), Some(args.volume), Some(args.speed), Some(args.pitch))
        };

        let wav_data = match client.generate_speech(&batch.text, &target_voice, &volume, &speed, &pitch).await {
            Ok(data) => data,
            Err(e) => {
                callback(ProcessEvent::Log(format!("警告: 章节 '{}' 批次 {} 转换失败，跳过。原因: {}", chapter.title, i + 1, e)));
                continue;
            }
        };
        if wav_data.is_empty() { continue; }

        let mut reader = hound::WavReader::new(Cursor::new(&wav_data))?;
        let spec = reader.spec();
        if wav_spec.is_none() { wav_spec = Some(spec); }

        let samples: Vec<i16> = reader.samples().collect::<Result<_, _>>()?;
        let total_duration_ms = (samples.len() as f64 / spec.channels as f64 / spec.sample_rate as f64) * 1000.0;
        let total_chars: usize = batch.lines.iter().map(|l| l.chars().count()).sum();

        if total_chars > 0 {
            let chars_per_ms = total_chars as f64 / total_duration_ms;
            for origin_line in &batch.lines {
                let line_chars = origin_line.chars().count();
                let line_duration = Duration::from_millis((line_chars as f64 / chars_per_ms) as u64);
                
                if args.sub > 0 {
                    let chunks = lrc::split_line_intelligently(origin_line, args.sub);
                    let chunk_duration = line_duration / chunks.len().max(1) as u32;
                    for chunk in chunks {
                        lyrics_data.push((current_timestamp.as_millis() as u32, chunk));
                        current_timestamp += chunk_duration;
                    }
                } else {
                    lyrics_data.push((current_timestamp.as_millis() as u32, origin_line.to_string()));
                    current_timestamp += line_duration;
                }
            }
        }
        all_wav_samples.extend(samples);
    }

    if all_wav_samples.is_empty() || wav_spec.is_none() {
        callback(ProcessEvent::Log(format!("章节转换后无音频数据: {}", chapter.title)));
        return Ok(());
    }

    let spec = wav_spec.unwrap();
    
    // --- 写入 WAV 文件 ---
    let mut writer = WavWriter::create(&output_path, spec)?;
    for sample in all_wav_samples {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;

    // --- ID3 Tag 写入 ---
    let mut tag = id3::Tag::new();
    tag.set_title(chapter.title.clone());
    tag.set_album(book.title.clone());
    if let Some(voice_name) = &args.voice {
        tag.set_artist(voice_name.clone());
    }
    if idx == 0 {
        if let Some((cover_data, mime_type)) = &book.cover {
            tag.add_frame(Picture {
                mime_type: mime_type.clone(),
                picture_type: PictureType::CoverFront,
                description: "Cover".to_string(),
                data: cover_data.clone(),
            });
        }
    }
    tag.write_to_path(&output_path, id3::Version::Id3v23)?;

    // --- 写入 lrc 文件 ---
    if args.sub > 0 && !lyrics_data.is_empty() {
        let mut lrc_content = String::new();
        for (timestamp_ms, text) in lyrics_data {
            let minutes = timestamp_ms / 60000;
            let seconds = (timestamp_ms % 60000) / 1000;
            let centiseconds = (timestamp_ms % 1000) / 10;
            lrc_content.push_str(&format!(
                "[{:02}:{:02}.{:02}]{}\n",
                minutes, seconds, centiseconds, text
            ));
        }
        let lrc_path = output_path.with_extension("lrc");
        fs::write(lrc_path, lrc_content)?;
    }

    callback(ProcessEvent::Log(format!("章节完成: {}", chapter_wav_filename)));
    callback(ProcessEvent::Progress { current: idx + 1, total: total_chapters });
    Ok(())
}

pub async fn process_file<F, C, Fut>(
    file_path: &Path,
    args: &Cli,
    client: &ApiClient,
    blacklist: &Option<Regex>,
    callback: F,
    check_cancel: C,
) -> Result<()>
where
    F: Fn(ProcessEvent) + Send + Sync + 'static + Clone,
    C: Fn() -> Fut + Send + Sync + 'static + Clone,
    Fut: std::future::Future<Output = bool> + Send,
{
    callback(ProcessEvent::Log(format!("正在处理文件: {:?}", file_path)));

    let book = extractor::extract_text(file_path)?;
    let total_chapters = book.chapters.len();
    if total_chapters == 0 {
        callback(ProcessEvent::Log(format!("未提取到任何章节内容，跳过: {:?}", file_path)));
        return Ok(());
    }

    let book_dir = args.out.join(utils::sanitize_filename(&book.title));
    fs::create_dir_all(&book_dir).context("创建书籍输出目录失败")?;
    callback(ProcessEvent::Log(format!("书籍: {}，共识别到 {} 个章节", book.title, total_chapters)));

    let concurrency_limit = if args.concurrency > 0 { args.concurrency } else { 4 };
    callback(ProcessEvent::Log(format!("开始多线程合成，并发数: {}", concurrency_limit)));

    let book = Arc::new(book);

    let mut stream = stream::iter(book.chapters.clone().into_iter().enumerate())
        .map(|(idx, chapter)| {
            let book_dir = book_dir.clone();
            let args = args.clone();
            let client = client.clone();
            let blacklist = blacklist.clone();
            let callback = callback.clone();
            let check_cancel = check_cancel.clone();
            let book = Arc::clone(&book);
            async move {
                if check_cancel().await {
                    return Err(anyhow::anyhow!("任务已取消"));
                }
                process_chapter(idx, total_chapters, &book, chapter, &book_dir, &args, &client, &blacklist, callback, check_cancel).await
            }
        })
        .buffer_unordered(concurrency_limit);

    while let Some(result) = stream.next().await {
        if let Err(e) = result {
            if e.to_string() == "任务已取消" {
                return Err(e);
            }
                callback(ProcessEvent::Log(format!("处理章节时发生严重错误: {}", e)));
        }
    }

    // 计算输出目录大小
    let mut total_size = 0;
    if let Ok(entries) = fs::read_dir(&book_dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total_size += meta.len();
                }
            }
        }
    }
    callback(ProcessEvent::Success { size: total_size, output_path: book_dir.to_string_lossy().to_string() });
    callback(ProcessEvent::Log(format!("书籍处理完成: {:?}", book_dir)));
    Ok(())
}

pub async fn process_directory<F>(
    dir_path: &Path,
    args: &Cli,
    client: &ApiClient,
    blacklist: &Option<Regex>,
    callback: F,
) -> Result<()>
where
    F: Fn(ProcessEvent) + Send + Sync + 'static + Clone,
{
    let supported_extensions = ["txt", "epub"];
    let mut entries: Vec<PathBuf> = fs::read_dir(dir_path)?
        .filter_map(|res| res.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|ext| ext.to_str()).map(|ext| supported_extensions.contains(&ext.to_lowercase().as_str())).unwrap_or(false)
        })
        .collect();

    entries.sort();

    if entries.is_empty() {
        callback(ProcessEvent::Log(format!("在目录 {:?} 中没有找到支持的文件 (txt, epub)。", dir_path)));
        return Ok(());
    }

    callback(ProcessEvent::Log(format!("找到 {} 个文件，准备处理...", entries.len())));

    let txt_files: Vec<PathBuf> = entries.iter().filter(|p| p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))).cloned().collect();
    if !txt_files.is_empty() {
        callback(ProcessEvent::Log("正在扫描 TXT 文件编码...".to_string()));
        let files_to_convert = utils::pre_scan_for_encoding_issues(&txt_files)?;
        if !files_to_convert.is_empty() {
            callback(ProcessEvent::Log(format!("警告：检测到 {} 个文件可能不是 UTF-8 编码，请在 CLI 模式下交互处理。", files_to_convert.len())));
        } else {
            callback(ProcessEvent::Log("所有 TXT 文件编码兼容，准备开始 TTS 任务。".to_string()));
        }
    }

    callback(ProcessEvent::Log("--------------------------------------------------".to_string()));

    for entry in entries {
        if let Err(e) = process_file(&entry, args, client, blacklist, callback.clone(), || async { false }).await {
            callback(ProcessEvent::Log(format!("处理文件 {:?} 时出错: {}", entry, e)));
        }
        callback(ProcessEvent::Log("--------------------------------------------------".to_string()));
    }

    callback(ProcessEvent::Log("所有文件处理完毕。".to_string()));
    Ok(())
}
