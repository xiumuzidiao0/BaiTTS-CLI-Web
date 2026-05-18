use crate::ai;
use crate::api::ApiClient;
use crate::args::Cli;
use crate::extractor::{self, Book, Chapter};
use crate::lrc;
use crate::utils;
use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use hound::WavWriter;
use id3::TagLike;
use id3::frame::{Picture, PictureType};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub enum ProcessEvent {
    Log(String),
    Progress { current: usize, total: usize },
    Success { size: u64, output_path: String },
}

#[derive(Clone, Debug)]
struct BatchData {
    text: String,
    lines: Vec<String>,
    is_dialogue: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct DialogueAnalysisTable {
    pub schema_version: u32,
    pub file_path: String,
    pub novel_title: String,
    pub generated_at: u64,
    pub chapters: Vec<ChapterDialogueAnalysis>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChapterDialogueAnalysis {
    pub chapter_index: usize,
    pub chapter_title: String,
    pub dialogues: Vec<DialogueAnalysisEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DialogueAnalysisEntry {
    pub batch_index: usize,
    pub text: String,
    pub speaker: ai::SpeakerResult,
}

fn data_dir() -> PathBuf {
    if PathBuf::from("/data").exists() {
        PathBuf::from("/data")
    } else {
        PathBuf::from("data")
    }
}

fn dialogue_analysis_dir() -> PathBuf {
    data_dir().join("dialogue_analysis")
}

pub fn dialogue_analysis_file_path(file_path: &Path) -> PathBuf {
    let hash = format!("{:x}", md5::compute(file_path.to_string_lossy().as_bytes()));
    dialogue_analysis_dir().join(format!("{}.json", hash))
}

pub fn load_dialogue_analysis_table(file_path: &Path) -> Option<DialogueAnalysisTable> {
    let path = dialogue_analysis_file_path(file_path);
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str::<DialogueAnalysisTable>(&content).ok()
}

pub fn save_dialogue_analysis_table(table: &DialogueAnalysisTable) -> Result<()> {
    fs::create_dir_all(dialogue_analysis_dir()).context("创建对话分析目录失败")?;
    let path = dialogue_analysis_file_path(Path::new(&table.file_path));
    let content = serde_json::to_string_pretty(table).context("序列化对话分析结果失败")?;
    fs::write(path, content).context("保存对话分析结果失败")?;
    Ok(())
}

fn upsert_chapter_analysis(
    table: &mut DialogueAnalysisTable,
    chapter_index: usize,
    chapter_title: String,
    dialogues: Vec<DialogueAnalysisEntry>,
) {
    if let Some(existing) = table
        .chapters
        .iter_mut()
        .find(|chapter| chapter.chapter_index == chapter_index)
    {
        existing.chapter_title = chapter_title;
        existing.dialogues = dialogues;
    } else {
        table.chapters.push(ChapterDialogueAnalysis {
            chapter_index,
            chapter_title,
            dialogues,
        });
        table.chapters.sort_by_key(|chapter| chapter.chapter_index);
    }
}

fn window_chars(text: &str, center_text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text.to_string();
    }

    let center = text
        .find(center_text)
        .map(|byte_idx| text[..byte_idx].chars().count())
        .unwrap_or(0);
    let half = max_chars / 2;
    let start = center.saturating_sub(half);
    text.chars().skip(start).take(max_chars).collect()
}

fn speaker_context(chapter_content: &str, dialogue: &str, max_chars: usize) -> String {
    let local = window_chars(chapter_content, dialogue, max_chars);
    format!(
        "【当前重点对话】\n{}\n\n【对话附近上下文】\n{}",
        dialogue, local
    )
}

fn parse_manual_speaker_tag(text: &str) -> Option<(String, Option<ai::VoiceCategory>, String)> {
    let re = Regex::new(r"^<<([^<>（）()|]{1,50})(?:[（(]([^（）()<>|]{1,10})[）)])?>>").ok()?;
    let captures = re.captures(text.trim_start())?;
    let full = captures.get(0)?.as_str();
    let name = captures.get(1)?.as_str().trim().to_string();
    if name.is_empty() {
        return None;
    }
    let category = captures
        .get(2)
        .and_then(|m| voice_category_from_label(m.as_str().trim()));
    Some((name, category, full.to_string()))
}

fn voice_category_from_label(label: &str) -> Option<ai::VoiceCategory> {
    match label {
        "男童" | "男儿童" => Some(ai::VoiceCategory::MaleChild),
        "女童" | "女儿童" => Some(ai::VoiceCategory::FemaleChild),
        "少男" | "男少年" => Some(ai::VoiceCategory::MaleTeen),
        "少女" | "女少年" => Some(ai::VoiceCategory::FemaleTeen),
        "男青年" => Some(ai::VoiceCategory::MaleYouth),
        "女青年" => Some(ai::VoiceCategory::FemaleYouth),
        "男中年" => Some(ai::VoiceCategory::MaleMiddleAge),
        "女中年" => Some(ai::VoiceCategory::FemaleMiddleAge),
        "男老年" => Some(ai::VoiceCategory::MaleElder),
        "女老年" => Some(ai::VoiceCategory::FemaleElder),
        _ => None,
    }
}

fn trim_for_log(text: &str) -> String {
    let mut value: String = text.chars().take(40).collect();
    if text.chars().count() > 40 {
        value.push_str("...");
    }
    value.replace('\n', " ")
}

fn save_allocation_table_live(table: &ai::VoiceAllocationTable) {
    let mut table = table.clone();
    table.normalize_legacy_text_fields();
    let data_dir = if PathBuf::from("/data").exists() {
        PathBuf::from("/data")
    } else {
        PathBuf::from("data")
    };
    let dir = data_dir.join("allocations");
    let _ = fs::create_dir_all(&dir);
    let hash = format!("{:x}", md5::compute(table.file_path.as_bytes()));
    let path = dir.join(format!("{}.json", hash));
    if let Ok(content) = serde_json::to_string_pretty(&table) {
        if fs::write(&path, content).is_ok() {
            let target = table.file_path.replace('\\', "/");
            if let Ok(rd) = fs::read_dir(&dir) {
                for entry in rd.flatten() {
                    let duplicate = entry.path();
                    if duplicate == path {
                        continue;
                    }
                    let Ok(content) = fs::read_to_string(&duplicate) else {
                        continue;
                    };
                    let Ok(other) = serde_json::from_str::<ai::VoiceAllocationTable>(&content)
                    else {
                        continue;
                    };
                    if other.file_path.replace('\\', "/") == target {
                        let _ = fs::remove_file(duplicate);
                    }
                }
            }
        }
    }
}

fn known_characters_from_table(
    config: &ai::AiDialogueConfig,
    alloc_table: Option<&Mutex<ai::VoiceAllocationTable>>,
) -> Vec<ai::CharacterVoice> {
    let mut known = config.characters.clone();

    if let Some(table) = alloc_table {
        let table = table.lock().unwrap();
        for entry in &table.entries {
            if ai::is_suspicious_ai_name(&entry.character_name) {
                continue;
            }
            if known.iter().any(|c| c.name == entry.character_name) {
                continue;
            }

            known.push(ai::CharacterVoice {
                name: entry.character_name.clone(),
                aliases: entry
                    .aliases
                    .iter()
                    .filter(|alias| !ai::is_suspicious_ai_name(alias))
                    .cloned()
                    .collect(),
                gender: None,
                age: None,
                category: entry.category.clone(),
                voice_id: if entry.voice_id.is_empty() {
                    None
                } else {
                    Some(entry.voice_id.clone())
                },
                enabled: true,
            });
        }
    }

    known
}

fn build_chapter_batches(
    chapter: &Chapter,
    blacklist: &Option<Regex>,
    ignore_regex: &Regex,
) -> Vec<BatchData> {
    let lines: Vec<&str> = chapter.content.lines().collect();
    let mut batches: Vec<BatchData> = Vec::new();
    const MAX_BATCH_CHARS: usize = 300;

    let dialogue_regex = Regex::new(r"“[^”]*”|「[^」]*」").unwrap();
    let mut current_batch = BatchData {
        text: String::new(),
        lines: Vec::new(),
        is_dialogue: false,
    };
    let mut is_batch_empty = true;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let processed_line = if let Some(bl_regex) = blacklist {
            utils::apply_blacklist(trimmed, bl_regex)
        } else {
            trimmed.to_string()
        };

        let processed_line = ignore_regex.replace_all(&processed_line, "").to_string();
        if processed_line.trim().is_empty() {
            continue;
        }

        let mut last_end = 0;
        let mut segments = Vec::new();

        for mat in dialogue_regex.find_iter(&processed_line) {
            if mat.start() > last_end {
                segments.push((&processed_line[last_end..mat.start()], false));
            }
            segments.push((mat.as_str(), true));
            last_end = mat.end();
        }
        if last_end < processed_line.len() {
            segments.push((&processed_line[last_end..], false));
        }

        for (seg_text, is_dialogue) in segments {
            if seg_text.trim().is_empty() {
                continue;
            }

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

            if !current_batch.text.is_empty()
                && (current_batch.text.len() + seg_text.len() > MAX_BATCH_CHARS)
            {
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

    batches
}

async fn analyze_chapter_ai_speakers<F, C, W, Fut, PauseFut>(
    book_title: &str,
    chapter: &Chapter,
    batches: &[BatchData],
    args: &Cli,
    alloc_table: Option<&Mutex<ai::VoiceAllocationTable>>,
    callback: F,
    check_cancel: C,
    wait_if_paused: W,
    mut chapter_ai_speakers: HashMap<usize, ai::SpeakerResult>,
) -> Result<HashMap<usize, ai::SpeakerResult>>
where
    F: Fn(ProcessEvent) + Send + Sync + 'static + Clone,
    C: Fn() -> Fut + Send + Sync + 'static + Clone,
    W: Fn() -> PauseFut + Send + Sync + 'static + Clone,
    Fut: std::future::Future<Output = bool> + Send,
    PauseFut: std::future::Future<Output = ()> + Send,
{
    if !ai::should_use_ai(&args.ai_dialogue) || !args.ai_dialogue.chapter_analysis_enabled {
        return Ok(chapter_ai_speakers);
    }

    let dialogue_inputs: Vec<ai::ChapterDialogueInput> = batches
        .iter()
        .enumerate()
        .filter(|(index, batch)| {
            batch.is_dialogue
                && parse_manual_speaker_tag(&batch.text).is_none()
                && !chapter_ai_speakers.contains_key(index)
        })
        .map(|(index, batch)| ai::ChapterDialogueInput {
            index,
            text: batch.text.clone(),
        })
        .collect();

    if dialogue_inputs.is_empty() {
        return Ok(chapter_ai_speakers);
    }

    ai::record_ai_chapter_analyzed(book_title).await;

    callback(ProcessEvent::Log(format!(
        "AI 章节级识别: '{}' 共 {} 段对话",
        chapter.title,
        dialogue_inputs.len()
    )));

    let known = known_characters_from_table(&args.ai_dialogue, alloc_table);
    wait_if_paused().await;
    if check_cancel().await {
        return Err(anyhow::anyhow!("任务已取消"));
    }

    match ai::identify_chapter_speakers(
        &args.ai_dialogue,
        &chapter.title,
        &chapter.content,
        &dialogue_inputs,
        &known,
        Some(book_title),
    )
    .await
    {
        Ok(results) => {
            for result in results {
                if dialogue_inputs
                    .iter()
                    .any(|item| item.index == result.index)
                {
                    chapter_ai_speakers.insert(result.index, result.into_speaker_result());
                }
            }
        }
        Err(e) => {
            callback(ProcessEvent::Log(format!(
                "AI 章节整章识别失败，将回退分块识别: {}",
                e
            )));
        }
    }

    let hit_rate = chapter_ai_speakers.len() as f32
        / batches
            .iter()
            .enumerate()
            .filter(|(index, batch)| {
                batch.is_dialogue
                    && parse_manual_speaker_tag(&batch.text).is_none()
                    && (chapter_ai_speakers.contains_key(index)
                        || dialogue_inputs.iter().any(|item| item.index == *index))
            })
            .count()
            .max(1) as f32;

    let missing_inputs: Vec<ai::ChapterDialogueInput> = dialogue_inputs
        .iter()
        .filter(|item| !chapter_ai_speakers.contains_key(&item.index))
        .cloned()
        .collect();

    if !missing_inputs.is_empty() && hit_rate < 0.8 {
        callback(ProcessEvent::Log(format!(
            "AI 章节整章识别命中 {}/{}，继续分块补齐 {} 段",
            dialogue_inputs.len() - missing_inputs.len(),
            dialogue_inputs.len(),
            missing_inputs.len()
        )));
    }

    if hit_rate < 0.8 {
        for chunk in missing_inputs.chunks(10) {
            wait_if_paused().await;
            if check_cancel().await {
                return Err(anyhow::anyhow!("任务已取消"));
            }

            match ai::identify_chapter_speakers(
                &args.ai_dialogue,
                &chapter.title,
                &chapter.content,
                chunk,
                &known,
                Some(book_title),
            )
            .await
            {
                Ok(results) => {
                    for result in results {
                        if chunk.iter().any(|item| item.index == result.index) {
                            chapter_ai_speakers.insert(result.index, result.into_speaker_result());
                        }
                    }
                }
                Err(e) => {
                    callback(ProcessEvent::Log(format!(
                        "AI 章节分块识别失败，未识别片段将回退逐句识别: {}",
                        e
                    )));
                }
            }
        }
    }

    callback(ProcessEvent::Log(format!(
        "AI 章节级识别完成: 命中 {}/{} 段对话",
        dialogue_inputs
            .iter()
            .filter(|item| chapter_ai_speakers.contains_key(&item.index))
            .count(),
        dialogue_inputs.len()
    )));

    Ok(chapter_ai_speakers)
}

fn record_speaker_allocation<F>(
    args: &Cli,
    voice_allocator: &Mutex<ai::VoiceAllocator>,
    alloc_table: Option<&Mutex<ai::VoiceAllocationTable>>,
    speaker: &ai::SpeakerResult,
    callback: &F,
) where
    F: Fn(ProcessEvent),
{
    let Some(at) = alloc_table else {
        return;
    };

    if ai::is_suspicious_ai_name(&speaker.name) {
        callback(ProcessEvent::Log(format!(
            "AI 角色名疑似乱码，跳过分配表写入: {}",
            speaker.name
        )));
        return;
    }

    let is_crowd_speaker = speaker.name.contains("群众") || speaker.name.contains("缇や紬");
    if is_crowd_speaker && !args.ai_dialogue.save_crowd_characters {
        return;
    }

    let existing_entry = at.lock().unwrap().lookup_match(&speaker.name).cloned();
    let matched_character =
        ai::match_character(&args.ai_dialogue.characters, &speaker.name).cloned();
    let dialogue_voice = if let Some(entry) = existing_entry.as_ref().filter(|entry| {
        (entry.locked || entry.source == ai::AllocationSource::Manual) && !entry.voice_id.is_empty()
    }) {
        Some(entry.voice_id.clone())
    } else if let Some(character) = matched_character.as_ref() {
        let mut va = voice_allocator.lock().unwrap();
        ai::resolve_character_voice(
            &args.ai_dialogue,
            &mut va,
            character,
            speaker.gender.as_deref(),
            speaker.age.as_deref(),
        )
    } else {
        let mut va = voice_allocator.lock().unwrap();
        ai::resolve_speaker_voice(
            &args.ai_dialogue,
            &mut va,
            &speaker.name,
            speaker.gender.as_deref(),
            speaker.age.as_deref(),
        )
    };

    let mut table = at.lock().unwrap();
    let existing = table.lookup_match(&speaker.name).cloned();
    if existing.as_ref().map(|entry| entry.locked).unwrap_or(false) {
        save_allocation_table_live(&table);
        return;
    }

    let merged_aliases = table.merge_alias_for_match(&speaker.name);
    let category = matched_character
        .as_ref()
        .and_then(|character| character.category.clone())
        .or_else(|| ai::VoiceCategory::infer(speaker.gender.as_deref(), speaker.age.as_deref()))
        .or_else(|| existing.as_ref().and_then(|entry| entry.category.clone()));
    let mut aliases = matched_character
        .as_ref()
        .map(|character| character.aliases.clone())
        .or_else(|| merged_aliases.clone())
        .or_else(|| existing.as_ref().map(|entry| entry.aliases.clone()))
        .unwrap_or_default();
    if let Some(merged) = merged_aliases {
        for alias in merged {
            let normalized = alias.trim();
            if !normalized.is_empty() && !aliases.iter().any(|existing| existing == normalized) {
                aliases.push(alias);
            }
        }
    }
    let source = if matched_character
        .as_ref()
        .and_then(|character| character.voice_id.as_ref())
        .is_some()
    {
        ai::AllocationSource::CharacterOverride
    } else if matched_character
        .as_ref()
        .and_then(|character| character.category.as_ref())
        .is_some()
    {
        ai::AllocationSource::CharacterCategory
    } else if existing
        .as_ref()
        .map(|entry| entry.source == ai::AllocationSource::Manual)
        .unwrap_or(false)
    {
        ai::AllocationSource::Manual
    } else {
        ai::AllocationSource::AIInferred
    };
    let voice_id = dialogue_voice.unwrap_or_default();

    table.upsert_ai_result(ai::VoiceAllocationEntry {
        character_name: matched_character
            .as_ref()
            .map(|character| character.name.clone())
            .or_else(|| existing.as_ref().map(|entry| entry.character_name.clone()))
            .unwrap_or_else(|| speaker.name.clone()),
        aliases,
        category: category.clone(),
        category_label: category.as_ref().map(|c| c.label().to_string()),
        voice_id: voice_id.clone(),
        source,
        locked: true,
        volume: None,
        speed: None,
        pitch: None,
        confidence: speaker.confidence,
        reason: speaker.reason.clone(),
        needs_review: speaker.confidence.map_or(true, |v| v < 0.6),
    });
    save_allocation_table_live(&table);
    callback(ProcessEvent::Log(format!(
        "分析写入分配表: {} (分类:{}, 声音:{})",
        speaker.name,
        category.as_ref().map(|c| c.label()).unwrap_or("?"),
        if voice_id.is_empty() {
            "无"
        } else {
            voice_id.as_str()
        }
    )));
}

async fn process_chapter<F, C, W, Fut, PauseFut>(
    idx: usize,
    total_chapters: usize,
    book: &Book,
    chapter: Chapter,
    book_dir: &Path,
    args: &Cli,
    client: &ApiClient,
    blacklist: &Option<Regex>,
    voice_allocator: &Mutex<ai::VoiceAllocator>,
    alloc_table: Option<&Mutex<ai::VoiceAllocationTable>>,
    saved_analysis: Option<&ChapterDialogueAnalysis>,
    require_existing_analysis: bool,
    callback: F,
    check_cancel: C,
    wait_if_paused: W,
) -> Result<()>
where
    F: Fn(ProcessEvent) + Send + Sync + 'static + Clone,
    C: Fn() -> Fut + Send + Sync + 'static + Clone,
    W: Fn() -> PauseFut + Send + Sync + 'static + Clone,
    Fut: std::future::Future<Output = bool> + Send,
    PauseFut: std::future::Future<Output = ()> + Send,
{
    let clean_title = utils::sanitize_filename(&chapter.title);
    let chapter_wav_filename = format!("{}_{}.wav", idx + 1, clean_title);
    let output_path = book_dir.join(&chapter_wav_filename);

    if output_path.exists() && output_path.metadata()?.len() > 0 {
        callback(ProcessEvent::Log(format!(
            "跳过已存在的章节: {}",
            chapter_wav_filename
        )));
        callback(ProcessEvent::Progress {
            current: idx + 1,
            total: total_chapters,
        });
        return Ok(());
    }

    callback(ProcessEvent::Log(format!(
        "正在处理章节 [{}/{}]: {}",
        idx + 1,
        total_chapters,
        chapter.title
    )));

    let lines: Vec<&str> = chapter.content.lines().collect();
    if lines.is_empty() {
        callback(ProcessEvent::Log(format!(
            "章节内容为空，跳过: {}",
            chapter.title
        )));
        callback(ProcessEvent::Progress {
            current: idx + 1,
            total: total_chapters,
        });
        return Ok(());
    }

    let mut all_wav_samples: Vec<i16> = Vec::new();
    let mut wav_spec: Option<hound::WavSpec> = None;
    let mut lyrics_data: Vec<(u32, String)> = Vec::new();
    let mut current_timestamp = Duration::from_secs(0);

    let ignore_regex =
        Regex::new(&args.ignore_regex).unwrap_or_else(|_| Regex::new(r"\*{3,}|#{2,}").unwrap());
    let batches = build_chapter_batches(&chapter, blacklist, &ignore_regex);
    let usage_title = args.output_name.as_deref().unwrap_or(&book.title);

    let mut chapter_ai_speakers: HashMap<usize, ai::SpeakerResult> = HashMap::new();
    if let Some(saved) = saved_analysis {
        let mut loaded = 0;
        for entry in &saved.dialogues {
            if entry.batch_index < batches.len()
                && batches[entry.batch_index].is_dialogue
                && batches[entry.batch_index].text == entry.text
            {
                chapter_ai_speakers.insert(entry.batch_index, entry.speaker.clone());
                loaded += 1;
            }
        }
        if loaded > 0 {
            callback(ProcessEvent::Log(format!(
                "已加载保存的对话分析: '{}' 命中 {} 段",
                chapter.title, loaded
            )));
        }
    }
    if require_existing_analysis {
        let missing_saved_dialogues = batches
            .iter()
            .enumerate()
            .filter(|(_, batch)| {
                batch.is_dialogue && parse_manual_speaker_tag(&batch.text).is_none()
            })
            .filter(|(index, _)| !chapter_ai_speakers.contains_key(index))
            .count();
        if missing_saved_dialogues > 0 {
            return Err(anyhow::anyhow!(
                "第 {} 章 '{}' 还有 {} 段对话未分析，请继续分析后再合成",
                idx + 1,
                chapter.title,
                missing_saved_dialogues
            ));
        }
    } else {
        chapter_ai_speakers = analyze_chapter_ai_speakers(
            usage_title,
            &chapter,
            &batches,
            args,
            alloc_table,
            callback.clone(),
            check_cancel.clone(),
            wait_if_paused.clone(),
            chapter_ai_speakers,
        )
        .await?;
    }

    for (i, batch) in batches.iter().enumerate() {
        wait_if_paused().await;
        if check_cancel().await {
            return Err(anyhow::anyhow!("任务已取消"));
        }
        if i % 5 == 0 && i > 0 {
            callback(ProcessEvent::Log(format!(
                "正在处理章节 '{}': 批次 {}/{}",
                chapter.title,
                i + 1,
                batches.len()
            )));
        }
        let manual_tag = if batch.is_dialogue {
            parse_manual_speaker_tag(&batch.text)
        } else {
            None
        };
        let (target_voice, volume, speed, pitch) = if batch.is_dialogue {
            let mut dialogue_voice = args
                .voice_dialogue
                .as_ref()
                .or(args.voice.as_ref())
                .cloned();
            let mut dialogue_entry_params: Option<(Option<u8>, Option<u8>, Option<u8>)> = None;
            if let Some((manual_name, manual_category, _tag_text)) = manual_tag.as_ref() {
                let existing_entry = alloc_table
                    .and_then(|at| at.lock().unwrap().lookup_match(manual_name).cloned());
                dialogue_entry_params = existing_entry
                    .as_ref()
                    .map(|entry| (entry.volume, entry.speed, entry.pitch));
                if let Some(entry) = existing_entry
                    .as_ref()
                    .filter(|entry| !entry.voice_id.is_empty())
                {
                    dialogue_voice = Some(entry.voice_id.clone());
                } else if let Some(category) = manual_category.as_ref() {
                    let voice = {
                        let mut va = voice_allocator.lock().unwrap();
                        va.allocate(&args.ai_dialogue.voice_pool, category, manual_name)
                    };
                    if let Some(voice_id) = voice {
                        dialogue_voice = Some(voice_id);
                    }
                }
                if let Some(at) = alloc_table {
                    let mut table = at.lock().unwrap();
                    if table
                        .lookup_match(manual_name)
                        .map(|entry| entry.locked)
                        .unwrap_or(false)
                    {
                        save_allocation_table_live(&table);
                    } else {
                        let category = manual_category.clone().or_else(|| {
                            existing_entry
                                .as_ref()
                                .and_then(|entry| entry.category.clone())
                        });
                        let voice_id = dialogue_voice.clone().unwrap_or_default();
                        table.upsert_ai_result(ai::VoiceAllocationEntry {
                            character_name: existing_entry
                                .as_ref()
                                .map(|entry| entry.character_name.clone())
                                .unwrap_or_else(|| manual_name.clone()),
                            aliases: existing_entry
                                .as_ref()
                                .map(|entry| entry.aliases.clone())
                                .unwrap_or_default(),
                            category: category.clone(),
                            category_label: category.as_ref().map(|c| c.label().to_string()),
                            voice_id,
                            source: ai::AllocationSource::Manual,
                            locked: true,
                            volume: None,
                            speed: None,
                            pitch: None,
                            confidence: Some(1.0),
                            reason: Some("手动标注".to_string()),
                            needs_review: false,
                        });
                        save_allocation_table_live(&table);
                    }
                }
                callback(ProcessEvent::Log(format!(
                    "手动角色标注: '{}' -> {}",
                    trim_for_log(&batch.text),
                    manual_name
                )));
            } else if chapter_ai_speakers.contains_key(&i)
                || (!require_existing_analysis && ai::should_use_ai(&args.ai_dialogue))
            {
                let context = speaker_context(
                    &chapter.content,
                    &batch.text,
                    args.ai_dialogue.context_chars,
                );
                let known = known_characters_from_table(&args.ai_dialogue, alloc_table);
                let speaker_result = if let Some(speaker) = chapter_ai_speakers.get(&i).cloned() {
                    Ok(speaker)
                } else {
                    ai::identify_speaker(
                        &args.ai_dialogue,
                        &batch.text,
                        &context,
                        &known,
                        Some(usage_title),
                    )
                    .await
                };
                match speaker_result {
                    Ok(speaker) => {
                        if ai::is_suspicious_ai_name(&speaker.name) {
                            callback(ProcessEvent::Log(format!(
                                "AI dialogue speaker '{}' 疑似乱码，使用默认对话声音且不写入分配表",
                                speaker.name
                            )));
                        } else {
                            let is_crowd_speaker = speaker.name.contains("群众");
                            let existing_entry = alloc_table.and_then(|at| {
                                at.lock().unwrap().lookup_match(&speaker.name).cloned()
                            });
                            dialogue_entry_params = existing_entry
                                .as_ref()
                                .map(|entry| (entry.volume, entry.speed, entry.pitch));
                            let matched_character =
                                ai::match_character(&args.ai_dialogue.characters, &speaker.name)
                                    .cloned();

                            if let Some(entry) = existing_entry.as_ref().filter(|entry| {
                                (entry.locked || entry.source == ai::AllocationSource::Manual)
                                    && !entry.voice_id.is_empty()
                            }) {
                                dialogue_voice = Some(entry.voice_id.clone());
                                callback(ProcessEvent::Log(format!(
                                    "AI dialogue: '{}' -> {} (使用分配表声音: {})",
                                    trim_for_log(&batch.text),
                                    entry.character_name,
                                    entry.voice_id
                                )));
                            } else if let Some(character) = matched_character.as_ref() {
                                let resolved = {
                                    let mut va = voice_allocator.lock().unwrap();
                                    ai::resolve_character_voice(
                                        &args.ai_dialogue,
                                        &mut va,
                                        character,
                                        speaker.gender.as_deref(),
                                        speaker.age.as_deref(),
                                    )
                                };
                                if let Some(voice_id) = resolved {
                                    dialogue_voice = Some(voice_id.clone());
                                    let cat_label = character
                                        .category
                                        .as_ref()
                                        .map(|c| c.label())
                                        .unwrap_or("none");
                                    callback(ProcessEvent::Log(format!(
                                        "AI dialogue: '{}' -> {} (category: {}, voice: {})",
                                        trim_for_log(&batch.text),
                                        character.name,
                                        cat_label,
                                        voice_id
                                    )));
                                } else {
                                    callback(ProcessEvent::Log(format!(
                                        "AI dialogue: '{}' -> {} (no voice in pool, using fallback)",
                                        trim_for_log(&batch.text),
                                        character.name
                                    )));
                                }
                            } else {
                                let auto_voice = {
                                    let mut va = voice_allocator.lock().unwrap();
                                    ai::resolve_speaker_voice(
                                        &args.ai_dialogue,
                                        &mut va,
                                        &speaker.name,
                                        speaker.gender.as_deref(),
                                        speaker.age.as_deref(),
                                    )
                                };
                                if let Some(voice_id) = auto_voice {
                                    dialogue_voice = Some(voice_id.clone());
                                    callback(ProcessEvent::Log(format!(
                                        "AI dialogue: '{}' -> speaker '{}' ({}/{}), auto pool: {}",
                                        trim_for_log(&batch.text),
                                        speaker.name,
                                        speaker.gender.as_deref().unwrap_or("?"),
                                        speaker.age.as_deref().unwrap_or("?"),
                                        voice_id
                                    )));
                                } else {
                                    callback(ProcessEvent::Log(format!(
                                        "AI dialogue speaker '{}' ({}/{}) not matched; using default dialogue voice",
                                        speaker.name,
                                        speaker.gender.as_deref().unwrap_or("?"),
                                        speaker.age.as_deref().unwrap_or("?")
                                    )));
                                }
                            }

                            if let Some(at) = alloc_table.filter(|_| {
                                args.ai_dialogue.save_crowd_characters || !is_crowd_speaker
                            }) {
                                let mut table = at.lock().unwrap();
                                let existing = table.lookup_match(&speaker.name).cloned();
                                let locked =
                                    existing.as_ref().map(|entry| entry.locked).unwrap_or(false);
                                let merged_aliases = table.merge_alias_for_match(&speaker.name);

                                if locked {
                                    save_allocation_table_live(&table);
                                    callback(ProcessEvent::Log(format!(
                                        "分配表已锁定，跳过覆盖: {}",
                                        speaker.name
                                    )));
                                } else {
                                    let category = matched_character
                                        .as_ref()
                                        .and_then(|character| character.category.clone())
                                        .or_else(|| {
                                            ai::VoiceCategory::infer(
                                                speaker.gender.as_deref(),
                                                speaker.age.as_deref(),
                                            )
                                        })
                                        .or_else(|| {
                                            existing
                                                .as_ref()
                                                .and_then(|entry| entry.category.clone())
                                        });
                                    let mut aliases = matched_character
                                        .as_ref()
                                        .map(|character| character.aliases.clone())
                                        .or_else(|| merged_aliases.clone())
                                        .or_else(|| {
                                            existing.as_ref().map(|entry| entry.aliases.clone())
                                        })
                                        .unwrap_or_default();
                                    if let Some(merged) = merged_aliases {
                                        for alias in merged {
                                            let normalized = alias.trim();
                                            if !normalized.is_empty()
                                                && !aliases
                                                    .iter()
                                                    .any(|existing| existing == normalized)
                                            {
                                                aliases.push(alias);
                                            }
                                        }
                                    }
                                    let source = if matched_character
                                        .as_ref()
                                        .and_then(|character| character.voice_id.as_ref())
                                        .is_some()
                                    {
                                        ai::AllocationSource::CharacterOverride
                                    } else if matched_character
                                        .as_ref()
                                        .and_then(|character| character.category.as_ref())
                                        .is_some()
                                    {
                                        ai::AllocationSource::CharacterCategory
                                    } else if existing
                                        .as_ref()
                                        .map(|entry| entry.source == ai::AllocationSource::Manual)
                                        .unwrap_or(false)
                                    {
                                        ai::AllocationSource::Manual
                                    } else {
                                        ai::AllocationSource::AIInferred
                                    };
                                    let voice_id = dialogue_voice.clone().unwrap_or_default();

                                    table.upsert_ai_result(ai::VoiceAllocationEntry {
                                        character_name: matched_character
                                            .as_ref()
                                            .map(|character| character.name.clone())
                                            .or_else(|| {
                                                existing
                                                    .as_ref()
                                                    .map(|entry| entry.character_name.clone())
                                            })
                                            .unwrap_or_else(|| speaker.name.clone()),
                                        aliases,
                                        category: category.clone(),
                                        category_label: category
                                            .as_ref()
                                            .map(|c| c.label().to_string()),
                                        voice_id: voice_id.clone(),
                                        source,
                                        locked: true,
                                        volume: None,
                                        speed: None,
                                        pitch: None,
                                        confidence: speaker.confidence,
                                        reason: speaker.reason.clone(),
                                        needs_review: speaker.confidence.map_or(true, |v| v < 0.6),
                                    });
                                    save_allocation_table_live(&table);

                                    callback(ProcessEvent::Log(format!(
                                        "分配表写入: {} (分类:{}, 声音:{})",
                                        speaker.name,
                                        category.as_ref().map(|c| c.label()).unwrap_or("?"),
                                        if voice_id.is_empty() {
                                            "无"
                                        } else {
                                            voice_id.as_str()
                                        }
                                    )));
                                }
                            } else if is_crowd_speaker {
                                callback(ProcessEvent::Log(format!(
                                    "群众角色未写入分配表: {}",
                                    speaker.name
                                )));
                            }
                        }
                    }
                    Err(e) => {
                        callback(ProcessEvent::Log(format!(
                            "AI dialogue assignment failed; using default dialogue voice: {}",
                            e
                        )));
                    }
                }
            }
            let (entry_volume, entry_speed, entry_pitch) =
                dialogue_entry_params.unwrap_or((None, None, None));
            (
                dialogue_voice,
                Some(entry_volume.or(args.volume_dialogue).unwrap_or(args.volume)),
                Some(entry_speed.or(args.speed_dialogue).unwrap_or(args.speed)),
                Some(entry_pitch.or(args.pitch_dialogue).unwrap_or(args.pitch)),
            )
        } else {
            (
                args.voice.as_ref().cloned(),
                Some(args.volume),
                Some(args.speed),
                Some(args.pitch),
            )
        };

        let tts_text = if let Some((_, _, tag_text)) = manual_tag.as_ref() {
            batch.text.replacen(tag_text, "", 1)
        } else {
            batch.text.clone()
        };

        let wav_data = match client
            .generate_speech(&tts_text, &target_voice, &volume, &speed, &pitch)
            .await
        {
            Ok(data) => data,
            Err(e) => {
                callback(ProcessEvent::Log(format!(
                    "警告: 章节 '{}' 批次 {} 转换失败，跳过。原因: {}",
                    chapter.title,
                    i + 1,
                    e
                )));
                continue;
            }
        };
        if wav_data.is_empty() {
            continue;
        }

        let mut reader = hound::WavReader::new(Cursor::new(&wav_data))?;
        let spec = reader.spec();
        if wav_spec.is_none() {
            wav_spec = Some(spec);
        }

        let samples: Vec<i16> = reader.samples().collect::<Result<_, _>>()?;
        let total_duration_ms =
            (samples.len() as f64 / spec.channels as f64 / spec.sample_rate as f64) * 1000.0;
        let total_chars: usize = batch.lines.iter().map(|l| l.chars().count()).sum();

        if total_chars > 0 {
            let chars_per_ms = total_chars as f64 / total_duration_ms;
            for origin_line in &batch.lines {
                let line_chars = origin_line.chars().count();
                let line_duration =
                    Duration::from_millis((line_chars as f64 / chars_per_ms) as u64);

                if args.sub > 0 {
                    let chunks = lrc::split_line_intelligently(origin_line, args.sub);
                    let chunk_duration = line_duration / chunks.len().max(1) as u32;
                    for chunk in chunks {
                        lyrics_data.push((current_timestamp.as_millis() as u32, chunk));
                        current_timestamp += chunk_duration;
                    }
                } else {
                    lyrics_data.push((
                        current_timestamp.as_millis() as u32,
                        origin_line.to_string(),
                    ));
                    current_timestamp += line_duration;
                }
            }
        }
        all_wav_samples.extend(samples);
    }

    if all_wav_samples.is_empty() || wav_spec.is_none() {
        callback(ProcessEvent::Log(format!(
            "章节转换后无音频数据: {}",
            chapter.title
        )));
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

    callback(ProcessEvent::Log(format!(
        "章节完成: {}",
        chapter_wav_filename
    )));
    callback(ProcessEvent::Progress {
        current: idx + 1,
        total: total_chapters,
    });
    Ok(())
}

pub async fn analyze_file_dialogues<F, C, W, Fut, PauseFut>(
    file_path: &Path,
    args: &Cli,
    blacklist: &Option<Regex>,
    callback: F,
    check_cancel: C,
    wait_if_paused: W,
    allocation_table: Option<Arc<Mutex<ai::VoiceAllocationTable>>>,
) -> Result<()>
where
    F: Fn(ProcessEvent) + Send + Sync + 'static + Clone,
    C: Fn() -> Fut + Send + Sync + 'static + Clone,
    W: Fn() -> PauseFut + Send + Sync + 'static + Clone,
    Fut: std::future::Future<Output = bool> + Send,
    PauseFut: std::future::Future<Output = ()> + Send,
{
    callback(ProcessEvent::Log(format!("开始分析文件: {:?}", file_path)));

    let book = extractor::extract_text(file_path)?;
    let total_chapters = book.chapters.len();
    if total_chapters == 0 {
        callback(ProcessEvent::Log(format!(
            "未提取到任何章节内容，跳过: {:?}",
            file_path
        )));
        return Ok(());
    }

    let usage_title = args.output_name.as_deref().unwrap_or(&book.title);

    callback(ProcessEvent::Log(format!(
        "书籍: {}，共识别到 {} 个章节，开始 AI 分析",
        usage_title, total_chapters
    )));

    let mut table =
        load_dialogue_analysis_table(file_path).unwrap_or_else(|| DialogueAnalysisTable {
            schema_version: 1,
            file_path: file_path.to_string_lossy().to_string(),
            novel_title: usage_title.to_string(),
            generated_at: 0,
            chapters: Vec::new(),
        });
    table.schema_version = 1;
    table.file_path = file_path.to_string_lossy().to_string();
    table.novel_title = usage_title.to_string();
    table.generated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let voice_allocator = Mutex::new(ai::VoiceAllocator::new());
    if let Some(ref at) = allocation_table {
        let table = at.lock().unwrap();
        for entry in &table.entries {
            if let Some(ref category) = entry.category {
                voice_allocator.lock().unwrap().pre_seed_category(
                    category,
                    &entry.character_name,
                    &entry.voice_id,
                );
            } else {
                voice_allocator
                    .lock()
                    .unwrap()
                    .pre_seed(&entry.character_name, &entry.voice_id);
            }
        }
    }

    let ignore_regex =
        Regex::new(&args.ignore_regex).unwrap_or_else(|_| Regex::new(r"\*{3,}|#{2,}").unwrap());

    for (idx, chapter) in book.chapters.iter().enumerate() {
        wait_if_paused().await;
        if check_cancel().await {
            return Err(anyhow::anyhow!("任务已取消"));
        }

        callback(ProcessEvent::Log(format!(
            "正在分析章节 [{}/{}]: {}",
            idx + 1,
            total_chapters,
            chapter.title
        )));

        let batches = build_chapter_batches(chapter, blacklist, &ignore_regex);
        let speakers = analyze_chapter_ai_speakers(
            usage_title,
            chapter,
            &batches,
            args,
            allocation_table.as_deref(),
            callback.clone(),
            check_cancel.clone(),
            wait_if_paused.clone(),
            HashMap::new(),
        )
        .await?;

        let mut entries: Vec<DialogueAnalysisEntry> = speakers
            .into_iter()
            .filter_map(|(batch_index, speaker)| {
                batches.get(batch_index).map(|batch| DialogueAnalysisEntry {
                    batch_index,
                    text: batch.text.clone(),
                    speaker,
                })
            })
            .collect();
        entries.sort_by_key(|entry| entry.batch_index);

        for entry in &entries {
            record_speaker_allocation(
                args,
                &voice_allocator,
                allocation_table.as_deref(),
                &entry.speaker,
                &callback,
            );
        }

        upsert_chapter_analysis(&mut table, idx, chapter.title.clone(), entries);
        save_dialogue_analysis_table(&table)?;
        if let Some(ref at) = allocation_table {
            save_allocation_table_live(&at.lock().unwrap());
        }

        callback(ProcessEvent::Progress {
            current: idx + 1,
            total: total_chapters,
        });
    }

    callback(ProcessEvent::Log(format!(
        "AI 分析完成，结果已保存: {}",
        dialogue_analysis_file_path(file_path).display()
    )));
    Ok(())
}

pub async fn process_file<F, C, W, Fut, PauseFut>(
    file_path: &Path,
    args: &Cli,
    client: &ApiClient,
    blacklist: &Option<Regex>,
    callback: F,
    check_cancel: C,
    wait_if_paused: W,
    allocation_table: Option<Arc<Mutex<ai::VoiceAllocationTable>>>,
) -> Result<()>
where
    F: Fn(ProcessEvent) + Send + Sync + 'static + Clone,
    C: Fn() -> Fut + Send + Sync + 'static + Clone,
    W: Fn() -> PauseFut + Send + Sync + 'static + Clone,
    Fut: std::future::Future<Output = bool> + Send,
    PauseFut: std::future::Future<Output = ()> + Send,
{
    process_file_inner(
        file_path,
        args,
        client,
        blacklist,
        callback,
        check_cancel,
        wait_if_paused,
        allocation_table,
        false,
    )
    .await
}

pub async fn process_file_with_existing_analysis<F, C, W, Fut, PauseFut>(
    file_path: &Path,
    args: &Cli,
    client: &ApiClient,
    blacklist: &Option<Regex>,
    callback: F,
    check_cancel: C,
    wait_if_paused: W,
    allocation_table: Option<Arc<Mutex<ai::VoiceAllocationTable>>>,
) -> Result<()>
where
    F: Fn(ProcessEvent) + Send + Sync + 'static + Clone,
    C: Fn() -> Fut + Send + Sync + 'static + Clone,
    W: Fn() -> PauseFut + Send + Sync + 'static + Clone,
    Fut: std::future::Future<Output = bool> + Send,
    PauseFut: std::future::Future<Output = ()> + Send,
{
    process_file_inner(
        file_path,
        args,
        client,
        blacklist,
        callback,
        check_cancel,
        wait_if_paused,
        allocation_table,
        true,
    )
    .await
}

async fn process_file_inner<F, C, W, Fut, PauseFut>(
    file_path: &Path,
    args: &Cli,
    client: &ApiClient,
    blacklist: &Option<Regex>,
    callback: F,
    check_cancel: C,
    wait_if_paused: W,
    allocation_table: Option<Arc<Mutex<ai::VoiceAllocationTable>>>,
    require_existing_analysis: bool,
) -> Result<()>
where
    F: Fn(ProcessEvent) + Send + Sync + 'static + Clone,
    C: Fn() -> Fut + Send + Sync + 'static + Clone,
    W: Fn() -> PauseFut + Send + Sync + 'static + Clone,
    Fut: std::future::Future<Output = bool> + Send,
    PauseFut: std::future::Future<Output = ()> + Send,
{
    callback(ProcessEvent::Log(format!("正在处理文件: {:?}", file_path)));

    let book = extractor::extract_text(file_path)?;
    let total_chapters = book.chapters.len();
    if total_chapters == 0 {
        callback(ProcessEvent::Log(format!(
            "未提取到任何章节内容，跳过: {:?}",
            file_path
        )));
        return Ok(());
    }

    let output_title = args.output_name.as_deref().unwrap_or(&book.title);
    let book_dir = args.out.join(utils::sanitize_filename(output_title));
    fs::create_dir_all(&book_dir).context("创建书籍输出目录失败")?;
    callback(ProcessEvent::Log(format!(
        "书籍: {}，共识别到 {} 个章节",
        output_title, total_chapters
    )));

    let concurrency_limit = if args.concurrency > 0 {
        args.concurrency
    } else {
        4
    };
    callback(ProcessEvent::Log(format!(
        "开始多线程合成，并发数: {}",
        concurrency_limit
    )));

    let book = Arc::new(book);
    let voice_allocator = Arc::new(Mutex::new(ai::VoiceAllocator::new()));
    let saved_analysis_by_chapter: Arc<HashMap<usize, ChapterDialogueAnalysis>> =
        match load_dialogue_analysis_table(file_path) {
            Some(table) => {
                let count: usize = table
                    .chapters
                    .iter()
                    .map(|chapter| chapter.dialogues.len())
                    .sum();
                if count > 0 {
                    callback(ProcessEvent::Log(format!(
                        "已加载对话分析结果: {} 章 / {} 段对话",
                        table.chapters.len(),
                        count
                    )));
                }
                Arc::new(
                    table
                        .chapters
                        .into_iter()
                        .map(|chapter| (chapter.chapter_index, chapter))
                        .collect(),
                )
            }
            None => Arc::new(HashMap::new()),
        };

    let chapters_to_process = if require_existing_analysis {
        let mut analyzed_count = 0usize;
        while analyzed_count < total_chapters
            && saved_analysis_by_chapter.contains_key(&analyzed_count)
        {
            analyzed_count += 1;
        }
        if analyzed_count == 0 {
            return Err(anyhow::anyhow!(
                "暂无可用的分析结果，请先继续分析后再开始合成"
            ));
        }
        if analyzed_count < total_chapters {
            callback(ProcessEvent::Log(format!(
                "分析结果只连续覆盖到第 {} 章，合成将停在此处；请继续分析后再继续合成。",
                analyzed_count
            )));
        }
        analyzed_count
    } else {
        total_chapters
    };

    // Pre-seed allocator from allocation table if provided
    if let Some(ref at) = allocation_table {
        let table = at.lock().unwrap();
        for entry in &table.entries {
            if let Some(ref category) = entry.category {
                voice_allocator.lock().unwrap().pre_seed_category(
                    category,
                    &entry.character_name,
                    &entry.voice_id,
                );
            } else {
                voice_allocator
                    .lock()
                    .unwrap()
                    .pre_seed(&entry.character_name, &entry.voice_id);
            }
        }
    }

    let mut stream = stream::iter(
        book.chapters
            .iter()
            .take(chapters_to_process)
            .cloned()
            .enumerate(),
    )
    .map(|(idx, chapter)| {
        let book_dir = book_dir.clone();
        let args = args.clone();
        let client = client.clone();
        let blacklist = blacklist.clone();
        let callback = callback.clone();
        let check_cancel = check_cancel.clone();
        let book = Arc::clone(&book);
        let voice_allocator = Arc::clone(&voice_allocator);
        let alloc_table = allocation_table.clone();
        let saved_analysis = saved_analysis_by_chapter.get(&idx).cloned();
        let wait_if_paused = wait_if_paused.clone();
        async move {
            if check_cancel().await {
                return Err(anyhow::anyhow!("任务已取消"));
            }
            process_chapter(
                idx,
                total_chapters,
                &book,
                chapter,
                &book_dir,
                &args,
                &client,
                &blacklist,
                &voice_allocator,
                alloc_table.as_deref(),
                saved_analysis.as_ref(),
                require_existing_analysis,
                callback,
                check_cancel,
                wait_if_paused.clone(),
            )
            .await
        }
    })
    .buffer_unordered(concurrency_limit);

    while let Some(result) = stream.next().await {
        if let Err(e) = result {
            if require_existing_analysis {
                return Err(e);
            }
            if e.to_string() == "任务已取消" {
                return Err(e);
            }
            callback(ProcessEvent::Log(format!("处理章节时发生严重错误: {}", e)));
        }
    }

    // Post-collect new entries into allocation table
    if let Some(ref at) = allocation_table {
        let mut table = at.lock().unwrap();
        let va = voice_allocator.lock().unwrap();
        for (name, voice_id) in va.get_resolved_entries() {
            if table.lookup(name).is_none() {
                table.upsert(ai::VoiceAllocationEntry {
                    character_name: name.clone(),
                    aliases: vec![],
                    category: None,
                    category_label: None,
                    voice_id: voice_id.clone(),
                    source: ai::AllocationSource::AIInferred,
                    locked: true,
                    volume: None,
                    speed: None,
                    pitch: None,
                    confidence: None,
                    reason: None,
                    needs_review: false,
                });
            }
        }
        save_allocation_table_live(&table);
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
    callback(ProcessEvent::Success {
        size: total_size,
        output_path: book_dir.to_string_lossy().to_string(),
    });
    if require_existing_analysis && chapters_to_process < total_chapters {
        return Err(anyhow::anyhow!(
            "已合成到第 {} 章；后续章节需要先继续分析才能继续合成",
            chapters_to_process
        ));
    }
    callback(ProcessEvent::Log(format!("书籍处理完成: {:?}", book_dir)));
    Ok(())
}

pub async fn process_directory<F, W, PauseFut>(
    dir_path: &Path,
    args: &Cli,
    client: &ApiClient,
    blacklist: &Option<Regex>,
    callback: F,
    wait_if_paused: W,
) -> Result<()>
where
    F: Fn(ProcessEvent) + Send + Sync + 'static + Clone,
    W: Fn() -> PauseFut + Send + Sync + 'static + Clone,
    PauseFut: std::future::Future<Output = ()> + Send,
{
    let supported_extensions = ["txt", "epub"];
    let mut entries: Vec<PathBuf> = fs::read_dir(dir_path)?
        .filter_map(|res| res.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| supported_extensions.contains(&ext.to_lowercase().as_str()))
                    .unwrap_or(false)
        })
        .collect();

    entries.sort();

    if entries.is_empty() {
        callback(ProcessEvent::Log(format!(
            "在目录 {:?} 中没有找到支持的文件 (txt, epub)。",
            dir_path
        )));
        return Ok(());
    }

    callback(ProcessEvent::Log(format!(
        "找到 {} 个文件，准备处理...",
        entries.len()
    )));

    let txt_files: Vec<PathBuf> = entries
        .iter()
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
        })
        .cloned()
        .collect();
    if !txt_files.is_empty() {
        callback(ProcessEvent::Log("正在扫描 TXT 文件编码...".to_string()));
        let files_to_convert = utils::pre_scan_for_encoding_issues(&txt_files)?;
        if !files_to_convert.is_empty() {
            callback(ProcessEvent::Log(format!(
                "警告：检测到 {} 个文件可能不是 UTF-8 编码，请在 CLI 模式下交互处理。",
                files_to_convert.len()
            )));
        } else {
            callback(ProcessEvent::Log(
                "所有 TXT 文件编码兼容，准备开始 TTS 任务。".to_string(),
            ));
        }
    }

    callback(ProcessEvent::Log(
        "--------------------------------------------------".to_string(),
    ));

    for entry in entries {
        if let Err(e) = process_file(
            &entry,
            args,
            client,
            blacklist,
            callback.clone(),
            || async { false },
            wait_if_paused.clone(),
            None,
        )
        .await
        {
            callback(ProcessEvent::Log(format!(
                "处理文件 {:?} 时出错: {}",
                entry, e
            )));
        }
        callback(ProcessEvent::Log(
            "--------------------------------------------------".to_string(),
        ));
    }

    callback(ProcessEvent::Log("所有文件处理完毕。".to_string()));
    Ok(())
}
