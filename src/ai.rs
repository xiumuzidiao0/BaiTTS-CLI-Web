use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

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
            MaleChild, FemaleChild, MaleTeen, FemaleTeen,
            MaleYouth, FemaleYouth, MaleMiddleAge, FemaleMiddleAge,
            MaleElder, FemaleElder,
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
        self.entries.get(category).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

/// Session-level round-robin allocator — resets per book, not persisted.
#[derive(Debug, Clone, Default)]
pub struct VoiceAllocator {
    next_index: HashMap<VoiceCategory, usize>,
    resolved: HashMap<String, String>,
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
        let idx = self.next_index.get(category).copied().unwrap_or(0);
        let voice_id = voices[idx % voices.len()].clone();
        self.next_index.insert(category.clone(), idx + 1);
        self.resolved.insert(character_name.to_string(), voice_id.clone());
        Some(voice_id)
    }

    /// Pre-seed a resolved entry (used to load pre-computed allocation tables).
    pub fn pre_seed(&mut self, character_name: &str, voice_id: &str) {
        self.resolved.insert(character_name.to_string(), voice_id.to_string());
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
    pub category: Option<VoiceCategory>,
    pub category_label: Option<String>,
    pub voice_id: String,
    pub source: AllocationSource,
    #[serde(default)]
    pub locked: bool,
}

/// Per-novel voice allocation table, persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VoiceAllocationTable {
    pub file_path: String,
    pub novel_title: String,
    pub entries: Vec<VoiceAllocationEntry>,
    pub generated_at: u64,
}

impl VoiceAllocationTable {
    pub fn lookup(&self, character_name: &str) -> Option<&VoiceAllocationEntry> {
        self.entries.iter().find(|e| e.character_name == character_name)
    }

    pub fn upsert(&mut self, entry: VoiceAllocationEntry) {
        if let Some(existing) = self.entries.iter_mut()
            .find(|e| e.character_name == entry.character_name)
        {
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
        allocator.pre_seed(&entry.character_name, &entry.voice_id);
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
                    category: character.category.clone(),
                    category_label: character.category.as_ref().map(|c| c.label().to_string()),
                    voice_id: voice_id.clone(),
                    source: AllocationSource::CharacterOverride,
                    locked: false,
                });
                continue;
            }
        }
        // Category-based round-robin
        if let Some(ref category) = character.category {
            if let Some(voice_id) = allocator.allocate(&config.voice_pool, category, &character.name) {
                entries.push(VoiceAllocationEntry {
                    character_name: character.name.clone(),
                    category: Some(category.clone()),
                    category_label: Some(category.label().to_string()),
                    voice_id,
                    source: AllocationSource::CharacterCategory,
                    locked: false,
                });
            }
        }
    }

    // Carry over locked entries NOT in character table (AI-inferred from previous run)
    for entry in locked.values() {
        if !entries.iter().any(|e| e.character_name == entry.character_name) {
            entries.push(entry.clone());
        }
    }

    VoiceAllocationTable {
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
    resolve_speaker_voice(config, allocator, &character.name, speaker_gender, speaker_age)
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

#[derive(Debug, Deserialize, Serialize)]
pub struct SpeakerResult {
    pub name: String,
    #[serde(default)]
    pub gender: Option<String>,
    #[serde(default)]
    pub age: Option<String>,
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

pub fn should_use_ai(config: &AiDialogueConfig) -> bool {
    check_ai_config(config).is_ok()
}

pub fn check_ai_config(config: &AiDialogueConfig) -> Result<(), &'static str> {
    if !config.enabled {
        return Err("AI 对话分配未启用");
    }
    if config.api_url.as_ref().map_or(true, |v| v.trim().is_empty()) {
        return Err("模型 API 地址未填写");
    }
    if config.api_key.as_ref().map_or(true, |v| v.trim().is_empty()) {
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

    let prompt = build_prompt(dialogue, chapter_context, &config.characters);
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

    let response = client
        .post(api_url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .context("AI request failed")?;

    if !response.status().is_success() {
        return Err(anyhow!("AI request failed with status {}", response.status()));
    }

    let parsed = response
        .json::<ChatCompletionResponse>()
        .await
        .context("failed to parse AI response")?;

    let content = parsed
        .choices
        .first()
        .map(|choice| choice.message.effective_content().trim())
        .filter(|content| !content.is_empty())
        .ok_or_else(|| anyhow!("AI response content is empty"))?;

    parse_speaker_json(content)
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

    let raw_text = response.text().await.context("failed to read AI suggestion response")?;

    let parsed: ChatCompletionResponse = serde_json::from_str(&raw_text)
        .with_context(|| format!("failed to parse AI suggestion response: {}", raw_text.chars().take(500).collect::<String>()))?;

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
    let pool: VoicePool = serde_json::from_str(json_text)
        .with_context(|| format!("AI suggestion JSON does not match voice pool schema. Parsed content: {}", json_text.chars().take(500).collect::<String>()))?;

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
            "{{\"name\":\"speaker name\",\"gender\":\"男 or 女 or null\",\"age\":\"儿童/少年/青年/中年/老年/女童/少女 or null\"}}\n\n",
            "Known characters:\n{}\n\n",
            "Highlighted dialogue:\n{}\n\n",
            "Context:\n{}"
        ),
        serde_json::to_string(&known_characters).unwrap_or_else(|_| "[]".to_string()),
        dialogue,
        chapter_context
    )
}

fn parse_speaker_json(content: &str) -> Result<SpeakerResult> {
    let json_text = extract_json_object(content).unwrap_or(content);
    let value: serde_json::Value =
        serde_json::from_str(json_text).with_context(|| {
            format!("AI response is not valid JSON. Raw: {}", content.chars().take(300).collect::<String>())
        })?;

    if let Some(speaker_info) = value.get("speaker_info") {
        return serde_json::from_value(speaker_info.clone())
            .context("AI speaker_info JSON does not match schema");
    }

    serde_json::from_value(value).context("AI JSON does not match speaker schema")
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
