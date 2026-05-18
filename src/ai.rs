use anyhow::{Context, Result, anyhow};
use once_cell::sync::Lazy;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep};

static AI_REQUEST_GAP: Lazy<Mutex<Instant>> = Lazy::new(|| Mutex::new(Instant::now()));
static AI_USAGE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
const AI_MIN_REQUEST_GAP: Duration = Duration::from_millis(1500);
const AI_SERVER_ERROR_COOLDOWN: Duration = Duration::from_secs(12);

fn default_schema_version() -> u32 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AiUsageStats {
    #[serde(default)]
    pub total_requests: u64,
    #[serde(default)]
    pub successful_requests: u64,
    #[serde(default)]
    pub failed_requests: u64,
    #[serde(default)]
    pub rate_limited_requests: u64,
    #[serde(default)]
    pub retry_attempts: u64,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub chapter_requests: u64,
    #[serde(default)]
    pub dialogue_requests: u64,
    #[serde(default)]
    pub started_at: u64,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub by_model: HashMap<String, AiUsageModelStats>,
    #[serde(default)]
    pub by_novel: HashMap<String, AiUsageNovelStats>,
    #[serde(default)]
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AiUsageModelStats {
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub successful_requests: u64,
    #[serde(default)]
    pub failed_requests: u64,
    #[serde(default)]
    pub rate_limited_requests: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AiUsageNovelStats {
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub chapter_requests: u64,
    #[serde(default)]
    pub dialogue_requests: u64,
    #[serde(default)]
    pub successful_requests: u64,
    #[serde(default)]
    pub failed_requests: u64,
    #[serde(default)]
    pub rate_limited_requests: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub estimated_tokens: u64,
    #[serde(default)]
    pub chapters_analyzed: u64,
    #[serde(default)]
    pub updated_at: u64,
}

#[derive(Debug, Clone, Copy)]
enum AiUsageKind {
    Chapter,
    Dialogue,
}

/// 10 fixed gender+age voice categories.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum VoiceCategory {
    #[serde(rename = "男童")]
    MaleChild,
    #[serde(rename = "女童")]
    FemaleChild,
    #[serde(rename = "少男")]
    MaleTeen,
    #[serde(rename = "少女")]
    FemaleTeen,
    #[serde(rename = "男青年")]
    MaleYouth,
    #[serde(rename = "女青年")]
    FemaleYouth,
    #[serde(rename = "男中年")]
    MaleMiddleAge,
    #[serde(rename = "女中年")]
    FemaleMiddleAge,
    #[serde(rename = "男老年")]
    MaleElder,
    #[serde(rename = "女老年")]
    FemaleElder,
}

impl VoiceCategory {
    /// All 10 categories in display order.
    pub fn all() -> [VoiceCategory; 10] {
        use VoiceCategory::*;
        [
            MaleChild,
            FemaleChild,
            MaleTeen,
            FemaleTeen,
            MaleYouth,
            FemaleYouth,
            MaleMiddleAge,
            FemaleMiddleAge,
            MaleElder,
            FemaleElder,
        ]
    }

    /// Human-readable Chinese label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::MaleChild => "男童",
            Self::FemaleChild => "女童",
            Self::MaleTeen => "少男",
            Self::FemaleTeen => "少女",
            Self::MaleYouth => "男青年",
            Self::FemaleYouth => "女青年",
            Self::MaleMiddleAge => "男中年",
            Self::FemaleMiddleAge => "女中年",
            Self::MaleElder => "男老年",
            Self::FemaleElder => "女老年",
        }
    }

    /// Infer category from AI-returned gender + age strings.
    pub fn infer(gender: Option<&str>, age: Option<&str>) -> Option<VoiceCategory> {
        let g = gender?;
        let a = age.unwrap_or("青年");
        match g {
            "男" => match a {
                "儿童" | "男童" => Some(Self::MaleChild),
                "少年" | "少男" => Some(Self::MaleTeen),
                "青年" => Some(Self::MaleYouth),
                "中年" => Some(Self::MaleMiddleAge),
                "老年" => Some(Self::MaleElder),
                _ => None,
            },
            "女" => match a {
                "儿童" | "女童" => Some(Self::FemaleChild),
                "少年" | "少女" => Some(Self::FemaleTeen),
                "青年" => Some(Self::FemaleYouth),
                "中年" => Some(Self::FemaleMiddleAge),
                "老年" => Some(Self::FemaleElder),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Per-category voice list, persisted in ai_dialogue_config.json.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VoicePool {
    #[serde(default)]
    pub entries: HashMap<VoiceCategory, Vec<String>>,
}

impl VoicePool {
    pub fn get(&self, category: &VoiceCategory) -> &[String] {
        self.entries
            .get(category)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}

/// Session-level round-robin allocator — resets per book, not persisted.
#[derive(Debug, Clone, Default)]
pub struct VoiceAllocator {
    next_index: HashMap<VoiceCategory, usize>,
    resolved: HashMap<String, String>,
    used_by_category: HashMap<VoiceCategory, Vec<String>>,
}

impl VoiceAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pick the next voice from the pool for a character in a category.
    /// Same character always gets the same voice within one session.
    /// Returns None when the pool for this category is empty.
    pub fn allocate(
        &mut self,
        pool: &VoicePool,
        category: &VoiceCategory,
        character_name: &str,
    ) -> Option<String> {
        if let Some(resolved) = self.resolved.get(character_name) {
            return Some(resolved.clone());
        }
        let voices = pool.get(category);
        if voices.is_empty() {
            return None;
        }
        let mut idx = self.next_index.get(category).copied().unwrap_or(0);
        let used = self.used_by_category.entry(category.clone()).or_default();
        let voice_id = if used.len() < voices.len() {
            let mut selected = voices[idx % voices.len()].clone();
            for offset in 0..voices.len() {
                let candidate = voices[(idx + offset) % voices.len()].clone();
                if !used.iter().any(|voice| voice == &candidate) {
                    selected = candidate;
                    idx += offset;
                    break;
                }
            }
            selected
        } else {
            voices[idx % voices.len()].clone()
        };
        self.next_index.insert(category.clone(), idx + 1);
        if !used.iter().any(|voice| voice == &voice_id) {
            used.push(voice_id.clone());
        }
        self.resolved
            .insert(character_name.to_string(), voice_id.clone());
        Some(voice_id)
    }

    /// Pre-seed a resolved entry (used to load pre-computed allocation tables).
    pub fn pre_seed(&mut self, character_name: &str, voice_id: &str) {
        self.resolved
            .insert(character_name.to_string(), voice_id.to_string());
    }

    pub fn pre_seed_category(
        &mut self,
        category: &VoiceCategory,
        character_name: &str,
        voice_id: &str,
    ) {
        self.pre_seed(character_name, voice_id);
        let used = self.used_by_category.entry(category.clone()).or_default();
        if !used.iter().any(|voice| voice == voice_id) {
            used.push(voice_id.to_string());
        }
    }

    pub fn get_resolved_entries(&self) -> impl Iterator<Item = (&String, &String)> {
        self.resolved.iter()
    }
}

/// How a voice was assigned to a character.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AllocationSource {
    #[serde(rename = "character_override")]
    CharacterOverride,
    #[serde(rename = "character_category")]
    CharacterCategory,
    #[serde(rename = "ai_inferred")]
    AIInferred,
    #[serde(rename = "manual")]
    Manual,
}

/// A single character→voice assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceAllocationEntry {
    pub character_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub category: Option<VoiceCategory>,
    pub category_label: Option<String>,
    #[serde(default)]
    pub voice_id: String,
    pub source: AllocationSource,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub volume: Option<u8>,
    #[serde(default)]
    pub speed: Option<u8>,
    #[serde(default)]
    pub pitch: Option<u8>,
    #[serde(default)]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub needs_review: bool,
}

/// Per-novel voice allocation table, persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VoiceAllocationTable {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub file_path: String,
    pub novel_title: String,
    pub entries: Vec<VoiceAllocationEntry>,
    pub generated_at: u64,
}

impl VoiceAllocationTable {
    pub fn normalize_legacy_text_fields(&mut self) {
        for entry in &mut self.entries {
            entry.character_name = normalize_ai_text(&entry.character_name);
            entry.aliases = entry
                .aliases
                .iter()
                .map(|alias| normalize_ai_text(alias))
                .filter(|alias| !alias.trim().is_empty() && !is_suspicious_ai_name(alias))
                .collect();
            entry.category_label = entry
                .category_label
                .as_ref()
                .map(|label| normalize_ai_text(label))
                .filter(|label| !label.trim().is_empty());
            entry.reason = entry
                .reason
                .as_ref()
                .map(|reason| normalize_ai_text(reason))
                .filter(|reason| !reason.trim().is_empty());
        }
        self.entries
            .retain(|entry| !is_suspicious_ai_name(&entry.character_name));
    }

    pub fn lookup(&self, character_name: &str) -> Option<&VoiceAllocationEntry> {
        self.entries
            .iter()
            .find(|e| e.character_name == character_name)
    }

    pub fn lookup_match(&self, character_name: &str) -> Option<&VoiceAllocationEntry> {
        let target = normalize_name(character_name);
        if target.is_empty() {
            return None;
        }

        self.entries.iter().find(|entry| {
            name_parts(&entry.character_name)
                .iter()
                .any(|name| normalize_name(name) == target)
                || entry
                    .aliases
                    .iter()
                    .flat_map(|alias| name_parts(alias))
                    .any(|alias| normalize_name(&alias) == target)
        })
    }

    pub fn merge_alias_for_match(&mut self, character_name: &str) -> Option<Vec<String>> {
        let target = normalize_name(character_name);
        if target.is_empty() {
            return None;
        }

        let entry = self.entries.iter_mut().find(|entry| {
            name_parts(&entry.character_name)
                .iter()
                .any(|name| normalize_name(name) == target)
                || entry
                    .aliases
                    .iter()
                    .flat_map(|alias| name_parts(alias))
                    .any(|alias| normalize_name(&alias) == target)
        })?;

        let speaker_names = name_parts(character_name);
        let existing: Vec<String> = name_parts(&entry.character_name)
            .into_iter()
            .chain(entry.aliases.iter().flat_map(|alias| name_parts(alias)))
            .map(|name| normalize_name(&name))
            .filter(|name| !name.is_empty())
            .collect();

        for name in speaker_names {
            let normalized = normalize_name(&name);
            if normalized.is_empty() || existing.iter().any(|existing| existing == &normalized) {
                continue;
            }
            entry.aliases.push(name);
        }

        Some(entry.aliases.clone())
    }

    pub fn upsert(&mut self, entry: VoiceAllocationEntry) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.character_name == entry.character_name)
        {
            let mut entry = entry;
            if entry.volume.is_none() {
                entry.volume = existing.volume;
            }
            if entry.speed.is_none() {
                entry.speed = existing.speed;
            }
            if entry.pitch.is_none() {
                entry.pitch = existing.pitch;
            }
            if entry.confidence.is_none() {
                entry.confidence = existing.confidence;
            }
            if entry.reason.is_none() {
                entry.reason = existing.reason.clone();
            }
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }

    pub fn upsert_ai_result(&mut self, entry: VoiceAllocationEntry) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.character_name == entry.character_name)
        {
            if existing.locked {
                return;
            }
            if !existing.voice_id.is_empty() {
                for alias in entry.aliases {
                    let normalized = normalize_name(&alias);
                    if !normalized.is_empty()
                        && !existing
                            .aliases
                            .iter()
                            .any(|existing_alias| normalize_name(existing_alias) == normalized)
                    {
                        existing.aliases.push(alias);
                    }
                }
                existing.locked = true;
                if existing.confidence.is_none() {
                    existing.confidence = entry.confidence;
                }
                if existing.reason.is_none() {
                    existing.reason = entry.reason;
                }
                existing.needs_review = existing.needs_review || entry.needs_review;
                return;
            }
            let mut entry = entry;
            if entry.volume.is_none() {
                entry.volume = existing.volume;
            }
            if entry.speed.is_none() {
                entry.speed = existing.speed;
            }
            if entry.pitch.is_none() {
                entry.pitch = existing.pitch;
            }
            if entry.confidence.is_none() {
                entry.confidence = existing.confidence;
            }
            if entry.reason.is_none() {
                entry.reason = existing.reason.clone();
            }
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }
}

/// Generate a per-novel allocation table from voice pool + character config.
/// Locked entries from an existing table survive regeneration.
pub fn generate_allocation_table(
    config: &AiDialogueConfig,
    file_path: &str,
    novel_title: &str,
    existing: Option<&VoiceAllocationTable>,
) -> VoiceAllocationTable {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Collect locked entries from existing table
    let locked: HashMap<String, VoiceAllocationEntry> = existing
        .map(|t| {
            t.entries
                .iter()
                .filter(|e| e.locked)
                .map(|e| (e.character_name.clone(), e.clone()))
                .collect()
        })
        .unwrap_or_default();

    let mut allocator = VoiceAllocator::new();
    // Pre-seed locked entries so round-robin skips their slots
    for entry in locked.values() {
        if let Some(ref category) = entry.category {
            allocator.pre_seed_category(category, &entry.character_name, &entry.voice_id);
        } else {
            allocator.pre_seed(&entry.character_name, &entry.voice_id);
        }
    }

    let mut entries: Vec<VoiceAllocationEntry> = Vec::new();

    for character in &config.characters {
        if !character.enabled {
            continue;
        }
        // If a locked entry exists, use it
        if let Some(locked_entry) = locked.get(&character.name) {
            entries.push(locked_entry.clone());
            continue;
        }
        // Voice ID override
        if let Some(ref voice_id) = character.voice_id {
            if !voice_id.is_empty() {
                entries.push(VoiceAllocationEntry {
                    character_name: character.name.clone(),
                    aliases: character.aliases.clone(),
                    category: character.category.clone(),
                    category_label: character.category.as_ref().map(|c| c.label().to_string()),
                    voice_id: voice_id.clone(),
                    source: AllocationSource::CharacterOverride,
                    locked: false,
                    volume: existing
                        .and_then(|table| table.lookup_match(&character.name))
                        .and_then(|entry| entry.volume),
                    speed: existing
                        .and_then(|table| table.lookup_match(&character.name))
                        .and_then(|entry| entry.speed),
                    pitch: existing
                        .and_then(|table| table.lookup_match(&character.name))
                        .and_then(|entry| entry.pitch),
                    confidence: None,
                    reason: None,
                    needs_review: false,
                });
                continue;
            }
        }
        // Category-based round-robin
        if let Some(ref category) = character.category {
            if let Some(voice_id) =
                allocator.allocate(&config.voice_pool, category, &character.name)
            {
                entries.push(VoiceAllocationEntry {
                    character_name: character.name.clone(),
                    aliases: character.aliases.clone(),
                    category: Some(category.clone()),
                    category_label: Some(category.label().to_string()),
                    voice_id,
                    source: AllocationSource::CharacterCategory,
                    locked: false,
                    volume: existing
                        .and_then(|table| table.lookup_match(&character.name))
                        .and_then(|entry| entry.volume),
                    speed: existing
                        .and_then(|table| table.lookup_match(&character.name))
                        .and_then(|entry| entry.speed),
                    pitch: existing
                        .and_then(|table| table.lookup_match(&character.name))
                        .and_then(|entry| entry.pitch),
                    confidence: None,
                    reason: None,
                    needs_review: false,
                });
            }
        }
    }

    // Carry over locked entries NOT in character table (AI-inferred from previous run)
    for entry in locked.values() {
        if !entries
            .iter()
            .any(|e| e.character_name == entry.character_name)
        {
            entries.push(entry.clone());
        }
    }

    VoiceAllocationTable {
        schema_version: default_schema_version(),
        file_path: file_path.to_string(),
        novel_title: novel_title.to_string(),
        entries,
        generated_at: now,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharacterVoice {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub gender: Option<String>,
    #[serde(default)]
    pub age: Option<String>,
    /// Which of the 10 fixed categories this character belongs to.
    #[serde(default)]
    pub category: Option<VoiceCategory>,
    /// Explicit voice override — when set, bypasses category pool allocation.
    #[serde(default)]
    pub voice_id: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiDialogueConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub api_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_top_p")]
    pub top_p: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_context_chars")]
    pub context_chars: usize,
    #[serde(default)]
    pub characters: Vec<CharacterVoice>,
    #[serde(default)]
    pub voice_pool: VoicePool,
    #[serde(default)]
    pub save_crowd_characters: bool,
    #[serde(default = "default_chapter_analysis_enabled")]
    pub chapter_analysis_enabled: bool,
    #[serde(default = "default_rate_limit_cooldown_secs")]
    pub rate_limit_cooldown_secs: u64,
}

impl Default for AiDialogueConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_url: None,
            api_key: None,
            model: None,
            temperature: default_temperature(),
            top_p: default_top_p(),
            max_tokens: default_max_tokens(),
            context_chars: default_context_chars(),
            characters: Vec::new(),
            voice_pool: VoicePool::default(),
            save_crowd_characters: false,
            chapter_analysis_enabled: default_chapter_analysis_enabled(),
            rate_limit_cooldown_secs: default_rate_limit_cooldown_secs(),
        }
    }
}

/// Resolve which voice to use for a matched character.
///
/// Priority (highest first):
///   1. `character.voice_id` explicit override
///   2. `character.category` → round-robin from `voice_pool`
///   3. Infer category from AI speaker gender+age → round-robin from voice_pool
///   4. None (caller falls back to `voice_dialogue`)
pub fn resolve_character_voice(
    config: &AiDialogueConfig,
    allocator: &mut VoiceAllocator,
    character: &CharacterVoice,
    speaker_gender: Option<&str>,
    speaker_age: Option<&str>,
) -> Option<String> {
    if let Some(ref voice_id) = character.voice_id {
        if !voice_id.is_empty() {
            return Some(voice_id.clone());
        }
    }
    if let Some(ref category) = character.category {
        if let Some(voice_id) = allocator.allocate(&config.voice_pool, category, &character.name) {
            return Some(voice_id);
        }
    }
    // Infer category from AI speaker gender+age
    resolve_speaker_voice(
        config,
        allocator,
        &character.name,
        speaker_gender,
        speaker_age,
    )
}

/// Auto-assign a voice from the voice pool based on speaker gender+age (no character table needed).
pub fn resolve_speaker_voice(
    config: &AiDialogueConfig,
    allocator: &mut VoiceAllocator,
    speaker_name: &str,
    speaker_gender: Option<&str>,
    speaker_age: Option<&str>,
) -> Option<String> {
    let category = VoiceCategory::infer(speaker_gender, speaker_age)?;
    allocator.allocate(&config.voice_pool, &category, speaker_name)
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    reasoning_content: Option<String>,
}

impl ChatMessage {
    fn effective_content(&self) -> &str {
        if !self.content.trim().is_empty() {
            &self.content
        } else if let Some(ref rc) = self.reasoning_content {
            rc.as_str()
        } else {
            ""
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpeakerResult {
    pub name: String,
    #[serde(default)]
    pub gender: Option<String>,
    #[serde(default)]
    pub age: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_confidence")]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChapterDialogueInput {
    pub index: usize,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChapterDialogueSpeaker {
    pub index: usize,
    pub name: String,
    #[serde(default)]
    pub gender: Option<String>,
    #[serde(default)]
    pub age: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_confidence")]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl ChapterDialogueSpeaker {
    pub fn into_speaker_result(self) -> SpeakerResult {
        let mut result = SpeakerResult {
            name: self.name,
            gender: self.gender,
            age: self.age,
            confidence: self.confidence,
            reason: self.reason,
        };
        normalize_speaker_result(&mut result);
        result
    }
}

fn deserialize_optional_confidence<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<f32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        serde_json::Value::Number(number) => Ok(number.as_f64().map(|v| v as f32)),
        serde_json::Value::String(text) => Ok(text.trim().parse::<f32>().ok()),
        _ => Ok(None),
    }
}

fn default_enabled() -> bool {
    true
}

fn default_temperature() -> f32 {
    0.05
}

fn default_top_p() -> f32 {
    0.3
}

fn default_max_tokens() -> u32 {
    120
}

fn default_context_chars() -> usize {
    1800
}

fn default_chapter_analysis_enabled() -> bool {
    true
}

fn default_rate_limit_cooldown_secs() -> u64 {
    60
}

pub fn should_use_ai(config: &AiDialogueConfig) -> bool {
    check_ai_config(config).is_ok()
}

pub fn check_ai_config(config: &AiDialogueConfig) -> Result<(), &'static str> {
    if !config.enabled {
        return Err("AI 对话分配未启用");
    }
    if config
        .api_url
        .as_ref()
        .map_or(true, |v| v.trim().is_empty())
    {
        return Err("模型 API 地址未填写");
    }
    if config
        .api_key
        .as_ref()
        .map_or(true, |v| v.trim().is_empty())
    {
        return Err("API 密钥未填写");
    }
    if config.model.as_ref().map_or(true, |v| v.trim().is_empty()) {
        return Err("模型名未填写");
    }
    Ok(())
}

pub async fn identify_speaker(
    config: &AiDialogueConfig,
    dialogue: &str,
    chapter_context: &str,
    known_characters: &[CharacterVoice],
    novel_title: Option<&str>,
) -> Result<SpeakerResult> {
    let api_url = config
        .api_url
        .as_deref()
        .ok_or_else(|| anyhow!("AI API URL is empty"))?;
    let api_key = pick_api_key(config.api_key.as_deref().unwrap_or_default())
        .ok_or_else(|| anyhow!("AI API key is empty"))?;
    let model = config
        .model
        .as_deref()
        .ok_or_else(|| anyhow!("AI model is empty"))?;

    let prompt = build_prompt(dialogue, chapter_context, known_characters);
    let body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "You are a JSON-only speaker identification API. Output ONLY the JSON object, no thinking, no markdown, no explanation."
            },
            {
                "role": "user",
                "content": prompt
            }
        ],
        "temperature": config.temperature,
        "top_p": config.top_p,
        "max_tokens": config.max_tokens,
        "thinking": {"type": "disabled"}
    });

    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("failed to create AI HTTP client")?;

    let parsed = send_chat_completion(
        &client,
        api_url,
        api_key,
        model,
        &body,
        "AI request",
        AiUsageKind::Dialogue,
        novel_title,
        Duration::from_secs(config.rate_limit_cooldown_secs.clamp(5, 600)),
    )
    .await?;

    let content = parsed
        .choices
        .first()
        .map(|choice| choice.message.effective_content().trim())
        .filter(|content| !content.is_empty())
        .ok_or_else(|| anyhow!("AI response content is empty"))?;

    parse_speaker_json(content)
}

pub async fn identify_chapter_speakers(
    config: &AiDialogueConfig,
    chapter_title: &str,
    chapter_content: &str,
    dialogues: &[ChapterDialogueInput],
    known_characters: &[CharacterVoice],
    novel_title: Option<&str>,
) -> Result<Vec<ChapterDialogueSpeaker>> {
    if dialogues.is_empty() {
        return Ok(Vec::new());
    }

    let api_url = config
        .api_url
        .as_deref()
        .ok_or_else(|| anyhow!("AI API URL is empty"))?;
    let api_key = pick_api_key(config.api_key.as_deref().unwrap_or_default())
        .ok_or_else(|| anyhow!("AI API key is empty"))?;
    let model = config
        .model
        .as_deref()
        .ok_or_else(|| anyhow!("AI model is empty"))?;

    let prompt = build_chapter_prompt(chapter_title, chapter_content, dialogues, known_characters);
    let max_tokens = config.max_tokens.max((dialogues.len() as u32 * 70) + 300);
    let body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "You are a JSON-only novel dialogue speaker identification API. Output ONLY the JSON object, no thinking, no markdown, no explanation."
            },
            {
                "role": "user",
                "content": prompt
            }
        ],
        "temperature": config.temperature,
        "top_p": config.top_p,
        "max_tokens": max_tokens,
        "thinking": {"type": "disabled"}
    });

    let client = Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
        .context("failed to create AI HTTP client")?;

    let parsed = send_chat_completion(
        &client,
        api_url,
        api_key,
        model,
        &body,
        "AI chapter request",
        AiUsageKind::Chapter,
        novel_title,
        Duration::from_secs(config.rate_limit_cooldown_secs.clamp(5, 600)),
    )
    .await?;

    let content = parsed
        .choices
        .first()
        .map(|choice| choice.message.effective_content().trim())
        .filter(|content| !content.is_empty())
        .ok_or_else(|| anyhow!("AI chapter response content is empty"))?;

    parse_chapter_speakers_json(content)
}

async fn wait_ai_turn() {
    let mut next_allowed = AI_REQUEST_GAP.lock().await;
    let now = Instant::now();
    if *next_allowed > now {
        sleep(*next_allowed - now).await;
    }
    *next_allowed = Instant::now() + AI_MIN_REQUEST_GAP;
}

async fn apply_ai_cooldown(delay: Duration) {
    let mut next_allowed = AI_REQUEST_GAP.lock().await;
    let cooldown_until = Instant::now() + delay;
    if *next_allowed < cooldown_until {
        *next_allowed = cooldown_until;
    }
}

async fn send_chat_completion(
    client: &Client,
    api_url: &str,
    api_key: &str,
    model: &str,
    body: &serde_json::Value,
    label: &str,
    kind: AiUsageKind,
    novel_title: Option<&str>,
    rate_limit_cooldown: Duration,
) -> Result<ChatCompletionResponse> {
    let retry_delays = [
        Duration::from_secs(3),
        Duration::from_secs(8),
        Duration::from_secs(15),
    ];
    let mut last_error = None;

    for attempt in 0..=retry_delays.len() {
        wait_ai_turn().await;
        let response = match client
            .post(api_url)
            .bearer_auth(api_key)
            .json(body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                record_ai_usage(
                    model,
                    kind,
                    false,
                    false,
                    attempt > 0,
                    novel_title,
                    None,
                    false,
                )
                .await;
                return Err(e).with_context(|| format!("{} failed", label));
            }
        };

        let status = response.status();
        if status.is_success() {
            let parsed = match response.json::<ChatCompletionResponse>().await {
                Ok(parsed) => parsed,
                Err(e) => {
                    record_ai_usage(
                        model,
                        kind,
                        false,
                        false,
                        attempt > 0,
                        novel_title,
                        None,
                        false,
                    )
                    .await;
                    return Err(e).with_context(|| format!("failed to parse {} response", label));
                }
            };
            let estimated_usage = if parsed.usage.is_none() {
                Some(estimate_chat_usage(body, &parsed))
            } else {
                None
            };
            let usage = parsed.usage.as_ref().or(estimated_usage.as_ref());
            record_ai_usage(
                model,
                kind,
                true,
                false,
                attempt > 0,
                novel_title,
                usage,
                estimated_usage.is_some(),
            )
            .await;
            return Ok(parsed);
        }

        let body_text = response.text().await.unwrap_or_default();
        let message = format!(
            "{} failed with status {}: {}",
            label,
            status,
            body_text.chars().take(300).collect::<String>()
        );

        if status.as_u16() == 429 || status.is_server_error() {
            let body_lower = body_text.to_ascii_lowercase();
            let is_rate_limit = status.as_u16() == 429
                || body_lower.contains("rpm exhausted")
                || body_lower.contains("quota_exceeded")
                || body_lower.contains("rate limit");
            apply_ai_cooldown(if is_rate_limit {
                rate_limit_cooldown
            } else {
                AI_SERVER_ERROR_COOLDOWN
            })
            .await;
            record_ai_usage(
                model,
                kind,
                false,
                is_rate_limit,
                attempt > 0,
                novel_title,
                None,
                false,
            )
            .await;
            last_error = Some(message.clone());
            if let Some(delay) = retry_delays.get(attempt) {
                sleep(*delay).await;
                continue;
            }
        }

        record_ai_usage(
            model,
            kind,
            false,
            false,
            attempt > 0,
            novel_title,
            None,
            false,
        )
        .await;
        return Err(anyhow!(message));
    }

    Err(anyhow!(
        "{} failed after retries: {}",
        label,
        last_error.unwrap_or_else(|| "unknown error".to_string())
    ))
}

fn estimate_chat_usage(body: &serde_json::Value, response: &ChatCompletionResponse) -> ChatUsage {
    let prompt_chars = body
        .get("messages")
        .and_then(|messages| messages.as_array())
        .map(|messages| {
            messages
                .iter()
                .filter_map(|message| message.get("content").and_then(|content| content.as_str()))
                .map(|content| content.chars().count() as u64)
                .sum::<u64>()
        })
        .unwrap_or(0);
    let completion_chars = response
        .choices
        .iter()
        .map(|choice| choice.message.effective_content().chars().count() as u64)
        .sum::<u64>();

    let prompt_tokens = estimate_tokens_from_chars(prompt_chars);
    let completion_tokens = estimate_tokens_from_chars(completion_chars);
    ChatUsage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
    }
}

fn estimate_tokens_from_chars(chars: u64) -> u64 {
    if chars == 0 {
        0
    } else {
        ((chars as f64) / 1.8).ceil() as u64
    }
}

fn ai_usage_path() -> PathBuf {
    let data_dir = if PathBuf::from("/data").exists() {
        PathBuf::from("/data")
    } else {
        PathBuf::from("data")
    };
    data_dir.join("ai_usage_stats.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn load_ai_usage_stats() -> AiUsageStats {
    let path = ai_usage_path();
    let Ok(content) = std::fs::read_to_string(path) else {
        return AiUsageStats::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_ai_usage_stats(stats: &AiUsageStats) {
    let path = ai_usage_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string_pretty(stats) {
        let _ = std::fs::write(path, content);
    }
}

pub async fn delete_ai_usage_novel(novel_title: &str) -> bool {
    let novel_title = novel_title.trim();
    if novel_title.is_empty() {
        return false;
    }

    let _guard = AI_USAGE_LOCK.lock().await;
    let mut stats = load_ai_usage_stats();
    let removed = stats.by_novel.remove(novel_title).is_some();
    if removed {
        stats.updated_at = now_secs();
        save_ai_usage_stats(&stats);
    }
    removed
}

async fn record_ai_usage(
    model: &str,
    kind: AiUsageKind,
    success: bool,
    rate_limited: bool,
    retry_attempt: bool,
    novel_title: Option<&str>,
    usage: Option<&ChatUsage>,
    usage_estimated: bool,
) {
    let _guard = AI_USAGE_LOCK.lock().await;
    let mut stats = load_ai_usage_stats();
    let now = now_secs();
    if stats.started_at == 0 {
        stats.started_at = now;
    }
    stats.updated_at = now;
    stats.total_requests += 1;
    match kind {
        AiUsageKind::Chapter => stats.chapter_requests += 1,
        AiUsageKind::Dialogue => stats.dialogue_requests += 1,
    }

    if retry_attempt {
        stats.retry_attempts += 1;
    }
    if rate_limited {
        stats.rate_limited_requests += 1;
    } else if success {
        stats.successful_requests += 1;
    } else {
        stats.failed_requests += 1;
    }
    if let Some(usage) = usage {
        stats.prompt_tokens += usage.prompt_tokens;
        stats.completion_tokens += usage.completion_tokens;
        stats.total_tokens += usage.total_tokens;
        if usage_estimated {
            stats.estimated_tokens += usage.total_tokens;
        }
    }

    let model_key = if model.trim().is_empty() {
        "unknown".to_string()
    } else {
        model.trim().to_string()
    };
    let model_stats = stats.by_model.entry(model_key).or_default();
    model_stats.requests += 1;
    if success {
        model_stats.successful_requests += 1;
    } else if rate_limited {
        model_stats.rate_limited_requests += 1;
    } else {
        model_stats.failed_requests += 1;
    }
    if let Some(usage) = usage {
        model_stats.total_tokens += usage.total_tokens;
        if usage_estimated {
            model_stats.estimated_tokens += usage.total_tokens;
        }
    }

    if let Some(novel_title) = novel_title.map(str::trim).filter(|value| !value.is_empty()) {
        let novel_stats = stats.by_novel.entry(novel_title.to_string()).or_default();
        novel_stats.requests += 1;
        novel_stats.updated_at = now;
        match kind {
            AiUsageKind::Chapter => novel_stats.chapter_requests += 1,
            AiUsageKind::Dialogue => novel_stats.dialogue_requests += 1,
        }
        if success {
            novel_stats.successful_requests += 1;
        } else if rate_limited {
            novel_stats.rate_limited_requests += 1;
        } else {
            novel_stats.failed_requests += 1;
        }
        if let Some(usage) = usage {
            novel_stats.total_tokens += usage.total_tokens;
            if usage_estimated {
                novel_stats.estimated_tokens += usage.total_tokens;
            }
        }
    }

    save_ai_usage_stats(&stats);
}

pub async fn record_ai_chapter_analyzed(novel_title: &str) {
    let novel_title = novel_title.trim();
    if novel_title.is_empty() {
        return;
    }
    let _guard = AI_USAGE_LOCK.lock().await;
    let mut stats = load_ai_usage_stats();
    let now = now_secs();
    if stats.started_at == 0 {
        stats.started_at = now;
    }
    stats.updated_at = now;
    let novel_stats = stats.by_novel.entry(novel_title.to_string()).or_default();
    novel_stats.chapters_analyzed += 1;
    novel_stats.updated_at = now;
    save_ai_usage_stats(&stats);
}

pub fn match_character<'a>(
    characters: &'a [CharacterVoice],
    speaker_name: &str,
) -> Option<&'a CharacterVoice> {
    let target = normalize_name(speaker_name);
    if target.is_empty() {
        return None;
    }

    characters.iter().find(|character| {
        character.enabled
            && (name_parts(&character.name)
                .iter()
                .any(|name| normalize_name(name) == target)
                || character
                    .aliases
                    .iter()
                    .flat_map(|alias| name_parts(alias))
                    .any(|alias| normalize_name(&alias) == target))
    })
}

/// Ask the LLM to suggest voice assignments for the 10 categories from a voice list.
pub async fn suggest_voice_pool(
    config: &AiDialogueConfig,
    voices: &[crate::api::Voice],
) -> Result<VoicePool> {
    let api_url = config
        .api_url
        .as_deref()
        .ok_or_else(|| anyhow!("AI API URL is empty"))?;
    let api_key = pick_api_key(config.api_key.as_deref().unwrap_or_default())
        .ok_or_else(|| anyhow!("AI API key is empty"))?;
    let model = config
        .model
        .as_deref()
        .ok_or_else(|| anyhow!("AI model is empty"))?;

    let voice_list: Vec<serde_json::Value> = voices
        .iter()
        .map(|v| {
            serde_json::json!({"id": v.id, "name": v.name, "gender": v.gender, "locale": v.locale})
        })
        .collect();

    let existing_pool: serde_json::Value =
        serde_json::to_value(&config.voice_pool).unwrap_or_default();

    let prompt = format!(
        concat!(
            "Task: assign voice IDs to 10 speaker categories for Chinese audiobook production.\n\n",
            "The 10 categories are:\n",
            "- 男童 (male child)\n- 女童 (female child)\n",
            "- 少男 (male teen)\n- 少女 (female teen)\n",
            "- 男青年 (male young adult)\n- 女青年 (female young adult)\n",
            "- 男中年 (male middle-aged)\n- 女中年 (female middle-aged)\n",
            "- 男老年 (male elder)\n- 女老年 (female elder)\n\n",
            "Available voices (pick by matching gender and voice character to category):\n{}\n\n",
            "Current assignment (refine this, add/remove as needed):\n{}\n\n",
            "Rules:\n",
            "1. Each category should have 1-5 suitable voices.\n",
            "2. Match gender strictly.\n",
            "3. Pick voices whose name/timbre fits the age group.\n",
            "4. You may leave a category empty if no suitable voice exists.\n",
            "5. Return STRICT JSON only, no markdown, no explanation.\n\n",
            "Output schema:\n",
            "{{\"entries\":{{\"男童\":[\"voice_id1\"],\"女童\":[\"voice_id2\"],...}}}}"
        ),
        serde_json::to_string(&voice_list).unwrap_or_default(),
        serde_json::to_string(&existing_pool).unwrap_or_default(),
    );

    let body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a JSON-only API. Output the requested JSON object directly, no thinking, no markdown, no explanation."},
            {"role": "user", "content": prompt}
        ],
        "temperature": 0.1,
        "max_tokens": 50000,
        "thinking": {"type": "disabled"}
    });

    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("failed to create AI HTTP client")?;

    let response = client
        .post(api_url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .context("AI voice pool suggestion request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "AI suggestion request failed with status {}: {}",
            status,
            body.chars().take(500).collect::<String>()
        ));
    }

    let raw_text = response
        .text()
        .await
        .context("failed to read AI suggestion response")?;

    let parsed: ChatCompletionResponse = serde_json::from_str(&raw_text).with_context(|| {
        format!(
            "failed to parse AI suggestion response: {}",
            raw_text.chars().take(500).collect::<String>()
        )
    })?;

    let content = parsed
        .choices
        .first()
        .map(|c| c.message.effective_content().trim())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "AI suggestion response content is empty. Raw response: {}",
                raw_text.chars().take(500).collect::<String>()
            )
        })?;

    let json_text = extract_json_object(content).unwrap_or(content);
    let pool: VoicePool = serde_json::from_str(json_text).with_context(|| {
        format!(
            "AI suggestion JSON does not match voice pool schema. Parsed content: {}",
            json_text.chars().take(500).collect::<String>()
        )
    })?;

    Ok(pool)
}

fn build_prompt(dialogue: &str, chapter_context: &str, characters: &[CharacterVoice]) -> String {
    let known_characters: Vec<serde_json::Value> = characters
        .iter()
        .filter(|character| character.enabled)
        .map(|character| {
            serde_json::json!({
                "name": &character.name,
                "aliases": &character.aliases,
                "gender": &character.gender,
                "age": &character.age
            })
        })
        .collect();

    format!(
        concat!(
            "Task: identify who speaks the highlighted dialogue in the novel context.\n",
            "Rules:\n",
            "1. Do not treat names inside quotation marks as the speaker.\n",
            "2. Prefer an existing known character when context supports it.\n",
            "3. If the speaker is unknown, use a descriptive crowd name such as \"群众男青年\" or \"群众女青年\".\n",
            "4. Return JSON only, no markdown, no explanation.\n\n",
            "Output schema:\n",
            "{{\"name\":\"speaker name\",\"gender\":\"男 or 女 or null\",\"age\":\"儿童/少年/青年/中年/老年/女童/少女 or null\",\"confidence\":0.0-1.0,\"reason\":\"short reason\"}}\n\n",
            "Known characters:\n{}\n\n",
            "Highlighted dialogue:\n{}\n\n",
            "Context:\n{}"
        ),
        serde_json::to_string(&known_characters).unwrap_or_else(|_| "[]".to_string()),
        dialogue,
        chapter_context
    )
}

fn build_chapter_prompt(
    chapter_title: &str,
    chapter_content: &str,
    dialogues: &[ChapterDialogueInput],
    characters: &[CharacterVoice],
) -> String {
    let known_characters: Vec<serde_json::Value> = characters
        .iter()
        .filter(|character| character.enabled)
        .map(|character| {
            serde_json::json!({
                "name": &character.name,
                "aliases": &character.aliases,
                "gender": &character.gender,
                "age": &character.age
            })
        })
        .collect();
    let chapter_excerpt: String = chapter_content.chars().take(12000).collect();

    format!(
        concat!(
            "Task: identify the speaker for each numbered dialogue in this chapter.\n",
            "Rules:\n",
            "1. Use the whole chapter context, but return answers only for the numbered dialogues.\n",
            "2. Do not treat names inside quotation marks as the speaker.\n",
            "3. Prefer an existing known character when context supports it.\n",
            "4. If the speaker is unknown, use a descriptive crowd name such as \"群众男青年\" or \"群众女青年\".\n",
            "5. Return JSON only, no markdown, no explanation.\n\n",
            "Output schema:\n",
            "{{\"dialogues\":[{{\"index\":0,\"name\":\"speaker name\",\"gender\":\"男 or 女 or null\",\"age\":\"儿童/少年/青年/中年/老年/女童/少女 or null\",\"confidence\":0.0-1.0}}]}}\n\n",
            "Known characters:\n{}\n\n",
            "Chapter title:\n{}\n\n",
            "Chapter content:\n{}\n\n",
            "Numbered dialogues:\n{}"
        ),
        serde_json::to_string(&known_characters).unwrap_or_else(|_| "[]".to_string()),
        chapter_title,
        chapter_excerpt,
        serde_json::to_string(dialogues).unwrap_or_else(|_| "[]".to_string())
    )
}

fn parse_speaker_json(content: &str) -> Result<SpeakerResult> {
    let json_text = extract_json_object(content).unwrap_or(content);
    let value: serde_json::Value = serde_json::from_str(json_text).with_context(|| {
        format!(
            "AI response is not valid JSON. Raw: {}",
            content.chars().take(300).collect::<String>()
        )
    })?;

    if let Some(speaker_info) = value.get("speaker_info") {
        let mut result: SpeakerResult = serde_json::from_value(speaker_info.clone())
            .context("AI speaker_info JSON does not match schema")?;
        normalize_speaker_result(&mut result);
        return Ok(result);
    }

    let mut result: SpeakerResult =
        serde_json::from_value(value).context("AI JSON does not match speaker schema")?;
    normalize_speaker_result(&mut result);
    Ok(result)
}

fn parse_chapter_speakers_json(content: &str) -> Result<Vec<ChapterDialogueSpeaker>> {
    let json_text = extract_json_object(content).unwrap_or(content);
    let value: serde_json::Value = serde_json::from_str(json_text).with_context(|| {
        format!(
            "AI chapter response is not valid JSON. Raw: {}",
            content.chars().take(300).collect::<String>()
        )
    })?;

    let items = if let Some(dialogues) = value.get("dialogues") {
        dialogues.clone()
    } else if let Some(assignments) = value.get("assignments") {
        assignments.clone()
    } else if let Some(items) = value.get("items") {
        items.clone()
    } else if value.is_array() {
        value
    } else {
        return Err(anyhow!("AI chapter JSON does not contain dialogues array"));
    };

    let results: Vec<ChapterDialogueSpeaker> =
        serde_json::from_value(items).context("AI chapter dialogues JSON does not match schema")?;
    Ok(results
        .into_iter()
        .filter(|result| !result.name.trim().is_empty())
        .collect())
}

fn normalize_speaker_result(result: &mut SpeakerResult) {
    result.name = normalize_ai_text(&result.name).trim().to_string();
    result.gender = result
        .gender
        .as_ref()
        .map(|value| normalize_ai_text(value).trim().to_string())
        .filter(|value| !value.is_empty());
    result.age = result
        .age
        .as_ref()
        .map(|value| normalize_ai_text(value).trim().to_string())
        .filter(|value| !value.is_empty());
    result.reason = result
        .reason
        .as_ref()
        .map(|value| normalize_ai_text(value).trim().to_string())
        .filter(|value| !value.is_empty());
    result.confidence = result.confidence.map(|value| {
        if value > 1.0 && value <= 100.0 {
            (value / 100.0).clamp(0.0, 1.0)
        } else {
            value.clamp(0.0, 1.0)
        }
    });
}

pub fn normalize_ai_text(value: &str) -> String {
    match value.trim() {
        "鐢?" | "鐢�" | "男" => "男".to_string(),
        "濂?" | "濂�" | "女" => "女".to_string(),
        "鍎跨" | "鍎跨童" | "儿童" => "儿童".to_string(),
        "灏戝勾" | "少年" | "少儿" => "少年".to_string(),
        "闈掑勾" | "青年" => "青年".to_string(),
        "涓勾" | "中年" => "中年".to_string(),
        "鑰佸勾" | "老年" => "老年".to_string(),
        "濂崇" | "女童" => "女童".to_string(),
        "灏戝コ" | "少女" => "少女".to_string(),
        "缇や紬鐢烽潚骞碶" => "群众男青年".to_string(),
        "缇や紬濂抽潚骞碶" => "群众女青年".to_string(),
        "缇や紬鐢峰皯骞碶" => "群众男少年".to_string(),
        "缇や紬濂冲皯骞碶" => "群众女少年".to_string(),
        "鑲栨晱" => "肖敏".to_string(),
        other => other.to_string(),
    }
}

pub fn is_suspicious_ai_name(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return true;
    }

    let suspicious_fragments = [
        "缇や紬",
        "鐢",
        "濂",
        "闈",
        "灏",
        "鍎",
        "鑰",
        "涓",
        "骞",
        "碶",
        "鑲",
        "栨",
        "晱",
        "�",
    ];

    suspicious_fragments
        .iter()
        .any(|fragment| value.contains(fragment))
}

fn extract_json_object(content: &str) -> Option<&str> {
    // Try each `{` as start, find matching `}`, try parsing.
    let mut start = 0;
    while let Some(pos) = content[start..].find('{') {
        let abs_start = start + pos;
        let rest = &content[abs_start..];
        if let Some(end) = rest.rfind('}') {
            let candidate = &content[abs_start..=abs_start + end];
            if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                return Some(candidate);
            }
        }
        start = abs_start + 1;
    }
    None
}

pub fn pick_api_key(keys: &str) -> Option<&str> {
    keys.split("@@").map(str::trim).find(|key| !key.is_empty())
}

fn normalize_name(name: &str) -> String {
    name.trim()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
}

fn name_parts(name: &str) -> Vec<String> {
    name.split('|')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}
