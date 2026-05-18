use axum::{
    Router,
    extract::{DefaultBodyLimit, Json, Multipart, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{
        Html, IntoResponse,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast};
use tokio::task::AbortHandle;
use tower_http::cors::CorsLayer;
use walkdir::WalkDir;

use crate::ai::{AiDialogueConfig, VoiceAllocationTable};
use crate::api::{ApiClient, Voice};
use crate::args::Cli;
use crate::process;
use futures::stream::Stream;
use regex::Regex;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_stream::StreamExt;

// 新增：用于存储预加载的数据
#[derive(Serialize, Clone, Default)]
struct InitialData {
    api_url: Option<String>,
    voices: Option<Vec<Voice>>,
    default_volume: u8,
    default_speed: u8,
    default_pitch: u8,
    ai_dialogue_config: Option<AiDialogueConfig>,
    preserve_structure: bool,
    enable_lrc: bool,
    lrc_chars: usize,
    analysis_only: bool,
    delete_task_allocations: bool,
    delete_task_outputs: bool,
    delete_task_sources: bool,
    ai_prompt_price_per_m: f64,
    ai_completion_price_per_m: f64,
}

// 新增：任务状态结构体
#[derive(Serialize, Deserialize, Clone, Debug)]
struct TaskState {
    id: String,
    file_name: String,
    full_path: Option<String>, // 新增：完整路径，用于重试
    status: String,            // pending, processing, paused, completed, error, cancelled
    current: usize,
    total: usize,
    error_msg: Option<String>,
    cli_config: Option<Cli>, // 新增：保存任务配置，用于重试
    start_time: Option<u64>, // 新增：任务开始时间戳
    end_time: Option<u64>,   // 新增：任务结束时间戳
    size: Option<u64>,       // 新增：任务输出大小
    eta: Option<u64>,        // 新增：预计剩余时间(秒)
    #[serde(default)]
    created_at: Option<u64>, // 新增：任务创建时间戳(毫秒)，用于排序
    #[serde(default)]
    output_path: Option<String>, // 新增：输出路径
    #[serde(default)]
    is_hidden: bool, // 新增：是否在前端隐藏
    #[serde(default)]
    active_phase: Option<String>, // analysis / synthesis
    #[serde(default)]
    analysis_phase: Option<TaskPhaseState>,
    #[serde(default)]
    synthesis_phase: Option<TaskPhaseState>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct TaskPhaseState {
    status: String,
    current: usize,
    total: usize,
    error_msg: Option<String>,
    start_time: Option<u64>,
    end_time: Option<u64>,
    size: Option<u64>,
    eta: Option<u64>,
    output_path: Option<String>,
}

// 共享状态
struct AppState {
    tx: broadcast::Sender<String>,
    task_handle: Mutex<Option<AbortHandle>>,
    initial_data: Arc<Mutex<InitialData>>,
    tasks: Arc<Mutex<HashMap<String, TaskState>>>, // 新增：任务列表
    default_volume: u8,
    default_speed: u8,
    default_pitch: u8,
    autorun_config: Arc<Mutex<Option<Cli>>>,
    is_autorun: Arc<AtomicBool>,
    ai_dialogue_config: Arc<Mutex<AiDialogueConfig>>,
}

#[derive(Deserialize)]
struct TtsRequest {
    api_url: String,
    voice_id: String,
    voice_dialogue_id: Option<String>,
    text_content: Option<String>,
    file_path: Option<String>,
    volume_dialogue: Option<u8>,
    speed_dialogue: Option<u8>,
    pitch_dialogue: Option<u8>,
    volume: Option<u8>,
    speed: Option<u8>,
    pitch: Option<u8>,
    output_name: Option<String>,
    sub: Option<usize>,
    concurrency: Option<usize>,
    ignore_regex: Option<String>,
    #[serde(default)]
    ai_dialogue: Option<AiDialogueConfig>,
    #[serde(default)]
    analysis_only: bool,
}

#[derive(Deserialize)]
struct AnalyzeBookRequest {
    file_path: String,
    ignore_regex: Option<String>,
    #[serde(default)]
    ai_dialogue: Option<AiDialogueConfig>,
}

#[derive(Serialize)]
struct ApiResponse {
    success: bool,
    message: String,
}

#[derive(Deserialize)]
struct VoicesQuery {
    api_url: String,
}

#[derive(Deserialize)]
struct BatchConvertRequest {
    voice_id: String,
    voice_dialogue_id: Option<String>,
    volume_dialogue: Option<u8>,
    speed_dialogue: Option<u8>,
    pitch_dialogue: Option<u8>,
    volume: Option<u8>,
    speed: Option<u8>,
    pitch: Option<u8>,
    sub: Option<usize>,
    ignore_regex: Option<String>,
    #[serde(default)]
    preserve_structure: bool,
    #[serde(default)]
    analysis_only: bool,
    #[serde(default)]
    ai_dialogue: Option<AiDialogueConfig>,
}

#[derive(Deserialize)]
struct CancelTaskRequest {
    id: String,
}

#[derive(Deserialize)]
struct DeleteTasksRequest {
    ids: Vec<String>,
    delete_allocations: Option<bool>,
    delete_outputs: Option<bool>,
    delete_sources: Option<bool>,
}

#[derive(Serialize)]
struct DeleteTasksResponse {
    success: bool,
    message: String,
    deleted_task_ids: Vec<String>,
    deleted_allocation_paths: Vec<String>,
    deleted_output_paths: Vec<String>,
    deleted_source_paths: Vec<String>,
    failed_allocation_paths: Vec<String>,
    failed_output_paths: Vec<String>,
    failed_source_paths: Vec<String>,
    deleted_tasks: usize,
    deleted_allocations: usize,
    deleted_outputs: usize,
    deleted_sources: usize,
}

#[derive(Deserialize)]
struct TestRegexRequest {
    regex: String,
    text: String,
}

#[derive(Serialize)]
struct TestRegexResponse {
    success: bool,
    result: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct AutorunRequest {
    enabled: bool,
    config: Option<BatchConvertRequest>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct ServerConfig {
    #[serde(default)]
    api_url: Option<String>,
    #[serde(default)]
    default_volume: Option<u8>,
    #[serde(default)]
    default_speed: Option<u8>,
    #[serde(default)]
    default_pitch: Option<u8>,
    #[serde(default)]
    preserve_structure: Option<bool>,
    #[serde(default)]
    enable_lrc: Option<bool>,
    #[serde(default)]
    lrc_chars: Option<usize>,
    #[serde(default)]
    analysis_only: Option<bool>,
    #[serde(default)]
    delete_task_allocations: Option<bool>,
    #[serde(default)]
    delete_task_outputs: Option<bool>,
    #[serde(default)]
    delete_task_sources: Option<bool>,
    #[serde(default)]
    ai_prompt_price_per_m: Option<f64>,
    #[serde(default)]
    ai_completion_price_per_m: Option<f64>,
}

fn load_server_config() -> ServerConfig {
    let path = data_dir().join("config.json");
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str(&content) {
                return cfg;
            }
        }
    }
    ServerConfig::default()
}

fn save_server_config(cfg: &ServerConfig) {
    let dir = data_dir();
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }
    let path = dir.join("config.json");
    if let Ok(content) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(path, content);
    }
}

#[derive(Serialize)]
struct AutorunStatus {
    enabled: bool,
}

#[derive(Deserialize)]
struct StartSynthesisFromTaskRequest {
    id: String,
}

fn task_phase_from_task(task: &TaskState, status: &str) -> TaskPhaseState {
    TaskPhaseState {
        status: status.to_string(),
        current: task.current,
        total: task.total,
        error_msg: task.error_msg.clone(),
        start_time: task.start_time,
        end_time: task.end_time,
        size: task.size,
        eta: task.eta,
        output_path: task.output_path.clone(),
    }
}

fn sync_active_phase(task: &mut TaskState) {
    match task.active_phase.as_deref() {
        Some("analysis") => {
            task.analysis_phase = Some(task_phase_from_task(task, &task.status));
        }
        Some("synthesis") => {
            task.synthesis_phase = Some(task_phase_from_task(task, &task.status));
        }
        _ => {}
    }
}

#[derive(Serialize)]
struct FileEntry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: u64,
}

#[derive(Deserialize)]
struct ListFilesQuery {
    root: String,
    path: Option<String>,
}

#[derive(Deserialize)]
struct FileActionRequest {
    root: String,
    path: String,
    inline: Option<bool>,
}

pub async fn start_server(port: u16, api_url: Option<String>) -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let (tx, _rx) = broadcast::channel(100);

    let saved_cfg = load_server_config();
    let api_url = api_url.or(saved_cfg.api_url);
    let default_volume = std::env::var("DEFAULT_VOLUME")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(saved_cfg.default_volume)
        .unwrap_or(50);
    let default_speed = std::env::var("DEFAULT_SPEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(saved_cfg.default_speed)
        .unwrap_or(50);
    let default_pitch = std::env::var("DEFAULT_PITCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(saved_cfg.default_pitch)
        .unwrap_or(50);
    let is_autorun_env = std::env::var("AUTORUN")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);

    let is_autorun = Arc::new(AtomicBool::new(is_autorun_env));
    let autorun_config = Arc::new(Mutex::new(None));

    if is_autorun_env {
        // 如果环境变量开启了自动运行，创建一个默认配置
        let cli = Cli {
            list: false,
            file: None,
            dir: None,
            api: api_url.clone(),
            out: if PathBuf::from("/output").exists() {
                PathBuf::from("/output")
            } else {
                PathBuf::from("output")
            },
            output_name: None,
            voice: None, // 将在运行时尝试使用默认或第一个可用声音
            voice_dialogue: None,
            volume_dialogue: None,
            speed_dialogue: None,
            pitch_dialogue: None,
            volume: default_volume,
            speed: default_speed,
            pitch: default_pitch,
            sub: 0,
            blacklist: None,
            ignore_regex: r"\*{3,}|#{2,}".to_string(),
            concurrency: 4,
            preserve_structure: false,
            analysis_only: false,
            web: false,
            ai_dialogue: AiDialogueConfig::default(),
        };
        *autorun_config.lock().await = Some(cli);
    }

    // 启动时加载任务状态
    let mut tasks_map = load_tasks_from_disk().await;
    // 服务重启后没有任何任务线程存活，遗留的 processing/paused 任务需要重试才能继续。
    let mut tasks_changed_on_startup = false;
    let startup_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for task in tasks_map.values_mut() {
        if task.status == "processing" || task.status == "paused" {
            task.status = "cancelled".to_string();
            task.error_msg = Some("服务器重启，任务已取消，请重试".to_string());
            task.end_time = Some(startup_time);
            task.eta = None;
            sync_active_phase(task);
            tasks_changed_on_startup = true;
        }
    }
    if tasks_changed_on_startup {
        save_tasks_to_disk(&tasks_map).await;
    }

    let initial_data = Arc::new(Mutex::new(InitialData {
        api_url: api_url.clone(),
        voices: None,
        default_volume,
        default_speed,
        default_pitch,
        ai_dialogue_config: None,
        preserve_structure: saved_cfg.preserve_structure.unwrap_or(false),
        enable_lrc: saved_cfg.enable_lrc.unwrap_or(false),
        lrc_chars: saved_cfg.lrc_chars.unwrap_or(15),
        analysis_only: saved_cfg.analysis_only.unwrap_or(false),
        delete_task_allocations: saved_cfg.delete_task_allocations.unwrap_or(false),
        delete_task_outputs: saved_cfg.delete_task_outputs.unwrap_or(false),
        delete_task_sources: saved_cfg.delete_task_sources.unwrap_or(false),
        ai_prompt_price_per_m: saved_cfg.ai_prompt_price_per_m.unwrap_or(0.0),
        ai_completion_price_per_m: saved_cfg.ai_completion_price_per_m.unwrap_or(0.0),
    }));

    if let Some(url) = api_url {
        let initial_data_clone = Arc::clone(&initial_data);
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            log_info(&tx_clone, format!("从环境变量加载 API URL: {}", url));
            log_info(&tx_clone, "正在后台获取声音列表...".to_string());
            match ApiClient::new(url) {
                Ok(client) => match client.fetch_voices().await {
                    Ok(voices) => {
                        let mut data = initial_data_clone.lock().await;
                        data.voices = Some(voices);
                        log_info(&tx_clone, "后台声音列表获取成功。".to_string());
                    }
                    Err(e) => log_error(&tx_clone, format!("后台获取声音列表失败: {}", e)),
                },
                Err(e) => log_error(&tx_clone, format!("创建 API 客户端失败: {}", e)),
            }
        });
    }

    let ai_dialogue_config = Arc::new(Mutex::new(load_ai_dialogue_config()));

    let app_state = Arc::new(AppState {
        tx,
        task_handle: Mutex::new(None),
        initial_data,
        tasks: Arc::new(Mutex::new(tasks_map)),
        default_volume,
        default_speed,
        default_pitch,
        autorun_config: autorun_config.clone(),
        is_autorun: is_autorun.clone(),
        ai_dialogue_config: ai_dialogue_config.clone(),
    });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/api/initial_data", get(initial_data_handler))
        .route("/api/tasks", get(get_tasks_handler)) // 新增：获取任务列表
        .route("/api/voices", get(get_voices_handler))
        .route("/api/synthesize", post(synthesize_handler))
        .route("/api/synthesize_upload", post(synthesize_upload_handler))
        .route("/api/analyze_book", post(analyze_book_handler))
        .route(
            "/api/analyze_book_upload",
            post(analyze_book_upload_handler),
        )
        .route("/api/batch_convert", post(batch_convert_handler)) // 新增
        .route("/api/autorun", post(set_autorun_handler)) // 新增：设置自动运行
        .route("/api/autorun/status", get(get_autorun_status_handler)) // 新增：获取自动运行状态
        .route("/api/cancel_task", post(cancel_task_handler))
        .route("/api/pause_task", post(pause_task_handler))
        .route("/api/resume_task", post(resume_task_handler))
        .route("/api/pause_all", post(pause_all_handler))
        .route("/api/resume_all", post(resume_all_handler))
        .route("/api/retry_task", post(retry_task_handler))
        .route(
            "/api/task/start_synthesis",
            post(start_synthesis_from_task_handler),
        )
        .route("/api/retry_all_failed", post(retry_all_failed_handler))
        .route(
            "/api/clear_completed_tasks",
            post(clear_completed_tasks_handler),
        )
        .route("/api/delete_tasks", post(delete_tasks_handler))
        .route("/api/reset_history", post(reset_history_handler))
        .route("/api/files/list", get(list_files_handler)) // 新增：列出文件
        .route("/api/files/delete", post(delete_file_handler)) // 新增：删除文件
        .route("/api/files/download", get(download_file_handler)) // 新增：下载文件
        .route("/api/files/upload", post(upload_file_manager_handler)) // 新增：上传文件(文件管理)
        .route("/api/preview", post(preview_handler)) // 新增预览接口
        .route("/api/test_regex", post(test_regex_handler)) // 新增：测试正则
        .route(
            "/api/ai_config",
            get(get_ai_config_handler).post(save_ai_config_handler),
        )
        .route("/api/ai_test", post(ai_test_handler))
        .route(
            "/api/ai_identify_preview",
            post(ai_identify_preview_handler),
        )
        .route("/api/ai_usage_stats", get(ai_usage_stats_handler))
        .route(
            "/api/ai_usage_stats/novel/delete",
            post(delete_ai_usage_novel_handler),
        )
        .route("/api/voice_preview", post(voice_preview_handler))
        .route("/api/save_settings", post(save_settings_handler))
        .route("/api/voice_pool/suggest", post(suggest_voice_pool_handler))
        .route("/api/allocations/list", get(list_allocations_handler))
        .route("/api/allocation/get", get(get_allocation_handler))
        .route(
            "/api/allocation/generate",
            post(generate_allocation_handler),
        )
        .route("/api/allocation/update", post(update_allocation_handler))
        .route("/api/allocation/save", post(save_allocation_handler))
        .route("/api/allocation/delete", post(delete_allocation_handler))
        .route("/api/stop", post(stop_handler))
        .route("/api/events", get(sse_handler))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .layer(CorsLayer::permissive())
        .with_state(app_state.clone());

    // 启动自动检测后台任务
    let watcher_state = app_state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            if watcher_state.is_autorun.load(Ordering::Relaxed) {
                scan_and_process_autorun(&watcher_state).await;
            }
        }
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("WebUI 已启动: http://localhost:{}", port);
    let _ = open::that(format!("http://localhost:{}", port));

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn set_autorun_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AutorunRequest>,
) -> impl IntoResponse {
    state.is_autorun.store(req.enabled, Ordering::Relaxed);

    if req.enabled {
        if let Some(config) = req.config {
            let api_url = state.initial_data.lock().await.api_url.clone();
            let out_dir = if PathBuf::from("/output").exists() {
                PathBuf::from("/output")
            } else {
                PathBuf::from("output")
            };

            let cli = Cli {
                list: false,
                file: None,
                dir: None,
                api: api_url,
                out: out_dir,
                output_name: None,
                voice: Some(config.voice_id),
                voice_dialogue: config.voice_dialogue_id,
                volume_dialogue: config.volume_dialogue,
                speed_dialogue: config.speed_dialogue,
                pitch_dialogue: config.pitch_dialogue,
                volume: config.volume.unwrap_or(state.default_volume),
                speed: config.speed.unwrap_or(state.default_speed),
                pitch: config.pitch.unwrap_or(state.default_pitch),
                sub: config.sub.unwrap_or(0),
                blacklist: None,
                ignore_regex: config
                    .ignore_regex
                    .unwrap_or_else(|| r"\*{3,}|#{2,}".to_string()),
                concurrency: 4,
                preserve_structure: config.preserve_structure,
                analysis_only: config.analysis_only,
                web: false,
                ai_dialogue: config.ai_dialogue.unwrap_or_default(),
            };
            *state.autorun_config.lock().await = Some(cli);
            log_info(
                &state.tx,
                "🤖 自动检测已开启，将每 10 秒扫描一次 /book 目录。".to_string(),
            );
        } else {
            // 如果没有提供配置但开启了，尝试使用现有配置，如果没有则报错
            let guard = state.autorun_config.lock().await;
            if guard.is_none() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ApiResponse {
                        success: false,
                        message: "开启自动检测需要提供配置参数".to_string(),
                    }),
                )
                    .into_response();
            }
            log_info(&state.tx, "🤖 自动检测已开启 (使用已有配置)。".to_string());
        }
    } else {
        log_info(&state.tx, "🤖 自动检测已停止。".to_string());
    }

    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: if req.enabled {
                "自动检测已开启"
            } else {
                "自动检测已关闭"
            }
            .to_string(),
        }),
    )
        .into_response()
}

async fn get_autorun_status_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let enabled = state.is_autorun.load(Ordering::Relaxed);
    Json(AutorunStatus { enabled })
}

async fn get_ai_config_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.ai_dialogue_config.lock().await;
    Json(config.clone())
}

async fn save_ai_config_handler(
    State(state): State<Arc<AppState>>,
    Json(config): Json<AiDialogueConfig>,
) -> impl IntoResponse {
    save_ai_dialogue_config(&config);
    *state.ai_dialogue_config.lock().await = config;
    log_info(&state.tx, "AI 对话分配配置已保存。".to_string());
    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: "配置已保存".to_string(),
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
struct SaveSettingsRequest {
    preserve_structure: Option<bool>,
    enable_lrc: Option<bool>,
    lrc_chars: Option<usize>,
    analysis_only: Option<bool>,
    delete_task_allocations: Option<bool>,
    delete_task_outputs: Option<bool>,
    delete_task_sources: Option<bool>,
    ai_prompt_price_per_m: Option<f64>,
    ai_completion_price_per_m: Option<f64>,
}

async fn save_settings_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SaveSettingsRequest>,
) -> impl IntoResponse {
    let mut cfg = load_server_config();
    if let Some(v) = req.preserve_structure {
        cfg.preserve_structure = Some(v);
    }
    if let Some(v) = req.enable_lrc {
        cfg.enable_lrc = Some(v);
    }
    if let Some(v) = req.lrc_chars {
        cfg.lrc_chars = Some(v);
    }
    if let Some(v) = req.analysis_only {
        cfg.analysis_only = Some(v);
    }
    if let Some(v) = req.delete_task_allocations {
        cfg.delete_task_allocations = Some(v);
    }
    if let Some(v) = req.delete_task_outputs {
        cfg.delete_task_outputs = Some(v);
    }
    if let Some(v) = req.delete_task_sources {
        cfg.delete_task_sources = Some(v);
    }
    if let Some(v) = req.ai_prompt_price_per_m {
        cfg.ai_prompt_price_per_m = Some(v.max(0.0));
    }
    if let Some(v) = req.ai_completion_price_per_m {
        cfg.ai_completion_price_per_m = Some(v.max(0.0));
    }
    save_server_config(&cfg);
    let mut initial_data = state.initial_data.lock().await;
    if let Some(v) = cfg.preserve_structure {
        initial_data.preserve_structure = v;
    }
    if let Some(v) = cfg.enable_lrc {
        initial_data.enable_lrc = v;
    }
    if let Some(v) = cfg.lrc_chars {
        initial_data.lrc_chars = v;
    }
    if let Some(v) = cfg.analysis_only {
        initial_data.analysis_only = v;
    }
    if let Some(v) = cfg.delete_task_allocations {
        initial_data.delete_task_allocations = v;
    }
    if let Some(v) = cfg.delete_task_outputs {
        initial_data.delete_task_outputs = v;
    }
    if let Some(v) = cfg.delete_task_sources {
        initial_data.delete_task_sources = v;
    }
    if let Some(v) = cfg.ai_prompt_price_per_m {
        initial_data.ai_prompt_price_per_m = v;
    }
    if let Some(v) = cfg.ai_completion_price_per_m {
        initial_data.ai_completion_price_per_m = v;
    }
    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: "设置已保存".to_string(),
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
struct AiTestRequest {
    api_url: String,
    api_key: String,
    model: String,
}

async fn ai_test_handler(Json(req): Json<AiTestRequest>) -> impl IntoResponse {
    let start = std::time::Instant::now();
    let api_key = crate::ai::pick_api_key(&req.api_key).unwrap_or(&req.api_key);
    let body = serde_json::json!({
        "model": req.model,
        "messages": [
            {"role": "user", "content": "Hi"}
        ],
        "max_tokens": 10
    });

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::json!({"success": false, "error": format!("Failed to create client: {}", e)}),
                ),
            );
        }
    };

    match client
        .post(&req.api_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&body)
        .send()
        .await
    {
        Ok(res) => {
            let latency = start.elapsed().as_millis();
            if res.status().is_success() {
                (
                    StatusCode::OK,
                    Json(serde_json::json!({"success": true, "latency_ms": latency})),
                )
            } else {
                let status = res.status().to_string();
                let body = res.text().await.unwrap_or_default();
                (
                    StatusCode::OK,
                    Json(
                        serde_json::json!({"success": false, "error": format!("Status {}: {}", status, body.chars().take(300).collect::<String>())}),
                    ),
                )
            }
        }
        Err(e) => (
            StatusCode::OK,
            Json(serde_json::json!({"success": false, "error": e.to_string()})),
        ),
    }
}

#[derive(Deserialize)]
struct AiIdentifyPreviewRequest {
    config: AiDialogueConfig,
    dialogue: String,
    context: String,
}

async fn ai_identify_preview_handler(
    Json(req): Json<AiIdentifyPreviewRequest>,
) -> impl IntoResponse {
    if let Err(msg) = crate::ai::check_ai_config(&req.config) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"success": false, "error": msg})),
        );
    }

    match crate::ai::identify_speaker(
        &req.config,
        &req.dialogue,
        &req.context,
        &req.config.characters,
        None,
    )
    .await
    {
        Ok(speaker) => {
            let matched = crate::ai::match_character(&req.config.characters, &speaker.name);
            let matched_name = matched.map(|c| c.name.clone());
            let matched_category =
                matched.and_then(|c| c.category.as_ref().map(|c| c.label().to_string()));
            let mut allocator = crate::ai::VoiceAllocator::new();
            let resolved_voice = if let Some(c) = matched {
                crate::ai::resolve_character_voice(
                    &req.config,
                    &mut allocator,
                    c,
                    speaker.gender.as_deref(),
                    speaker.age.as_deref(),
                )
            } else {
                crate::ai::resolve_speaker_voice(
                    &req.config,
                    &mut allocator,
                    &speaker.name,
                    speaker.gender.as_deref(),
                    speaker.age.as_deref(),
                )
            };
            let category_label = matched_category.or_else(|| {
                crate::ai::VoiceCategory::infer(speaker.gender.as_deref(), speaker.age.as_deref())
                    .map(|c| c.label().to_string())
            });
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "speaker": speaker,
                    "matched_character": matched_name,
                    "matched_category": category_label,
                    "matched_voice": resolved_voice,
                    "assigned_voice": resolved_voice,
                })),
            )
        }
        Err(e) => (
            StatusCode::OK,
            Json(serde_json::json!({"success": false, "error": e.to_string()})),
        ),
    }
}

async fn ai_usage_stats_handler() -> impl IntoResponse {
    let stats = crate::ai::load_ai_usage_stats();
    (StatusCode::OK, Json(stats)).into_response()
}

#[derive(Deserialize)]
struct DeleteAiUsageNovelRequest {
    novel_title: String,
}

async fn delete_ai_usage_novel_handler(
    Json(req): Json<DeleteAiUsageNovelRequest>,
) -> impl IntoResponse {
    if crate::ai::delete_ai_usage_novel(&req.novel_title).await {
        (
            StatusCode::OK,
            Json(ApiResponse {
                success: true,
                message: "已删除该小说的 AI 统计".to_string(),
            }),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(ApiResponse {
                success: false,
                message: "未找到该小说的 AI 统计".to_string(),
            }),
        )
    }
}

#[derive(Deserialize)]
struct SuggestVoicePoolRequest {
    config: AiDialogueConfig,
    voices: Vec<crate::api::Voice>,
}

async fn suggest_voice_pool_handler(Json(req): Json<SuggestVoicePoolRequest>) -> impl IntoResponse {
    if let Err(msg) = crate::ai::check_ai_config(&req.config) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"success": false, "error": msg})),
        );
    }
    match crate::ai::suggest_voice_pool(&req.config, &req.voices).await {
        Ok(pool) => (
            StatusCode::OK,
            Json(serde_json::json!({"success": true, "voice_pool": pool})),
        ),
        Err(e) => (
            StatusCode::OK,
            Json(serde_json::json!({"success": false, "error": e.to_string()})),
        ),
    }
}

// --- Allocation API ---

#[derive(Deserialize)]
struct GenerateAllocationRequest {
    file_path: String,
    config: AiDialogueConfig,
}

#[derive(Deserialize)]
struct GetAllocationQuery {
    file_path: String,
}

#[derive(Deserialize)]
struct UpdateAllocationRequest {
    file_path: String,
    character_name: String,
    voice_id: String,
    locked: bool,
}

#[derive(Deserialize)]
struct SaveAllocationRequest {
    table: VoiceAllocationTable,
}

#[derive(Deserialize)]
struct DeleteAllocationRequest {
    file_path: String,
}

async fn list_allocations_handler() -> impl IntoResponse {
    let dir = allocations_dir();
    if !dir.exists() {
        return Json(Vec::<serde_json::Value>::new());
    }
    let mut list = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                if let Ok(table) = serde_json::from_str::<VoiceAllocationTable>(&content) {
                    let file_name = std::path::Path::new(&table.file_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    list.push(serde_json::json!({
                        "file_path": table.file_path,
                        "novel_title": table.novel_title,
                        "entry_count": table.entries.len(),
                        "generated_at": table.generated_at,
                        "file_name": file_name,
                    }));
                }
            }
        }
    }
    list.sort_by_key(|v| v["file_name"].as_str().unwrap_or("").to_string());
    Json(list)
}

async fn get_allocation_handler(
    Query(q): Query<GetAllocationQuery>,
    State(_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Some(table) = load_allocation_table(&q.file_path) {
        return (
            StatusCode::OK,
            Json(serde_json::json!({"found": true, "table": table})),
        )
            .into_response();
    }
    (StatusCode::OK, Json(serde_json::json!({"found": false}))).into_response()
}

async fn generate_allocation_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<GenerateAllocationRequest>,
) -> impl IntoResponse {
    let path = std::path::Path::new(&req.file_path);
    if !path.exists() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"success": false, "error": "文件不存在"})),
        );
    }
    let config = &req.config;
    let book = match crate::extractor::extract_text(path) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::json!({"success": false, "error": format!("无法提取文本: {}", e)}),
                ),
            );
        }
    };
    let existing = load_allocation_table(&req.file_path);
    let table = crate::ai::generate_allocation_table(
        config,
        &req.file_path,
        &book.title,
        existing.as_ref(),
    );
    save_allocation_table(&table);
    // Also update in-memory config
    let mut cfg = state.ai_dialogue_config.lock().await;
    *cfg = config.clone();
    save_ai_dialogue_config(&cfg);
    (
        StatusCode::OK,
        Json(serde_json::json!({"success": true, "table": table})),
    )
}

async fn update_allocation_handler(Json(req): Json<UpdateAllocationRequest>) -> impl IntoResponse {
    let mut table = match load_allocation_table(&req.file_path) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"success": false, "error": "分配表不存在，请先生成"})),
            );
        }
    };
    let entry = crate::ai::VoiceAllocationEntry {
        character_name: req.character_name.clone(),
        category: None,
        category_label: None,
        voice_id: req.voice_id,
        source: crate::ai::AllocationSource::Manual,
        aliases: vec![],
        locked: req.locked,
        volume: None,
        speed: None,
        pitch: None,
        confidence: None,
        reason: None,
        needs_review: false,
    };
    table.upsert(entry);
    save_allocation_table(&table);
    (
        StatusCode::OK,
        Json(serde_json::json!({"success": true, "table": table})),
    )
}

async fn save_allocation_handler(Json(req): Json<SaveAllocationRequest>) -> impl IntoResponse {
    let mut table = req.table;
    if table.file_path.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"success": false, "error": "文件路径不能为空"})),
        )
            .into_response();
    }
    if table.generated_at == 0 {
        table.generated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
    }
    table = normalize_allocation_table(table);
    if let Err(e) = try_save_allocation_table(&table) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"success": false, "error": format!("保存失败: {}", e)})),
        )
            .into_response();
    }
    let table = load_allocation_table(&table.file_path).unwrap_or(table);
    (
        StatusCode::OK,
        Json(serde_json::json!({"success": true, "table": table})),
    )
        .into_response()
}

async fn delete_allocation_handler(Json(req): Json<DeleteAllocationRequest>) -> impl IntoResponse {
    if req.file_path.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"success": false, "error": "文件路径不能为空"})),
        )
            .into_response();
    }
    match delete_allocation_files_for_path(&req.file_path) {
        Ok(0) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"success": false, "error": "分配表不存在或路径不匹配"})),
            )
                .into_response();
        }
        Ok(deleted) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({"success": true, "deleted": deleted})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"success": false, "error": format!("删除失败: {}", e)})),
            )
                .into_response();
        }
    }
}

fn delete_allocation_files_for_path(file_path: &str) -> io::Result<usize> {
    let paths = find_allocation_file_paths(file_path);
    if paths.is_empty() {
        return Ok(0);
    }
    let mut deleted = 0;
    for path in paths {
        std::fs::remove_file(&path)?;
        deleted += 1;
    }
    Ok(deleted)
}

async fn scan_and_process_autorun(state: &Arc<AppState>) {
    let config_guard = state.autorun_config.lock().await;
    let cli = if let Some(c) = &*config_guard {
        c.clone()
    } else {
        return;
    };
    drop(config_guard);

    let api_url = if let Some(url) = &cli.api {
        url.clone()
    } else {
        let data = state.initial_data.lock().await;
        if let Some(url) = &data.api_url {
            url.clone()
        } else {
            return;
        }
    };

    let mut cli = cli;
    cli.api = Some(api_url.clone());

    let book_dir = if PathBuf::from("/book").exists() {
        PathBuf::from("/book")
    } else {
        PathBuf::from("book")
    };
    if !book_dir.exists() {
        return;
    }

    let files: Vec<PathBuf> = WalkDir::new(&book_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path()
                    .extension()
                    .map_or(false, |ext| ext == "txt" || ext == "epub")
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    let client = match ApiClient::new(api_url) {
        Ok(c) => c,
        Err(_) => return,
    };

    // 1. 发现新文件并加入队列 (Pending)
    let new_files: Vec<PathBuf> = {
        let tasks = state.tasks.lock().await;
        files
            .into_iter()
            .filter(|path| {
                let file_id = if cli.analysis_only {
                    analysis_task_id(path)
                } else {
                    synthesis_task_id(path)
                };
                !tasks.contains_key(&file_id)
            })
            .collect()
    };

    let mut new_task_specs = Vec::new();
    for path in new_files {
        let task_name = derive_task_title(&path, &cli.ai_dialogue).await;
        let mut task_cli = cli.clone();
        task_cli.output_name = Some(task_name.clone());
        if cli.preserve_structure {
            if let Ok(relative) = path.strip_prefix(&book_dir) {
                if let Some(parent) = relative.parent() {
                    if parent.components().count() > 0 {
                        task_cli.out = task_cli.out.join(parent);
                    }
                }
            }
        }
        new_task_specs.push((path, task_name, task_cli));
    }

    {
        let mut tasks = state.tasks.lock().await;
        let mut added = false;
        for (path, task_name, task_cli) in new_task_specs {
            let file_id = if task_cli.analysis_only {
                analysis_task_id(&path)
            } else {
                synthesis_task_id(&path)
            };
            if tasks.contains_key(&file_id) {
                continue;
            }

            tasks.insert(
                file_id.clone(),
                TaskState {
                    id: file_id,
                    file_name: if task_cli.analysis_only {
                        format!("分析: {}", task_name)
                    } else {
                        task_name
                    },
                    full_path: Some(path.to_string_lossy().to_string()),
                    status: "pending".to_string(), // 默认为等待中
                    current: 0,
                    total: 0,
                    error_msg: None,
                    cli_config: Some(task_cli.clone()),
                    start_time: None,
                    end_time: None,
                    size: None,
                    eta: None,
                    created_at: Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64,
                    ),
                    output_path: None,
                    is_hidden: false,
                    active_phase: Some(if task_cli.analysis_only {
                        "analysis".to_string()
                    } else {
                        "synthesis".to_string()
                    }),
                    analysis_phase: if task_cli.analysis_only {
                        Some(TaskPhaseState {
                            status: "pending".to_string(),
                            ..Default::default()
                        })
                    } else {
                        None
                    },
                    synthesis_phase: if task_cli.analysis_only {
                        None
                    } else {
                        Some(TaskPhaseState {
                            status: "pending".to_string(),
                            ..Default::default()
                        })
                    },
                },
            );
            added = true;
            log_info(
                &state.tx,
                format!(
                    "🤖 自动检测到新文件(已加入队列): {:?}",
                    path.file_name().unwrap_or_default()
                ),
            );
        }
        if added {
            save_tasks_to_disk(&*tasks).await;
        }
    }

    // 2. 调度逻辑：检查是否有正在运行的任务，如果没有则启动下一个 Pending 任务
    let next_task = {
        let mut tasks = state.tasks.lock().await;

        // 如果有任务正在进行中，则跳过本次调度
        if tasks
            .values()
            .any(|t| t.status == "processing" || t.status == "paused")
        {
            return;
        }

        // 按加入时间排序找到下一个 pending 任务
        let target = tasks
            .values_mut()
            .filter(|t| t.status == "pending")
            .min_by(|a, b| {
                let t_a = a.created_at.unwrap_or(0);
                let t_b = b.created_at.unwrap_or(0);
                if t_a == t_b {
                    a.file_name.cmp(&b.file_name) // 时间相同则按文件名兜底
                } else {
                    t_a.cmp(&t_b)
                }
            });

        if let Some(t) = target {
            t.status = "processing".to_string();
            t.start_time = Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            );
            Some((t.id.clone(), t.full_path.clone(), t.cli_config.clone()))
        } else {
            None
        }
    };

    if let Some((file_id, Some(full_path), Some(task_cli))) = next_task {
        // 保存状态变更
        {
            let tasks = state.tasks.lock().await;
            save_tasks_to_disk(&*tasks).await;
        }

        let path = PathBuf::from(full_path);
        let tx = state.tx.clone();
        let tasks_state = state.tasks.clone();
        let fid = file_id.clone();
        let fid_cb = file_id.clone();
        let path_clone = path.clone();
        let cli_clone = task_cli.clone();
        let client_clone = client.clone();
        let is_autorun = state.is_autorun.clone();

        tokio::spawn(async move {
            let callback_tx = tx.clone();
            let tasks_clone = tasks_state.clone();

            let callback = move |event: process::ProcessEvent| match event {
                process::ProcessEvent::Log(msg) => log_info(&callback_tx, msg),
                process::ProcessEvent::Progress { current, total } => {
                    let tasks = tasks_clone.clone();
                    let f_id = fid_cb.clone();
                    tokio::spawn(async move {
                        let mut lock = tasks.lock().await;
                        if let Some(task) = lock.get_mut(&f_id) {
                            task.current = current;
                            task.total = total;
                            if let Some(start) = task.start_time {
                                let now = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap()
                                    .as_secs();
                                let elapsed = now.saturating_sub(start);
                                if elapsed > 0 && current > 0 {
                                    let rate = current as f64 / elapsed as f64;
                                    let remaining = total.saturating_sub(current);
                                    task.eta = Some((remaining as f64 / rate) as u64);
                                }
                            }
                        }
                    });
                }
                process::ProcessEvent::Success { size, output_path } => {
                    let tasks = tasks_clone.clone();
                    let f_id = fid_cb.clone();
                    tokio::spawn(async move {
                        let mut lock = tasks.lock().await;
                        if let Some(task) = lock.get_mut(&f_id) {
                            task.size = Some(size);
                            task.output_path = Some(output_path);
                        }
                        save_tasks_to_disk(&*lock).await;
                    });
                }
            };

            let tasks_state_cancel = tasks_state.clone();
            let check_cancel = move || {
                let is_autorun = is_autorun.clone();
                let tasks = tasks_state_cancel.clone();
                let fid = fid.clone();
                async move {
                    if !is_autorun.load(Ordering::Relaxed) {
                        return true;
                    }
                    task_cancelled_or_missing(tasks, fid).await
                }
            };

            log_info(
                &tx,
                format!(
                    "🤖 自动检测任务开始: {:?}",
                    path_clone.file_name().unwrap_or_default()
                ),
            );
            let fp_str = path_clone.to_string_lossy().to_string();
            let alloc_table = if crate::ai::should_use_ai(&cli_clone.ai_dialogue) {
                let table =
                    load_allocation_table(&fp_str).unwrap_or_else(|| VoiceAllocationTable {
                        schema_version: 2,
                        file_path: fp_str.clone(),
                        novel_title: String::new(),
                        entries: vec![],
                        generated_at: 0,
                    });
                Some(Arc::new(std::sync::Mutex::new(table)))
            } else {
                None
            };
            let wait_if_paused = {
                let tasks = tasks_state.clone();
                let fid = file_id.clone();
                move || {
                    let tasks = tasks.clone();
                    let fid = fid.clone();
                    async move { wait_task_if_paused(tasks, fid).await }
                }
            };
            let result = if cli_clone.analysis_only {
                process::analyze_file_dialogues(
                    &path_clone,
                    &cli_clone,
                    &None,
                    callback,
                    check_cancel,
                    wait_if_paused,
                    alloc_table.clone(),
                )
                .await
            } else {
                process::process_file(
                    &path_clone,
                    &cli_clone,
                    &client_clone,
                    &None,
                    callback,
                    check_cancel,
                    wait_if_paused,
                    alloc_table.clone(),
                )
                .await
            };
            match result {
                Ok(_) => {
                    if let Some(ref at) = alloc_table {
                        save_allocation_table(&at.lock().unwrap());
                    }
                    update_task_status_by_id(&tasks_state, &file_id, "completed", None).await;
                }
                Err(e) => {
                    if e.to_string() == "任务已取消" {
                        update_task_status_by_id(&tasks_state, &file_id, "cancelled", None).await;
                    } else {
                        update_task_status_by_id(
                            &tasks_state,
                            &file_id,
                            "error",
                            Some(e.to_string()),
                        )
                        .await;
                    }
                }
            }
        });
    }
}

async fn initial_data_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut data = state.initial_data.lock().await.clone();
    data.ai_dialogue_config = Some(state.ai_dialogue_config.lock().await.clone());
    Json(data)
}

// 新增：获取任务列表 Handler
async fn get_tasks_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let tasks_map = state.tasks.lock().await;
    let tasks: Vec<TaskState> = tasks_map
        .values()
        .filter(|t| !t.is_hidden)
        .cloned()
        .collect();
    Json(tasks)
}

async fn stop_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut handle_lock = state.task_handle.lock().await;

    // 更新所有进行中或等待的任务状态为已取消
    {
        let mut tasks = state.tasks.lock().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        for task in tasks.values_mut() {
            if task.status == "processing" || task.status == "pending" || task.status == "paused" {
                task.status = "cancelled".to_string();
                task.error_msg = Some("用户手动停止".to_string());
                task.end_time = Some(now);
                sync_active_phase(task);
            }
        }
        save_tasks_to_disk(&*tasks).await;
    }

    if let Some(handle) = handle_lock.take() {
        handle.abort();
        let _ = state.tx.send("🛑 用户已手动停止任务。".to_string());
        (
            StatusCode::OK,
            Json(ApiResponse {
                success: true,
                message: "任务已停止".to_string(),
            }),
        )
            .into_response()
    } else {
        (
            StatusCode::OK,
            Json(ApiResponse {
                success: true,
                message: "任务已停止 (清理了残留状态)".to_string(),
            }),
        )
            .into_response()
    }
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("static/index.html"))
}

async fn sse_handler(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>> {
    let rx = state.tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).map(|msg| match msg {
        Ok(msg) => Ok(Event::default().data(msg)),
        Err(_) => Ok(Event::default().comment("skipped")),
    });
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

// 修改：成功后持久化 API URL 和声音列表
async fn get_voices_handler(
    Query(params): Query<VoicesQuery>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    log_info(
        &state.tx,
        format!("正在从 {} 获取声音列表...", params.api_url),
    );
    match ApiClient::new(params.api_url.clone()) {
        Ok(client) => match client.fetch_voices().await {
            Ok(voices) => {
                // 持久化
                let mut data = state.initial_data.lock().await;
                data.api_url = Some(params.api_url.clone());
                data.voices = Some(voices.clone());
                let mut cfg = load_server_config();
                cfg.api_url = Some(params.api_url);
                save_server_config(&cfg);
                log_info(&state.tx, "API 地址和声音列表已更新。".to_string());

                (StatusCode::OK, Json(serde_json::to_value(voices).unwrap())).into_response()
            }
            Err(e) => {
                log_error(&state.tx, format!("获取声音列表失败: {}", e));
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e.to_string() })),
                )
                    .into_response()
            }
        },
        Err(e) => {
            log_error(&state.tx, format!("创建 API 客户端失败: {}", e));
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("无法创建客户端: {}", e) })),
            )
                .into_response()
        }
    }
}

/// Generate a short preview audio clip for a voice.
#[derive(Deserialize)]
struct VoicePreviewRequest {
    api_url: String,
    voice_id: String,
    volume: Option<u8>,
    speed: Option<u8>,
    pitch: Option<u8>,
}

async fn voice_preview_handler(Json(req): Json<VoicePreviewRequest>) -> impl IntoResponse {
    let client = match crate::api::ApiClient::new(req.api_url) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse {
                    success: false,
                    message: e.to_string(),
                }),
            )
                .into_response();
        }
    };
    let text = "你好，这是一段试听文本，用于测试声音效果。";
    let volume = req.volume.or(Some(60));
    let speed = req.speed.or(Some(50));
    let pitch = req.pitch.or(Some(50));
    match client
        .generate_speech(text, &Some(req.voice_id), &volume, &speed, &pitch)
        .await
    {
        Ok(data) => (StatusCode::OK, [("Content-Type", "audio/wav")], data).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiResponse {
                success: false,
                message: e.to_string(),
            }),
        )
            .into_response(),
    }
}

// 新增：批量转换 Handler
async fn batch_convert_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BatchConvertRequest>,
) -> impl IntoResponse {
    let tx = state.tx.clone();
    let tasks_state = state.tasks.clone();
    let api_url = {
        let data = state.initial_data.lock().await;
        data.api_url.clone()
    };

    let Some(api_url) = api_url else {
        let err_msg = "API 地址未设置，请先在 API 设置中获取声音列表。".to_string();
        log_error(&tx, err_msg.clone());
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: err_msg,
            }),
        )
            .into_response();
    };

    let default_volume = state.default_volume;
    let default_speed = state.default_speed;
    let default_pitch = state.default_pitch;

    let task = tokio::spawn(async move {
        let book_dir_abs = PathBuf::from("/book");
        let book_dir_rel = PathBuf::from("book");
        let book_dir = if book_dir_abs.exists() {
            book_dir_abs
        } else {
            book_dir_rel
        };

        let out_dir_abs = PathBuf::from("/output");
        let out_dir_rel = PathBuf::from("output");
        let out_dir = if out_dir_abs.exists() {
            out_dir_abs
        } else {
            out_dir_rel
        };

        if !book_dir.exists() {
            log_error(
                &tx,
                format!(
                    "输入目录 {:?} 不存在。请确认 Docker 卷已正确挂载或当前目录下存在 book 文件夹。",
                    book_dir
                ),
            );
            let _ = tx.send("__STATUS__:DONE".to_string());
            return;
        }
        if let Err(e) = tokio::fs::create_dir_all(&out_dir).await {
            log_error(&tx, format!("无法创建输出目录 {:?}: {}", out_dir, e));
            let _ = tx.send("__STATUS__:DONE".to_string());
            return;
        }

        let files_to_process: Vec<PathBuf> = WalkDir::new(&book_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().is_file()
                    && e.path()
                        .extension()
                        .map_or(false, |ext| ext == "txt" || ext == "epub")
            })
            .map(|e| e.path().to_path_buf())
            .collect();

        if files_to_process.is_empty() {
            log_info(
                &tx,
                format!("在 {:?} 目录中未找到 .txt 或 .epub 文件。", book_dir),
            );
            let _ = tx.send("__STATUS__:DONE".to_string());
            return;
        }

        log_info(
            &tx,
            format!("找到 {} 个文件，开始批量转换...", files_to_process.len()),
        );

        let cli = Cli {
            list: false,
            file: None,
            dir: None,
            api: Some(api_url.clone()),
            out: out_dir,
            output_name: None,
            voice: Some(req.voice_id),
            voice_dialogue: req.voice_dialogue_id,
            volume_dialogue: req.volume_dialogue,
            speed_dialogue: req.speed_dialogue,
            pitch_dialogue: req.pitch_dialogue,
            volume: req.volume.unwrap_or(default_volume),
            speed: req.speed.unwrap_or(default_speed),
            pitch: req.pitch.unwrap_or(default_pitch),
            sub: req.sub.unwrap_or(0),
            blacklist: None,
            ignore_regex: req
                .ignore_regex
                .clone()
                .unwrap_or_else(|| r"\*{3,}|#{2,}".to_string()),
            concurrency: 4,
            preserve_structure: req.preserve_structure,
            analysis_only: req.analysis_only,
            web: false,
            ai_dialogue: req.ai_dialogue.unwrap_or_default(),
        };

        let mut task_specs = Vec::new();
        for path in &files_to_process {
            let task_name = derive_task_title(path, &cli.ai_dialogue).await;
            let mut task_cli = cli.clone();
            task_cli.output_name = Some(task_name.clone());
            if req.preserve_structure {
                if let Ok(relative) = path.strip_prefix(&book_dir) {
                    if let Some(parent) = relative.parent() {
                        if parent.components().count() > 0 {
                            task_cli.out = task_cli.out.join(parent);
                        }
                    }
                }
            }
            task_specs.push((path.clone(), task_name, task_cli));
        }

        // 初始化任务列表
        {
            let mut tasks_lock = tasks_state.lock().await;
            for (path, task_name, task_cli) in &task_specs {
                let id = if task_cli.analysis_only {
                    analysis_task_id(path)
                } else {
                    synthesis_task_id(path)
                };

                // 如果任务不存在，或者状态不是 completed/cancelled，则重置为 pending
                let should_reset = if let Some(task) = tasks_lock.get(&id) {
                    task.status != "completed"
                        && task.status != "cancelled"
                        && task.status != "paused"
                } else {
                    true
                };

                if should_reset {
                    tasks_lock.insert(
                        id.clone(),
                        TaskState {
                            id,
                            file_name: if task_cli.analysis_only {
                                format!("分析: {}", task_name)
                            } else {
                                task_name.clone()
                            },
                            full_path: Some(path.to_string_lossy().to_string()),
                            status: "pending".to_string(),
                            current: 0,
                            total: 0,
                            error_msg: None,
                            cli_config: Some(task_cli.clone()),
                            start_time: None,
                            end_time: None,
                            size: None,
                            eta: None,
                            created_at: Some(
                                SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap()
                                    .as_millis() as u64,
                            ),
                            output_path: None,
                            is_hidden: false,
                            active_phase: Some(if task_cli.analysis_only {
                                "analysis".to_string()
                            } else {
                                "synthesis".to_string()
                            }),
                            analysis_phase: if task_cli.analysis_only {
                                Some(TaskPhaseState {
                                    status: "pending".to_string(),
                                    ..Default::default()
                                })
                            } else {
                                None
                            },
                            synthesis_phase: if task_cli.analysis_only {
                                None
                            } else {
                                Some(TaskPhaseState {
                                    status: "pending".to_string(),
                                    ..Default::default()
                                })
                            },
                        },
                    );
                }
            }
            save_tasks_to_disk(&*tasks_lock).await;
        }

        let client = match ApiClient::new(api_url) {
            Ok(c) => c,
            Err(e) => {
                log_error(&tx, format!("批量转换失败：无法创建 API 客户端: {}", e));
                let _ = tx.send("__STATUS__:DONE".to_string());
                return;
            }
        };

        for file_path in files_to_process {
            let tx_clone = tx.clone();
            let tasks_clone = tasks_state.clone();
            let file_id = if cli.analysis_only {
                analysis_task_id(&file_path)
            } else {
                synthesis_task_id(&file_path)
            };

            // 检查任务状态，如果是 completed 或 cancelled 则跳过
            let task_cli = {
                let tasks = tasks_state.lock().await;
                tasks
                    .get(&file_id)
                    .and_then(|t| t.cli_config.clone())
                    .unwrap_or_else(|| cli.clone())
            };
            {
                let tasks = tasks_state.lock().await;
                if let Some(t) = tasks.get(&file_id) {
                    if matches!(
                        t.status.as_str(),
                        "completed" | "cancelled" | "processing" | "paused"
                    ) {
                        log_info(
                            &tx,
                            format!(
                                "跳过任务 ({}): {:?}",
                                t.status,
                                file_path.file_name().unwrap_or_default()
                            ),
                        );
                        continue;
                    }
                }
            }

            // 更新状态为 processing
            {
                let mut tasks = tasks_clone.lock().await;
                if let Some(task) = tasks.get_mut(&file_id) {
                    task.status = "processing".to_string();
                    task.start_time = Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    );
                    task.end_time = None;
                    task.size = None;
                }
            }
            save_tasks_to_disk(&*tasks_state.lock().await).await;

            let file_id_cb = file_id.clone();
            let callback = move |event: process::ProcessEvent| {
                match event {
                    process::ProcessEvent::Log(msg) => log_info(&tx_clone, msg),
                    process::ProcessEvent::Progress { current, total } => {
                        let tasks = tasks_clone.clone();
                        let fid = file_id_cb.clone();
                        // 异步更新状态
                        tokio::spawn(async move {
                            let mut lock = tasks.lock().await;
                            if let Some(task) = lock.get_mut(&fid) {
                                task.current = current;
                                task.total = total;
                                // 计算 ETA
                                if let Some(start) = task.start_time {
                                    let now = SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs();
                                    let elapsed = now.saturating_sub(start);
                                    if elapsed > 0 && current > 0 {
                                        let rate = current as f64 / elapsed as f64; // 章节/秒
                                        let remaining = total.saturating_sub(current);
                                        task.eta = Some((remaining as f64 / rate) as u64);
                                    }
                                }
                            }
                        });
                    }
                    process::ProcessEvent::Success { size, output_path } => {
                        let tasks = tasks_clone.clone();
                        let f_id = file_id_cb.clone();
                        tokio::spawn(async move {
                            let mut lock = tasks.lock().await;
                            if let Some(task) = lock.get_mut(&f_id) {
                                task.size = Some(size);
                                task.output_path = Some(output_path);
                            }
                            save_tasks_to_disk(&*lock).await;
                        });
                    }
                }
            };

            let tasks_clone_cancel = tasks_state.clone();
            let fid_cancel = file_id.clone();
            let check_cancel = move || {
                let tasks = tasks_clone_cancel.clone();
                let fid = fid_cancel.clone();
                async move { task_cancelled_or_missing(tasks, fid).await }
            };

            log_info(
                &tx,
                format!(
                    "▶️ 开始处理文件: {:?}",
                    file_path.file_name().unwrap_or_default()
                ),
            );

            let file_path_str = file_path.to_string_lossy().to_string();
            let alloc_table = if crate::ai::should_use_ai(&task_cli.ai_dialogue) {
                let table =
                    load_allocation_table(&file_path_str).unwrap_or_else(|| VoiceAllocationTable {
                        schema_version: 2,
                        file_path: file_path_str.clone(),
                        novel_title: String::new(),
                        entries: vec![],
                        generated_at: 0,
                    });
                Some(Arc::new(std::sync::Mutex::new(table)))
            } else {
                None
            };

            let wait_if_paused = {
                let tasks = tasks_state.clone();
                let fid = file_id.clone();
                move || {
                    let tasks = tasks.clone();
                    let fid = fid.clone();
                    async move { wait_task_if_paused(tasks, fid).await }
                }
            };
            let result = if task_cli.analysis_only {
                process::analyze_file_dialogues(
                    &file_path,
                    &task_cli,
                    &None,
                    callback,
                    check_cancel,
                    wait_if_paused,
                    alloc_table.clone(),
                )
                .await
            } else {
                process::process_file(
                    &file_path,
                    &task_cli,
                    &client,
                    &None,
                    callback,
                    check_cancel,
                    wait_if_paused,
                    alloc_table.clone(),
                )
                .await
            };
            match result {
                Ok(_) => {
                    if let Some(ref at) = alloc_table {
                        save_allocation_table(&at.lock().unwrap());
                    }
                    update_task_status_by_id(&tasks_state, &file_id, "completed", None).await;
                }
                Err(e) => {
                    if e.to_string() == "任务已取消" {
                        update_task_status_by_id(&tasks_state, &file_id, "cancelled", None).await;
                    } else {
                        update_task_status_by_id(
                            &tasks_state,
                            &file_id,
                            "error",
                            Some(e.to_string()),
                        )
                        .await;
                    }
                }
            };
        }

        log_info(&tx, "所有文件处理完毕。".to_string());
        let _ = tx.send("__STATUS__:DONE".to_string());
    });

    remember_latest_task_handle(&state, task.abort_handle()).await;

    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: "批量转换任务已在后台启动，请查看下方实时日志...".to_string(),
        }),
    )
        .into_response()
}

async fn cancel_task_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CancelTaskRequest>,
) -> impl IntoResponse {
    let mut tasks = state.tasks.lock().await;
    if let Some(task) = tasks.get_mut(&req.id) {
        if task.status == "pending" || task.status == "processing" || task.status == "paused" {
            task.status = "cancelled".to_string();
            sync_active_phase(task);
            save_tasks_to_disk(&tasks).await;
        }
    }
    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: "任务已取消".to_string(),
        }),
    )
}

async fn pause_task_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CancelTaskRequest>,
) -> impl IntoResponse {
    let mut tasks = state.tasks.lock().await;
    let (should_pause, file_name) = {
        if let Some(task) = tasks.get_mut(&req.id) {
            if task.status == "processing" {
                task.status = "paused".to_string();
                task.eta = None;
                sync_active_phase(task);
                (true, task.file_name.clone())
            } else {
                (false, String::new())
            }
        } else {
            (false, String::new())
        }
    };
    if should_pause {
        save_tasks_to_disk(&tasks).await;
        let _ = state.tx.send(format!("⏸ 任务已暂停: {}", file_name));
    }
    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: "任务已暂停".to_string(),
        }),
    )
}

async fn resume_task_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CancelTaskRequest>,
) -> impl IntoResponse {
    let mut tasks = state.tasks.lock().await;
    let (should_resume, file_name) = {
        if let Some(task) = tasks.get_mut(&req.id) {
            if task.status == "paused" {
                task.status = "processing".to_string();
                sync_active_phase(task);
                (true, task.file_name.clone())
            } else {
                (false, String::new())
            }
        } else {
            (false, String::new())
        }
    };
    if should_resume {
        save_tasks_to_disk(&tasks).await;
        let _ = state.tx.send(format!("▶ 任务已恢复: {}", file_name));
    }
    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: "任务已恢复".to_string(),
        }),
    )
}

async fn pause_all_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut tasks = state.tasks.lock().await;
    let mut count = 0;
    for task in tasks.values_mut() {
        if task.status == "processing" {
            task.status = "paused".to_string();
            task.eta = None;
            sync_active_phase(task);
            count += 1;
        }
    }
    if count > 0 {
        save_tasks_to_disk(&tasks).await;
        let _ = state.tx.send(format!("⏸ 已暂停 {} 个任务", count));
    }
    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: format!("已暂停 {} 个任务", count),
        }),
    )
}

async fn resume_all_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut tasks = state.tasks.lock().await;
    let mut count = 0;
    for task in tasks.values_mut() {
        if task.status == "paused" {
            task.status = "processing".to_string();
            sync_active_phase(task);
            count += 1;
        }
    }
    if count > 0 {
        save_tasks_to_disk(&tasks).await;
        let _ = state.tx.send(format!("▶ 已恢复 {} 个任务", count));
    }
    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: format!("已恢复 {} 个任务", count),
        }),
    )
}

async fn start_synthesis_from_task_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StartSynthesisFromTaskRequest>,
) -> impl IntoResponse {
    let source_task = {
        let tasks = state.tasks.lock().await;
        tasks.get(&req.id).cloned()
    };

    let Some(source_task) = source_task else {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiResponse {
                success: false,
                message: "任务不存在".to_string(),
            }),
        )
            .into_response();
    };

    let is_analysis_task = source_task
        .cli_config
        .as_ref()
        .map(|cli| cli.analysis_only)
        .unwrap_or(false)
        || source_task.file_name.starts_with("分析: ");
    if !is_analysis_task || !matches!(source_task.status.as_str(), "completed" | "paused") {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: "只能从已完成或已暂停的分析任务开始合成".to_string(),
            }),
        )
            .into_response();
    }

    let Some(full_path) = source_task.full_path.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: "分析任务缺少源文件路径".to_string(),
            }),
        )
            .into_response();
    };
    let path = PathBuf::from(full_path);
    if !path.exists() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: "源文件不存在，无法开始合成".to_string(),
            }),
        )
            .into_response();
    }

    let Some(mut cli) = source_task.cli_config.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: "分析任务缺少合成配置".to_string(),
            }),
        )
            .into_response();
    };
    cli.analysis_only = false;
    cli.output_name = Some(
        source_task
            .file_name
            .trim_start_matches("分析: ")
            .to_string(),
    );
    if cli.voice.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: "分析任务缺少发音人配置，无法开始合成".to_string(),
            }),
        )
            .into_response();
    }
    if cli.api.is_none() {
        let data = state.initial_data.lock().await;
        cli.api = data.api_url.clone();
    }
    let Some(api_url) = cli.api.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: "缺少 MultiTTS API 地址，无法开始合成".to_string(),
            }),
        )
            .into_response();
    };
    let client = match ApiClient::new(api_url) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse {
                    success: false,
                    message: e.to_string(),
                }),
            )
                .into_response();
        }
    };

    let require_existing_analysis = source_task.status == "paused";
    let file_id = source_task.id.clone();
    {
        let mut tasks = state.tasks.lock().await;
        tasks.insert(
            file_id.clone(),
            TaskState {
                id: file_id.clone(),
                file_name: cli
                    .output_name
                    .clone()
                    .unwrap_or_else(|| fallback_task_title(&path)),
                full_path: Some(path.to_string_lossy().to_string()),
                status: "processing".to_string(),
                current: 0,
                total: 0,
                error_msg: None,
                cli_config: Some(cli.clone()),
                start_time: Some(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                ),
                end_time: None,
                size: None,
                eta: None,
                created_at: Some(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64,
                ),
                output_path: None,
                is_hidden: false,
                active_phase: Some("synthesis".to_string()),
                analysis_phase: source_task.analysis_phase.clone().or_else(|| {
                    Some(task_phase_from_task(
                        &source_task,
                        if source_task.status == "completed" {
                            "completed"
                        } else {
                            "paused"
                        },
                    ))
                }),
                synthesis_phase: Some(TaskPhaseState {
                    status: "processing".to_string(),
                    start_time: Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    ),
                    ..Default::default()
                }),
            },
        );
        save_tasks_to_disk(&*tasks).await;
    }

    let tx = state.tx.clone();
    let tasks_state = state.tasks.clone();
    let fid_cb = file_id.clone();
    let fid_control = file_id.clone();
    let task = tokio::spawn(async move {
        let callback_tx = tx.clone();
        let tasks_clone = tasks_state.clone();
        let callback = move |event: process::ProcessEvent| match event {
            process::ProcessEvent::Log(msg) => log_info(&callback_tx, msg),
            process::ProcessEvent::Progress { current, total } => {
                let tasks = tasks_clone.clone();
                let f_id = fid_cb.clone();
                tokio::spawn(async move {
                    let mut lock = tasks.lock().await;
                    if let Some(task) = lock.get_mut(&f_id) {
                        task.current = current;
                        task.total = total;
                        if let Some(start) = task.start_time {
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs();
                            let elapsed = now.saturating_sub(start);
                            if elapsed > 0 && current > 0 {
                                let rate = current as f64 / elapsed as f64;
                                let remaining = total.saturating_sub(current);
                                task.eta = Some((remaining as f64 / rate) as u64);
                            }
                        }
                    }
                });
            }
            process::ProcessEvent::Success { size, output_path } => {
                let tasks = tasks_clone.clone();
                let f_id = fid_cb.clone();
                tokio::spawn(async move {
                    let mut lock = tasks.lock().await;
                    if let Some(task) = lock.get_mut(&f_id) {
                        task.size = Some(size);
                        task.output_path = Some(output_path);
                    }
                    save_tasks_to_disk(&*lock).await;
                });
            }
        };

        log_info(&tx, format!("▶️ 从分析结果开始合成: {:?}", path));
        let alloc_table = create_live_allocation_table(&path, &cli.ai_dialogue);
        let check_cancel = {
            let tasks = tasks_state.clone();
            let fid = fid_control.clone();
            move || {
                let tasks = tasks.clone();
                let fid = fid.clone();
                async move { task_cancelled_or_missing(tasks, fid).await }
            }
        };
        let wait_if_paused = {
            let tasks = tasks_state.clone();
            let fid = fid_control.clone();
            move || {
                let tasks = tasks.clone();
                let fid = fid.clone();
                async move { wait_task_if_paused(tasks, fid).await }
            }
        };
        let result = if require_existing_analysis {
            process::process_file_with_existing_analysis(
                &path,
                &cli,
                &client,
                &None,
                callback,
                check_cancel,
                wait_if_paused,
                alloc_table.clone(),
            )
            .await
        } else {
            process::process_file(
                &path,
                &cli,
                &client,
                &None,
                callback,
                check_cancel,
                wait_if_paused,
                alloc_table.clone(),
            )
            .await
        };
        match result {
            Ok(_) => {
                if let Some(ref at) = alloc_table {
                    save_allocation_table(&at.lock().unwrap());
                }
                update_task_status_by_id(&tasks_state, &fid_control, "completed", None).await;
            }
            Err(e) => {
                let message = e.to_string();
                if e.to_string() == "任务已取消" {
                    update_task_status_by_id(&tasks_state, &fid_control, "cancelled", None).await;
                } else if require_existing_analysis
                    && (message.contains("继续分析") || message.contains("未分析"))
                {
                    update_task_status_by_id(&tasks_state, &fid_control, "paused", Some(message))
                        .await;
                } else {
                    update_task_status_by_id(&tasks_state, &fid_control, "error", Some(message))
                        .await;
                }
            }
        }
        let _ = tx.send("__STATUS__:DONE".to_string());
    });

    remember_latest_task_handle(&state, task.abort_handle()).await;

    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: "合成任务已从分析结果启动".to_string(),
        }),
    )
        .into_response()
}

async fn retry_task_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CancelTaskRequest>, // 复用 CancelTaskRequest (只包含 id)
) -> impl IntoResponse {
    let tasks = state.tasks.lock().await;
    let task_opt = tasks.get(&req.id).cloned();
    drop(tasks); // 释放锁

    if let Some(task) = task_opt {
        if let (Some(cli), Some(full_path)) = (task.cli_config, task.full_path) {
            let path = PathBuf::from(&full_path);
            if !path.exists() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ApiResponse {
                        success: false,
                        message: "原文件不存在，无法重试".to_string(),
                    }),
                )
                    .into_response();
            }

            let client = if cli.analysis_only {
                None
            } else {
                match ApiClient::new(cli.api.clone().unwrap_or_default()) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(ApiResponse {
                                success: false,
                                message: e.to_string(),
                            }),
                        )
                            .into_response();
                    }
                }
            };

            let tx = state.tx.clone();
            let tasks_state = state.tasks.clone();
            let file_id = req.id.clone();

            // 重置任务状态
            {
                let mut tasks = tasks_state.lock().await;
                if let Some(t) = tasks.get_mut(&file_id) {
                    t.status = "processing".to_string();
                    t.error_msg = None;
                    t.current = 0;
                    t.start_time = Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    );
                    t.end_time = None;
                    t.size = None;
                    t.eta = None;
                }
                save_tasks_to_disk(&tasks).await;
            }

            // 启动后台任务
            tokio::spawn(async move {
                let tx_clone = tx.clone();
                let tasks_clone = tasks_state.clone();
                let fid = file_id.clone();
                let fid_cb = fid.clone();

                let callback = move |event: process::ProcessEvent| {
                    match event {
                        process::ProcessEvent::Log(msg) => log_info(&tx_clone, msg),
                        process::ProcessEvent::Progress { current, total } => {
                            let tasks = tasks_clone.clone();
                            let f_id = fid_cb.clone();
                            tokio::spawn(async move {
                                let mut lock = tasks.lock().await;
                                if let Some(task) = lock.get_mut(&f_id) {
                                    task.current = current;
                                    task.total = total;
                                    // 计算 ETA
                                    if let Some(start) = task.start_time {
                                        let now = SystemTime::now()
                                            .duration_since(UNIX_EPOCH)
                                            .unwrap()
                                            .as_secs();
                                        let elapsed = now.saturating_sub(start);
                                        if elapsed > 0 && current > 0 {
                                            let rate = current as f64 / elapsed as f64;
                                            let remaining = total.saturating_sub(current);
                                            task.eta = Some((remaining as f64 / rate) as u64);
                                        }
                                    }
                                }
                            });
                        }
                        process::ProcessEvent::Success { size, output_path } => {
                            let tasks = tasks_clone.clone();
                            let f_id = fid_cb.clone();
                            tokio::spawn(async move {
                                let mut lock = tasks.lock().await;
                                if let Some(task) = lock.get_mut(&f_id) {
                                    task.size = Some(size);
                                    task.output_path = Some(output_path);
                                }
                                save_tasks_to_disk(&*lock).await;
                            });
                        }
                    }
                };

                let tasks_clone_cancel = tasks_state.clone();
                let fid_cancel = fid.clone();
                let check_cancel = move || {
                    let tasks = tasks_clone_cancel.clone();
                    let fid = fid_cancel.clone();
                    async move { task_cancelled_or_missing(tasks, fid).await }
                };

                log_info(
                    &tx,
                    format!("▶️ 重试任务: {:?}", path.file_name().unwrap_or_default()),
                );
                let wait_if_paused = {
                    let tasks = tasks_state.clone();
                    let fid = fid.clone();
                    move || {
                        let tasks = tasks.clone();
                        let fid = fid.clone();
                        async move { wait_task_if_paused(tasks, fid).await }
                    }
                };
                let alloc_table = create_live_allocation_table(&path, &cli.ai_dialogue);
                let result = if cli.analysis_only {
                    process::analyze_file_dialogues(
                        &path,
                        &cli,
                        &None,
                        callback,
                        check_cancel,
                        wait_if_paused,
                        alloc_table.clone(),
                    )
                    .await
                } else {
                    process::process_file(
                        &path,
                        &cli,
                        client
                            .as_ref()
                            .expect("client is present for synthesis retry"),
                        &None,
                        callback,
                        check_cancel,
                        wait_if_paused,
                        alloc_table.clone(),
                    )
                    .await
                };
                match result {
                    Ok(_) => {
                        if let Some(ref at) = alloc_table {
                            save_allocation_table(&at.lock().unwrap());
                        }
                        update_task_status_by_id(&tasks_state, &fid, "completed", None).await;
                    }
                    Err(e) => {
                        if e.to_string() == "任务已取消" {
                            update_task_status_by_id(&tasks_state, &fid, "cancelled", None).await;
                        } else {
                            update_task_status_by_id(
                                &tasks_state,
                                &fid,
                                "error",
                                Some(e.to_string()),
                            )
                            .await;
                        }
                    }
                };
            });

            return (
                StatusCode::OK,
                Json(ApiResponse {
                    success: true,
                    message: "任务已开始重试".to_string(),
                }),
            )
                .into_response();
        }
    }
    (
        StatusCode::BAD_REQUEST,
        Json(ApiResponse {
            success: false,
            message: "任务无效或缺少配置信息".to_string(),
        }),
    )
        .into_response()
}

async fn retry_all_failed_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let tasks_map = state.tasks.lock().await;
    let failed_tasks: Vec<(String, String, Cli)> = tasks_map
        .values()
        .filter(|t| t.status == "error" || t.status == "cancelled" || t.status == "paused")
        .filter_map(|t| {
            if let (Some(path), Some(config)) = (&t.full_path, &t.cli_config) {
                Some((t.id.clone(), path.clone(), config.clone()))
            } else {
                None
            }
        })
        .collect();
    drop(tasks_map); // 释放锁

    if failed_tasks.is_empty() {
        return (
            StatusCode::OK,
            Json(ApiResponse {
                success: true,
                message: "没有需要重试的任务".to_string(),
            }),
        )
            .into_response();
    }

    let tx = state.tx.clone();
    let tasks_state = state.tasks.clone();

    // 启动后台任务依次处理
    tokio::spawn(async move {
        log_info(
            &tx,
            format!("开始重试 {} 个失败/已取消的任务...", failed_tasks.len()),
        );

        for (id, full_path, cli) in failed_tasks {
            let path = PathBuf::from(&full_path);
            if !path.exists() {
                log_error(&tx, format!("文件不存在，跳过: {:?}", path));
                continue;
            }

            let client = match ApiClient::new(cli.api.clone().unwrap_or_default()) {
                Ok(c) => c,
                Err(e) => {
                    log_error(&tx, format!("创建客户端失败: {}", e));
                    continue;
                }
            };

            // 更新状态为 processing
            {
                let mut tasks = tasks_state.lock().await;
                if let Some(t) = tasks.get_mut(&id) {
                    t.status = "processing".to_string();
                    t.error_msg = None;
                    t.current = 0;
                    t.start_time = Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    );
                    t.end_time = None;
                    t.size = None;
                    t.eta = None;
                }
                save_tasks_to_disk(&tasks).await;
            }

            let tx_clone = tx.clone();
            let tasks_clone = tasks_state.clone();
            let fid = id.clone();
            let fid_cb = fid.clone();

            let callback = move |event: process::ProcessEvent| {
                match event {
                    process::ProcessEvent::Log(msg) => log_info(&tx_clone, msg),
                    process::ProcessEvent::Progress { current, total } => {
                        let tasks = tasks_clone.clone();
                        let f_id = fid_cb.clone();
                        tokio::spawn(async move {
                            let mut lock = tasks.lock().await;
                            if let Some(task) = lock.get_mut(&f_id) {
                                task.current = current;
                                task.total = total;
                                // 计算 ETA
                                if let Some(start) = task.start_time {
                                    let now = SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs();
                                    let elapsed = now.saturating_sub(start);
                                    if elapsed > 0 && current > 0 {
                                        let rate = current as f64 / elapsed as f64;
                                        let remaining = total.saturating_sub(current);
                                        task.eta = Some((remaining as f64 / rate) as u64);
                                    }
                                }
                            }
                        });
                    }
                    process::ProcessEvent::Success { size, output_path } => {
                        let tasks = tasks_clone.clone();
                        let f_id = fid_cb.clone();
                        tokio::spawn(async move {
                            let mut lock = tasks.lock().await;
                            if let Some(task) = lock.get_mut(&f_id) {
                                task.size = Some(size);
                                task.output_path = Some(output_path);
                            }
                            save_tasks_to_disk(&*lock).await;
                        });
                    }
                }
            };

            let tasks_clone_cancel = tasks_state.clone();
            let fid_cancel = fid.clone();
            let check_cancel = move || {
                let tasks = tasks_clone_cancel.clone();
                let fid = fid_cancel.clone();
                async move { task_cancelled_or_missing(tasks, fid).await }
            };

            log_info(
                &tx,
                format!("▶️ 重试任务: {:?}", path.file_name().unwrap_or_default()),
            );
            let wait_if_paused = {
                let tasks = tasks_state.clone();
                let fid = fid.clone();
                move || {
                    let tasks = tasks.clone();
                    let fid = fid.clone();
                    async move { wait_task_if_paused(tasks, fid).await }
                }
            };
            let alloc_table = create_live_allocation_table(&path, &cli.ai_dialogue);
            match process::process_file(
                &path,
                &cli,
                &client,
                &None,
                callback,
                check_cancel,
                wait_if_paused,
                alloc_table.clone(),
            )
            .await
            {
                Ok(_) => {
                    if let Some(ref at) = alloc_table {
                        save_allocation_table(&at.lock().unwrap());
                    }
                    update_task_status(&tasks_state, &path, "completed", None).await;
                }
                Err(e) => {
                    if e.to_string() == "任务已取消" {
                        update_task_status(&tasks_state, &path, "cancelled", None).await;
                    } else {
                        update_task_status(&tasks_state, &path, "error", Some(e.to_string())).await;
                    }
                }
            };
        }
        log_info(&tx, "重试队列处理完毕。".to_string());
    });

    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: "已启动后台重试任务".to_string(),
        }),
    )
        .into_response()
}

async fn clear_completed_tasks_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut tasks = state.tasks.lock().await;
    for task in tasks.values_mut() {
        if task.status == "completed" {
            // 标记隐藏，前端不再显示，但保留记录
            task.is_hidden = true;
        }
    }
    save_tasks_to_disk(&tasks).await;
    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: "已清除已完成任务".to_string(),
        }),
    )
}

fn output_root_path() -> PathBuf {
    if PathBuf::from("/output").exists() {
        PathBuf::from("/output")
    } else {
        PathBuf::from("output")
    }
}

fn book_root_path() -> PathBuf {
    if PathBuf::from("/book").exists() {
        PathBuf::from("/book")
    } else {
        PathBuf::from("book")
    }
}

async fn task_cancelled_or_missing(
    tasks: Arc<Mutex<HashMap<String, TaskState>>>,
    fid: String,
) -> bool {
    let tasks = tasks.lock().await;
    match tasks.get(&fid) {
        Some(t) => t.status == "cancelled",
        None => true,
    }
}

async fn wait_task_if_paused(tasks: Arc<Mutex<HashMap<String, TaskState>>>, fid: String) {
    loop {
        let paused = {
            let ts = tasks.lock().await;
            ts.get(&fid).map(|t| t.status == "paused").unwrap_or(false)
        };
        if !paused {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn task_output_path(task: &TaskState) -> Option<String> {
    if let Some(path) = task.output_path.as_deref().filter(|p| !p.trim().is_empty()) {
        return Some(path.to_string());
    }

    let cli = task.cli_config.as_ref()?;
    let output_name = cli
        .output_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())?;
    Some(
        cli.out
            .join(crate::utils::sanitize_filename(output_name))
            .to_string_lossy()
            .to_string(),
    )
}

fn delete_output_path(path_str: &str) -> io::Result<bool> {
    let candidate = PathBuf::from(path_str);
    if !candidate.exists() {
        return Ok(false);
    }

    let root = output_root_path();
    if !root.exists() {
        return Ok(false);
    }

    let root = root.canonicalize()?;
    let candidate = candidate.canonicalize()?;
    if candidate == root || !candidate.starts_with(&root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "output path is outside the output directory",
        ));
    }

    if candidate.is_dir() {
        std::fs::remove_dir_all(candidate)?;
    } else {
        std::fs::remove_file(candidate)?;
    }
    Ok(true)
}

fn delete_source_file(path_str: &str) -> io::Result<bool> {
    let candidate = PathBuf::from(path_str);
    if !candidate.exists() {
        return Ok(false);
    }

    let candidate = candidate.canonicalize()?;
    if !candidate.is_file() {
        return Ok(false);
    }

    let book_root = book_root_path();
    if !book_root.exists() {
        return Ok(false);
    }
    let book_root = book_root.canonicalize()?;
    if !candidate.starts_with(&book_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "source file is outside the book directory",
        ));
    }

    let output_root = output_root_path();
    if output_root.exists() {
        let output_root = output_root.canonicalize()?;
        if candidate.starts_with(&output_root) {
            return Ok(false);
        }
    }

    std::fs::remove_file(candidate)?;
    Ok(true)
}

async fn delete_tasks_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DeleteTasksRequest>,
) -> impl IntoResponse {
    let delete_allocations = req.delete_allocations.unwrap_or_else(|| {
        load_server_config()
            .delete_task_allocations
            .unwrap_or(false)
    });
    let delete_outputs = req
        .delete_outputs
        .unwrap_or_else(|| load_server_config().delete_task_outputs.unwrap_or(false));
    let delete_sources = req
        .delete_sources
        .unwrap_or_else(|| load_server_config().delete_task_sources.unwrap_or(false));

    let delete_ids = req.ids;
    let mut selected_tasks = Vec::new();
    let mut cancelled_running_task = false;
    {
        let mut tasks = state.tasks.lock().await;
        for id in &delete_ids {
            if let Some(task) = tasks.get_mut(id) {
                let was_running = task.status == "processing" || task.status == "paused";
                if task.status == "pending"
                    || task.status == "processing"
                    || task.status == "paused"
                {
                    task.status = "cancelled".to_string();
                    task.end_time = Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    );
                    task.eta = None;
                    sync_active_phase(task);
                    cancelled_running_task |= was_running;
                }
                selected_tasks.push(task.clone());
            }
        }
        save_tasks_to_disk(&tasks).await;
    }
    if cancelled_running_task && (delete_outputs || delete_sources) {
        tokio::time::sleep(std::time::Duration::from_millis(750)).await;
    }

    let mut allocation_paths = Vec::new();
    let mut output_paths = Vec::new();
    let mut source_paths = Vec::new();
    let mut seen_allocation_paths = HashSet::new();
    let mut seen_output_paths = HashSet::new();
    let mut seen_source_paths = HashSet::new();
    for task in &selected_tasks {
        if delete_sources {
            if let Some(path) = task.full_path.as_deref().filter(|p| !p.trim().is_empty()) {
                let normalized = path.replace('\\', "/");
                if seen_source_paths.insert(normalized) {
                    source_paths.push(path.to_string());
                }
            }
        }
        if delete_outputs {
            if let Some(path) = task_output_path(task) {
                let normalized = path.replace('\\', "/");
                if seen_output_paths.insert(normalized) {
                    output_paths.push(path);
                }
            }
        }
        if delete_allocations {
            if let Some(path) = task.full_path.as_deref() {
                let normalized = path.replace('\\', "/");
                if seen_allocation_paths.insert(normalized) {
                    allocation_paths.push(path.to_string());
                }
            }
        }
    }

    let mut deleted_allocations = 0;
    let mut deleted_allocation_paths = Vec::new();
    let mut failed_allocation_paths = Vec::new();
    for path in allocation_paths {
        match delete_allocation_files_for_path(&path) {
            Ok(count) => {
                deleted_allocations += count;
                if count > 0 {
                    deleted_allocation_paths.push(path);
                }
            }
            Err(_) => failed_allocation_paths.push(path),
        }
    }
    let mut deleted_outputs = 0;
    let mut deleted_output_paths = Vec::new();
    let mut failed_output_paths = Vec::new();
    for path in output_paths {
        match delete_output_path(&path) {
            Ok(true) => {
                deleted_outputs += 1;
                deleted_output_paths.push(path);
            }
            Ok(false) => {}
            Err(_) => failed_output_paths.push(path),
        }
    }
    let mut deleted_sources = 0;
    let mut deleted_source_paths = Vec::new();
    let mut failed_source_paths = Vec::new();
    for path in source_paths {
        match delete_source_file(&path) {
            Ok(true) => {
                deleted_sources += 1;
                deleted_source_paths.push(path);
            }
            Ok(false) => {}
            Err(_) => failed_source_paths.push(path),
        }
    }

    let mut deleted_task_ids = Vec::new();
    {
        let mut tasks = state.tasks.lock().await;
        for id in delete_ids {
            if tasks.remove(&id).is_some() {
                deleted_task_ids.push(id);
            }
        }
        save_tasks_to_disk(&tasks).await;
    }

    let deleted = deleted_task_ids.len();
    let message = if delete_allocations || delete_outputs || delete_sources {
        let mut parts = vec![format!("已删除 {} 个任务记录", deleted)];
        if delete_allocations {
            parts.push(format!("{} 张分配表", deleted_allocations));
        }
        if delete_outputs {
            parts.push(format!("{} 个输出路径", deleted_outputs));
        }
        if delete_sources {
            parts.push(format!("{} 个源文件", deleted_sources));
        }
        let failed =
            failed_allocation_paths.len() + failed_output_paths.len() + failed_source_paths.len();
        if failed > 0 {
            parts.push(format!("{} 项删除失败", failed));
        }
        parts.join("，")
    } else if delete_allocations {
        if !failed_allocation_paths.is_empty() {
            format!(
                "已删除 {} 个任务记录，{} 张分配表，{} 个分配表删除失败",
                deleted,
                deleted_allocations,
                failed_allocation_paths.len()
            )
        } else {
            format!(
                "已删除 {} 个任务记录，{} 张分配表",
                deleted, deleted_allocations
            )
        }
    } else {
        format!("已删除 {} 个任务记录", deleted)
    };

    (
        StatusCode::OK,
        Json(DeleteTasksResponse {
            success: true,
            message,
            deleted_task_ids,
            deleted_allocation_paths,
            deleted_output_paths,
            deleted_source_paths,
            failed_allocation_paths,
            failed_output_paths,
            failed_source_paths,
            deleted_tasks: deleted,
            deleted_allocations,
            deleted_outputs,
            deleted_sources,
        }),
    )
}

async fn reset_history_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut tasks = state.tasks.lock().await;
    tasks.clear();
    save_tasks_to_disk(&tasks).await;
    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: "已彻底重置所有任务历史".to_string(),
        }),
    )
}

// --- 文件管理相关 Handler ---

fn get_safe_path(root: &str, sub_path: Option<&str>) -> Option<PathBuf> {
    let base = match root {
        "book" => {
            if PathBuf::from("/book").exists() {
                PathBuf::from("/book")
            } else {
                PathBuf::from("book")
            }
        }
        "output" => {
            if PathBuf::from("/output").exists() {
                PathBuf::from("/output")
            } else {
                PathBuf::from("output")
            }
        }
        _ => return None,
    };

    let sub = sub_path.unwrap_or("");
    let path = std::path::Path::new(sub);

    // 防止路径遍历
    for component in path.components() {
        if let Component::ParentDir = component {
            return None;
        }
    }

    let clean_sub = sub.trim_start_matches(|c| c == '/' || c == '\\');
    let target = base.join(clean_sub);

    // 简单检查
    if target.starts_with(&base) {
        Some(target)
    } else {
        None
    }
}

fn resolve_input_file_path(path_str: &str) -> PathBuf {
    let path = PathBuf::from(path_str);
    if path.exists() {
        return path;
    }

    let normalized = path_str.replace('\\', "/");
    if let Some(rest) = normalized.strip_prefix("book/") {
        let book_root = if PathBuf::from("/book").exists() {
            PathBuf::from("/book")
        } else {
            PathBuf::from("book")
        };
        let candidate = book_root.join(rest);
        if candidate.exists() {
            return candidate;
        }
    }

    path
}

async fn list_files_handler(Query(req): Query<ListFilesQuery>) -> impl IntoResponse {
    let path = match get_safe_path(&req.root, req.path.as_deref()) {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, Json::<Vec<FileEntry>>(vec![])).into_response(),
    };

    if !path.exists() || !path.is_dir() {
        return (StatusCode::NOT_FOUND, Json::<Vec<FileEntry>>(vec![])).into_response();
    }

    let mut entries = Vec::new();
    if let Ok(mut read_dir) = tokio::fs::read_dir(path).await {
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            if let Ok(meta) = entry.metadata().await {
                let modified = meta
                    .modified()
                    .unwrap_or(UNIX_EPOCH)
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                entries.push(FileEntry {
                    name: entry.file_name().to_string_lossy().to_string(),
                    is_dir: meta.is_dir(),
                    size: meta.len(),
                    modified,
                });
            }
        }
    }

    // 排序：文件夹在前，然后按文件名
    entries.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            b.is_dir.cmp(&a.is_dir)
        } else {
            a.name.cmp(&b.name)
        }
    });

    (StatusCode::OK, Json(entries)).into_response()
}

async fn delete_file_handler(Json(req): Json<FileActionRequest>) -> impl IntoResponse {
    let path = match get_safe_path(&req.root, Some(&req.path)) {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse {
                    success: false,
                    message: "非法路径".to_string(),
                }),
            )
                .into_response();
        }
    };

    if path.is_dir() {
        if let Err(e) = tokio::fs::remove_dir_all(path).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse {
                    success: false,
                    message: format!("删除目录失败: {}", e),
                }),
            )
                .into_response();
        }
    } else {
        if let Err(e) = tokio::fs::remove_file(path).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse {
                    success: false,
                    message: format!("删除文件失败: {}", e),
                }),
            )
                .into_response();
        }
    }

    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: "删除成功".to_string(),
        }),
    )
        .into_response()
}

fn parse_range_header(value: &str, len: usize) -> Option<(usize, usize)> {
    let range = value.strip_prefix("bytes=")?;
    let first = range.split(',').next()?.trim();
    let (start_raw, end_raw) = first.split_once('-')?;

    if start_raw.is_empty() {
        let suffix_len = end_raw.parse::<usize>().ok()?;
        if suffix_len == 0 || len == 0 {
            return None;
        }
        let start = len.saturating_sub(suffix_len);
        return Some((start, len - 1));
    }

    let start = start_raw.parse::<usize>().ok()?;
    if start >= len {
        return None;
    }

    let end = if end_raw.is_empty() {
        len - 1
    } else {
        end_raw.parse::<usize>().ok()?.min(len - 1)
    };
    if end < start {
        return None;
    }

    Some((start, end))
}

async fn download_file_handler(
    headers: HeaderMap,
    Query(req): Query<FileActionRequest>,
) -> impl IntoResponse {
    let path = match get_safe_path(&req.root, Some(&req.path)) {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, "非法路径".to_string()).into_response(),
    };

    if !path.exists() || !path.is_file() {
        return (StatusCode::NOT_FOUND, "文件不存在".to_string()).into_response();
    }

    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let filename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let content_type = mime_guess::from_path(&path)
                .first_or_octet_stream()
                .to_string();
            let len = bytes.len();
            let disposition = if req.inline.unwrap_or(false) {
                format!("inline; filename=\"{}\"", filename)
            } else {
                format!("attachment; filename=\"{}\"", filename)
            };

            if let Some(range_header) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
                if let Some((start, end)) = parse_range_header(range_header, len) {
                    let partial = bytes[start..=end].to_vec();
                    return (
                        StatusCode::PARTIAL_CONTENT,
                        [
                            ("Content-Type", content_type),
                            ("Content-Disposition", disposition),
                            ("Accept-Ranges", "bytes".to_string()),
                            ("Content-Range", format!("bytes {}-{}/{}", start, end, len)),
                            ("Content-Length", partial.len().to_string()),
                        ],
                        partial,
                    )
                        .into_response();
                }

                return (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [
                        ("Accept-Ranges", "bytes".to_string()),
                        ("Content-Range", format!("bytes */{}", len)),
                    ],
                    "范围无效".to_string(),
                )
                    .into_response();
            }

            (
                StatusCode::OK,
                [
                    ("Content-Type", content_type),
                    ("Content-Disposition", disposition),
                    ("Accept-Ranges", "bytes".to_string()),
                    ("Content-Length", len.to_string()),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("读取文件失败: {}", e),
        )
            .into_response(),
    }
}

async fn upload_file_manager_handler(mut multipart: Multipart) -> impl IntoResponse {
    let mut target_dir_str = "book".to_string();
    let mut sub_path = "".to_string();
    let mut file_saved = false;

    // 注意：这里简化处理，假设字段顺序或先读取到 file 后处理。
    // 实际上 Multipart 流式读取，建议前端把 file 放在最后，或者我们先缓存。
    // 为简单起见，我们遍历所有字段。

    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let name = field.name().unwrap_or("").to_string();
        if name == "root" {
            if let Ok(val) = field.text().await {
                target_dir_str = val;
            }
        } else if name == "path" {
            if let Ok(val) = field.text().await {
                sub_path = val;
            }
        } else if name == "file" {
            let file_name = field.file_name().unwrap_or("uploaded_file").to_string();
            let root_path = get_safe_path(&target_dir_str, Some(&sub_path))
                .unwrap_or_else(|| PathBuf::from("book"));

            if let Err(_) = tokio::fs::create_dir_all(&root_path).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiResponse {
                        success: false,
                        message: "无法创建目录".to_string(),
                    }),
                )
                    .into_response();
            }

            let file_path = root_path.join(&file_name);
            if let Ok(mut file) = File::create(&file_path).await {
                let mut stream = field;
                while let Some(chunk) = stream.chunk().await.unwrap_or(None) {
                    let _ = file.write_all(&chunk).await;
                }
                file_saved = true;
            }
        }
    }

    if file_saved {
        (
            StatusCode::OK,
            Json(ApiResponse {
                success: true,
                message: "上传成功".to_string(),
            }),
        )
            .into_response()
    } else {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: "上传失败".to_string(),
            }),
        )
            .into_response()
    }
}

async fn update_task_status(
    tasks: &Arc<Mutex<HashMap<String, TaskState>>>,
    path: &PathBuf,
    status: &str,
    err: Option<String>,
) {
    let id = synthesis_task_id(path);
    update_task_status_by_id(tasks, &id, status, err).await;
}

async fn update_task_status_by_id(
    tasks: &Arc<Mutex<HashMap<String, TaskState>>>,
    id: &str,
    status: &str,
    err: Option<String>,
) {
    let mut lock = tasks.lock().await;
    if let Some(task) = lock.get_mut(id) {
        let next_status = if task.status == "cancelled" && status == "error" {
            "cancelled"
        } else {
            status
        };
        task.status = next_status.to_string();
        if let Some(e) = err.filter(|_| next_status != "cancelled") {
            task.error_msg = Some(e);
        }
        if next_status == "completed" {
            task.current = task.total;
        }
        if next_status == "completed" || next_status == "error" || next_status == "cancelled" {
            task.end_time = Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            );
            task.eta = None;
        }
        sync_active_phase(task);
    }
    save_tasks_to_disk(&lock).await;
}

async fn save_tasks_to_disk(tasks: &HashMap<String, TaskState>) {
    let dir = data_dir();
    if !dir.exists() {
        let _ = tokio::fs::create_dir_all(&dir).await;
    }
    let path = dir.join("baitts_tasks.json");
    if let Ok(content) = serde_json::to_string_pretty(tasks) {
        let _ = tokio::fs::write(path, content).await;
    }
}

async fn load_tasks_from_disk() -> HashMap<String, TaskState> {
    let path = data_dir().join("baitts_tasks.json");
    if path.exists() {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            if let Ok(tasks) = serde_json::from_str(&content) {
                return tasks;
            }
        }
    }
    HashMap::new()
}

fn data_dir() -> PathBuf {
    if PathBuf::from("/data").exists() {
        PathBuf::from("/data")
    } else {
        PathBuf::from("data")
    }
}

fn load_ai_dialogue_config() -> AiDialogueConfig {
    let path = data_dir().join("ai_dialogue_config.json");
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(config) = serde_json::from_str(&content) {
                return config;
            }
        }
    }
    AiDialogueConfig::default()
}

fn save_ai_dialogue_config(config: &AiDialogueConfig) {
    let dir = data_dir();
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }
    let path = dir.join("ai_dialogue_config.json");
    if let Ok(content) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(path, content);
    }
}

fn allocations_dir() -> PathBuf {
    data_dir().join("allocations")
}

fn allocation_file_path(file_path: &str) -> PathBuf {
    let hash = format!("{:x}", md5::compute(file_path.as_bytes()));
    allocations_dir().join(format!("{}.json", hash))
}

fn find_allocation_file_path(file_path: &str) -> Option<PathBuf> {
    find_allocation_file_paths(file_path).into_iter().next()
}

fn find_allocation_file_paths(file_path: &str) -> Vec<PathBuf> {
    let mut matches = Vec::new();
    let primary = allocation_file_path(file_path);
    if primary.exists() {
        matches.push(primary.clone());
    }
    let target = file_path.replace('\\', "/");
    let dir = allocations_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return matches;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if matches.iter().any(|existing| existing == &path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(table) = serde_json::from_str::<VoiceAllocationTable>(&content) else {
            continue;
        };
        if table.file_path.replace('\\', "/") == target {
            matches.push(path);
        }
    }
    matches
}

fn load_allocation_table(file_path: &str) -> Option<VoiceAllocationTable> {
    let path = find_allocation_file_path(file_path)?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content)
        .ok()
        .map(normalize_allocation_table)
}

fn normalize_allocation_table(mut table: VoiceAllocationTable) -> VoiceAllocationTable {
    if table.schema_version == 0 {
        table.schema_version = 2;
    }
    table.normalize_legacy_text_fields();
    table
}

fn try_save_allocation_table(table: &VoiceAllocationTable) -> io::Result<()> {
    let dir = allocations_dir();
    std::fs::create_dir_all(&dir)?;
    let path = find_allocation_file_path(&table.file_path)
        .unwrap_or_else(|| allocation_file_path(&table.file_path));
    let table = normalize_allocation_table(table.clone());
    let content = serde_json::to_string_pretty(&table)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, content)?;
    for duplicate in find_allocation_file_paths(&table.file_path) {
        if duplicate != path {
            let _ = std::fs::remove_file(duplicate);
        }
    }
    Ok(())
}

fn save_allocation_table(table: &VoiceAllocationTable) {
    let _ = try_save_allocation_table(table);
}

fn analysis_task_id(path: &Path) -> String {
    format!(
        "{:x}",
        md5::compute(format!("analysis:{}", path.to_string_lossy()).as_bytes())
    )
}

fn synthesis_task_id(path: &Path) -> String {
    format!("{:x}", md5::compute(path.to_string_lossy().as_bytes()))
}

async fn remember_latest_task_handle(state: &Arc<AppState>, handle: AbortHandle) {
    let mut handle_lock = state.task_handle.lock().await;
    *handle_lock = Some(handle);
}

fn create_live_allocation_table(
    file_path: &std::path::Path,
    config: &AiDialogueConfig,
) -> Option<Arc<std::sync::Mutex<VoiceAllocationTable>>> {
    if !crate::ai::should_use_ai(config) {
        return None;
    }
    let file_path_str = file_path.to_string_lossy().to_string();
    let table = load_allocation_table(&file_path_str).unwrap_or_else(|| VoiceAllocationTable {
        schema_version: 2,
        file_path: file_path_str,
        novel_title: String::new(),
        entries: vec![],
        generated_at: 0,
    });
    Some(Arc::new(std::sync::Mutex::new(table)))
}

// 新增：预览 Handler
async fn preview_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TtsRequest>,
) -> impl IntoResponse {
    // 简单的参数校验
    if req.text_content.is_none() || req.text_content.as_ref().unwrap().trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: "预览文本不能为空".to_string(),
            }),
        )
            .into_response();
    }

    let client = match ApiClient::new(req.api_url.clone()) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse {
                    success: false,
                    message: format!("创建客户端失败: {}", e),
                }),
            )
                .into_response();
        }
    };

    let text = req.text_content.unwrap();

    // 1. 文本切分逻辑 (复用 process.rs 中的逻辑)
    let dialogue_regex = Regex::new(r"“[^”]*”|「[^」]*」").unwrap();
    let ignore_regex_str = req.ignore_regex.as_deref().unwrap_or(r"\*{3,}|#{2,}");
    let ignore_regex =
        Regex::new(ignore_regex_str).unwrap_or_else(|_| Regex::new(r"\*{3,}|#{2,}").unwrap());

    struct BatchData {
        text: String,
        is_dialogue: bool,
    }

    let mut batches: Vec<BatchData> = Vec::new();
    let mut current_batch = BatchData {
        text: String::new(),
        is_dialogue: false,
    };
    let mut is_batch_empty = true;
    const MAX_BATCH_CHARS: usize = 300;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let processed_line = ignore_regex.replace_all(trimmed, "").to_string();
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
                    is_dialogue,
                };
            }

            if !current_batch.text.is_empty() {
                current_batch.text.push('\n');
            }
            current_batch.text.push_str(seg_text);
            is_batch_empty = false;
        }
    }
    if !is_batch_empty {
        batches.push(current_batch);
    }

    // 2. 批量合成并拼接音频
    let mut all_samples: Vec<i16> = Vec::new();
    let mut wav_spec: Option<hound::WavSpec> = None;

    for batch in batches {
        let (target_voice, volume, speed, pitch) = if batch.is_dialogue {
            (
                req.voice_dialogue_id.clone().or(Some(req.voice_id.clone())),
                req.volume_dialogue
                    .or(req.volume)
                    .or(Some(state.default_volume)),
                req.speed_dialogue
                    .or(req.speed)
                    .or(Some(state.default_speed)),
                req.pitch_dialogue
                    .or(req.pitch)
                    .or(Some(state.default_pitch)),
            )
        } else {
            (
                Some(req.voice_id.clone()),
                req.volume.or(Some(state.default_volume)),
                req.speed.or(Some(state.default_speed)),
                req.pitch.or(Some(state.default_pitch)),
            )
        };

        match client
            .generate_speech(&batch.text, &target_voice, &volume, &speed, &pitch)
            .await
        {
            Ok(audio_data) => {
                if audio_data.is_empty() {
                    continue;
                }
                // 解码 WAV 获取采样点
                if let Ok(mut reader) = hound::WavReader::new(Cursor::new(&audio_data)) {
                    if wav_spec.is_none() {
                        wav_spec = Some(reader.spec());
                    }
                    for sample in reader.samples::<i16>() {
                        if let Ok(s) = sample {
                            all_samples.push(s);
                        }
                    }
                }
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiResponse {
                        success: false,
                        message: format!("合成失败: {}", e),
                    }),
                )
                    .into_response();
            }
        }
    }

    if all_samples.is_empty() || wav_spec.is_none() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiResponse {
                success: false,
                message: "生成音频为空".to_string(),
            }),
        )
            .into_response();
    }

    // 3. 重新编码为单个 WAV
    let spec = wav_spec.unwrap();
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut writer = match hound::WavWriter::new(&mut buffer, spec) {
            Ok(w) => w,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiResponse {
                        success: false,
                        message: format!("创建 WAV Writer 失败: {}", e),
                    }),
                )
                    .into_response();
            }
        };

        for sample in all_samples {
            if writer.write_sample(sample).is_err() {
                break;
            }
        }
        if let Err(e) = writer.finalize() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse {
                    success: false,
                    message: format!("WAV Finalize 失败: {}", e),
                }),
            )
                .into_response();
        }
    }

    let final_wav_bytes = buffer.into_inner();
    (
        StatusCode::OK,
        [("Content-Type", "audio/wav")],
        final_wav_bytes,
    )
        .into_response()
}

async fn test_regex_handler(Json(req): Json<TestRegexRequest>) -> impl IntoResponse {
    match Regex::new(&req.regex) {
        Ok(re) => {
            let result = re.replace_all(&req.text, "").to_string();
            (
                StatusCode::OK,
                Json(TestRegexResponse {
                    success: true,
                    result: Some(result),
                    error: None,
                }),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::OK,
            Json(TestRegexResponse {
                success: false,
                result: None,
                error: Some(e.to_string()),
            }),
        )
            .into_response(),
    }
}

fn log_info(tx: &broadcast::Sender<String>, msg: String) {
    println!("{}", msg);
    let _ = tx.send(msg);
}

fn log_error(tx: &broadcast::Sender<String>, msg: String) {
    eprintln!("{}", msg);
    let _ = tx.send(format!("❌ Error: {}", msg));
}

fn fallback_task_title(path: &Path) -> String {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());

    if let Some(captures) = Regex::new(r"《([^》]+)》")
        .ok()
        .and_then(|re| re.captures(&file_name))
    {
        if let Some(title) = captures
            .get(1)
            .map(|m| m.as_str().trim())
            .filter(|v| !v.is_empty())
        {
            return title.to_string();
        }
    }

    file_name
}

fn normalize_task_title(value: &str, fallback: &str) -> String {
    let trimmed = value
        .trim()
        .trim_matches(['"', '\'', '`', '“', '”', '‘', '’'])
        .trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }

    let lower = trimmed.to_lowercase();
    if lower.ends_with(".epub") || lower.ends_with(".txt") {
        return Path::new(trimmed)
            .file_stem()
            .map(|stem| stem.to_string_lossy().trim().to_string())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| fallback.to_string());
    }

    trimmed.to_string()
}

fn title_from_ai_response(content: &str, fallback: &str) -> Option<String> {
    let mut text = content.trim();
    if text.starts_with("```") {
        text = text.trim_matches('`').trim();
        if let Some(rest) = text.strip_prefix("json") {
            text = rest.trim();
        }
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(title) = value
            .get("title")
            .and_then(|v| v.as_str())
            .map(|v| normalize_task_title(v, fallback))
            .filter(|v| !v.is_empty())
        {
            return Some(title);
        }
    }

    let title = normalize_task_title(text, fallback);
    if title == fallback && text.is_empty() {
        None
    } else {
        Some(title)
    }
}

async fn derive_task_title(path: &Path, config: &AiDialogueConfig) -> String {
    let fallback = fallback_task_title(path);
    if !crate::ai::should_use_ai(config) {
        return fallback;
    }

    let Some(api_url) = config.api_url.as_deref() else {
        return fallback;
    };
    let Some(api_key) = crate::ai::pick_api_key(config.api_key.as_deref().unwrap_or_default())
    else {
        return fallback;
    };
    let Some(model) = config.model.as_deref() else {
        return fallback;
    };
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "You extract the book title from a file name. Return JSON only: {\"title\":\"...\"}. Do not include author, source site, file extension, quality tags, or commentary."
            },
            {
                "role": "user",
                "content": format!("File name: {}", file_name)
            }
        ],
        "temperature": 0.0,
        "max_tokens": 80,
        "thinking": {"type": "disabled"}
    });

    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .build()
    else {
        return fallback;
    };
    let Ok(response) = client
        .post(api_url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
    else {
        return fallback;
    };
    if !response.status().is_success() {
        return fallback;
    }
    let Ok(value) = response.json::<serde_json::Value>().await else {
        return fallback;
    };
    let content = value
        .get("choices")
        .and_then(|choices| choices.as_array())
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| {
            message
                .get("content")
                .or_else(|| message.get("reasoning_content"))
        })
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    title_from_ai_response(content, &fallback).unwrap_or(fallback)
}

//... (synthesize_handler and synthesize_upload_handler remain mostly the same, but omitted for brevity)
async fn synthesize_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TtsRequest>,
) -> impl IntoResponse {
    let tx = state.tx.clone();
    log_info(&tx, "接收到合成请求".to_string());

    let output_dir = PathBuf::from("output");

    let ai_dialogue = req.ai_dialogue.clone().unwrap_or_default();
    let mut cli = Cli {
        list: false,
        file: None,
        dir: None,
        api: Some(req.api_url.clone()),
        out: output_dir.clone(),
        output_name: None,
        voice: Some(req.voice_id.clone()),
        voice_dialogue: req.voice_dialogue_id.clone(),
        volume_dialogue: req.volume_dialogue,
        speed_dialogue: req.speed_dialogue,
        pitch_dialogue: req.pitch_dialogue,
        volume: req.volume.unwrap_or(state.default_volume),
        speed: req.speed.unwrap_or(state.default_speed),
        pitch: req.pitch.unwrap_or(state.default_pitch),
        sub: req.sub.unwrap_or(0),
        blacklist: None,
        ignore_regex: req
            .ignore_regex
            .clone()
            .unwrap_or_else(|| r"\*{3,}|#{2,}".to_string()),
        concurrency: req.concurrency.unwrap_or(4),
        preserve_structure: false,
        analysis_only: req.analysis_only,
        web: false,
        ai_dialogue,
    };

    let client = if cli.analysis_only {
        None
    } else {
        match ApiClient::new(req.api_url.clone()) {
            Ok(c) => Some(c),
            Err(e) => {
                let err_msg = format!("创建 API 客户端失败: {}", e);
                log_error(&tx, err_msg.clone());
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiResponse {
                        success: false,
                        message: err_msg,
                    }),
                )
                    .into_response();
            }
        }
    };

    if let Some(text) = req.text_content {
        let file_name = req.output_name.unwrap_or_else(|| "web_task".to_string());
        cli.output_name = Some(file_name.clone());
        if let Err(e) = std::fs::create_dir_all(&output_dir) {
            let err_msg = format!("无法创建输出目录: {}", e);
            log_error(&tx, err_msg.clone());
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse {
                    success: false,
                    message: err_msg,
                }),
            )
                .into_response();
        }
        let temp_path = output_dir.join(format!("{}.txt", file_name));

        if let Err(e) = std::fs::write(&temp_path, text) {
            let err_msg = format!("无法写入临时文件: {}", e);
            log_error(&tx, err_msg.clone());
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse {
                    success: false,
                    message: err_msg,
                }),
            )
                .into_response();
        }

        if cli.analysis_only {
            return start_analysis_task(state, temp_path, cli, file_name, true).await;
        }

        let file_id = format!("{:x}", md5::compute(temp_path.to_string_lossy().as_bytes()));

        // 添加到任务列表
        {
            let mut tasks = state.tasks.lock().await;
            tasks.insert(
                file_id.clone(),
                TaskState {
                    id: file_id.clone(),
                    file_name: file_name.clone(),
                    full_path: Some(temp_path.to_string_lossy().to_string()),
                    status: "processing".to_string(),
                    current: 0,
                    total: 0,
                    error_msg: None,
                    cli_config: Some(cli.clone()),
                    start_time: Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    ),
                    end_time: None,
                    size: None,
                    eta: None,
                    created_at: Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64,
                    ),
                    output_path: None,
                    is_hidden: false,
                    active_phase: Some("synthesis".to_string()),
                    analysis_phase: None,
                    synthesis_phase: Some(TaskPhaseState {
                        status: "processing".to_string(),
                        start_time: Some(
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs(),
                        ),
                        ..Default::default()
                    }),
                },
            );
            save_tasks_to_disk(&*tasks).await;
        }

        let tx_clone = tx.clone();
        let tasks_state = state.tasks.clone();
        let fid_cb = file_id.clone();
        let fid_control = file_id.clone();

        let task = tokio::spawn(async move {
            let callback_tx = tx_clone.clone();
            let tasks_clone = tasks_state.clone();

            let callback = move |event: process::ProcessEvent| match event {
                process::ProcessEvent::Log(msg) => log_info(&callback_tx, msg),
                process::ProcessEvent::Progress { current, total } => {
                    let tasks = tasks_clone.clone();
                    let f_id = fid_cb.clone();
                    tokio::spawn(async move {
                        let mut lock = tasks.lock().await;
                        if let Some(task) = lock.get_mut(&f_id) {
                            task.current = current;
                            task.total = total;
                        }
                    });
                }
                process::ProcessEvent::Success { size, output_path } => {
                    let tasks = tasks_clone.clone();
                    let f_id = fid_cb.clone();
                    tokio::spawn(async move {
                        let mut lock = tasks.lock().await;
                        if let Some(task) = lock.get_mut(&f_id) {
                            task.size = Some(size);
                            task.output_path = Some(output_path);
                        }
                        save_tasks_to_disk(&*lock).await;
                    });
                }
            };

            log_info(&tx_clone, "后台任务启动: 处理文本...".to_string());
            let alloc_table = create_live_allocation_table(&temp_path, &cli.ai_dialogue);
            let check_cancel = {
                let tasks = tasks_state.clone();
                let fid = fid_control.clone();
                move || {
                    let tasks = tasks.clone();
                    let fid = fid.clone();
                    async move { task_cancelled_or_missing(tasks, fid).await }
                }
            };
            let wait_if_paused = {
                let tasks = tasks_state.clone();
                let fid = fid_control.clone();
                move || {
                    let tasks = tasks.clone();
                    let fid = fid.clone();
                    async move { wait_task_if_paused(tasks, fid).await }
                }
            };
            match process::process_file(
                &temp_path,
                &cli,
                client
                    .as_ref()
                    .expect("client is present for synthesis task"),
                &Option::<Regex>::None,
                callback,
                check_cancel,
                wait_if_paused,
                alloc_table.clone(),
            )
            .await
            {
                Ok(_) => {
                    if let Some(ref at) = alloc_table {
                        save_allocation_table(&at.lock().unwrap());
                    }
                    log_info(&tx_clone, "后台任务完成: 文本处理完毕。".to_string());
                    update_task_status(&tasks_state, &temp_path, "completed", None).await;
                    let _ = std::fs::remove_file(temp_path);
                }
                Err(e) => {
                    log_error(&tx_clone, format!("后台任务出错: {}", e));
                    if e.to_string() == "浠诲姟宸插彇娑?" {
                        update_task_status(&tasks_state, &temp_path, "cancelled", None).await;
                    } else {
                        update_task_status(&tasks_state, &temp_path, "error", Some(e.to_string()))
                            .await;
                    }
                }
            }
            let _ = tx_clone.send("__STATUS__:DONE".to_string());
        });

        remember_latest_task_handle(&state, task.abort_handle()).await;

        (
            StatusCode::OK,
            Json(ApiResponse {
                success: true,
                message: "任务已在后台启动，请查看下方实时日志...".to_string(),
            }),
        )
            .into_response()
    } else if let Some(path_str) = req.file_path {
        let path = resolve_input_file_path(&path_str);
        if !path.exists() {
            let err_msg = "文件不存在".to_string();
            log_error(&tx, err_msg.clone());
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse {
                    success: false,
                    message: err_msg,
                }),
            )
                .into_response();
        }

        let task_name = derive_task_title(&path, &cli.ai_dialogue).await;
        cli.output_name = Some(task_name.clone());
        if cli.analysis_only {
            return start_analysis_task(state, path, cli, task_name, false).await;
        }
        let file_id = format!("{:x}", md5::compute(path.to_string_lossy().as_bytes()));

        // 添加到任务列表
        {
            let mut tasks = state.tasks.lock().await;
            tasks.insert(
                file_id.clone(),
                TaskState {
                    id: file_id.clone(),
                    file_name: task_name.clone(),
                    full_path: Some(path.to_string_lossy().to_string()),
                    status: "processing".to_string(),
                    current: 0,
                    total: 0,
                    error_msg: None,
                    cli_config: Some(cli.clone()),
                    start_time: Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    ),
                    end_time: None,
                    size: None,
                    eta: None,
                    created_at: Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64,
                    ),
                    output_path: None,
                    is_hidden: false,
                    active_phase: Some("synthesis".to_string()),
                    analysis_phase: None,
                    synthesis_phase: Some(TaskPhaseState {
                        status: "processing".to_string(),
                        start_time: Some(
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs(),
                        ),
                        ..Default::default()
                    }),
                },
            );
            save_tasks_to_disk(&*tasks).await;
        }

        let tx_clone = tx.clone();
        let tasks_state = state.tasks.clone();
        let fid_cb = file_id.clone();
        let fid_control = file_id.clone();

        let task = tokio::spawn(async move {
            let callback_tx = tx_clone.clone();
            let tasks_clone = tasks_state.clone();

            let callback = move |event: process::ProcessEvent| match event {
                process::ProcessEvent::Log(msg) => log_info(&callback_tx, msg),
                process::ProcessEvent::Progress { current, total } => {
                    let tasks = tasks_clone.clone();
                    let f_id = fid_cb.clone();
                    tokio::spawn(async move {
                        let mut lock = tasks.lock().await;
                        if let Some(task) = lock.get_mut(&f_id) {
                            task.current = current;
                            task.total = total;
                        }
                    });
                }
                process::ProcessEvent::Success { size, output_path } => {
                    let tasks = tasks_clone.clone();
                    let f_id = fid_cb.clone();
                    tokio::spawn(async move {
                        let mut lock = tasks.lock().await;
                        if let Some(task) = lock.get_mut(&f_id) {
                            task.size = Some(size);
                            task.output_path = Some(output_path);
                        }
                        save_tasks_to_disk(&*lock).await;
                    });
                }
            };

            log_info(
                &tx_clone,
                format!("后台任务启动: 处理本地文件 {:?}...", path),
            );
            let alloc_table = create_live_allocation_table(&path, &cli.ai_dialogue);
            let check_cancel = {
                let tasks = tasks_state.clone();
                let fid = fid_control.clone();
                move || {
                    let tasks = tasks.clone();
                    let fid = fid.clone();
                    async move { task_cancelled_or_missing(tasks, fid).await }
                }
            };
            let wait_if_paused = {
                let tasks = tasks_state.clone();
                let fid = fid_control.clone();
                move || {
                    let tasks = tasks.clone();
                    let fid = fid.clone();
                    async move { wait_task_if_paused(tasks, fid).await }
                }
            };
            match process::process_file(
                &path,
                &cli,
                client
                    .as_ref()
                    .expect("client is present for synthesis task"),
                &Option::<Regex>::None,
                callback,
                check_cancel,
                wait_if_paused,
                alloc_table.clone(),
            )
            .await
            {
                Ok(_) => {
                    if let Some(ref at) = alloc_table {
                        save_allocation_table(&at.lock().unwrap());
                    }
                    log_info(&tx_clone, format!("后台任务完成: {:?} 处理完毕。", path));
                    update_task_status(&tasks_state, &path, "completed", None).await;
                    if let Err(e) = std::fs::remove_file(&path) {
                        log_error(&tx_clone, format!("无法删除上传文件: {}", e));
                    }
                }
                Err(e) => {
                    log_error(&tx_clone, format!("后台任务出错: {}", e));
                    update_task_status(&tasks_state, &path, "error", Some(e.to_string())).await;
                }
            }
            let _ = tx_clone.send("__STATUS__:DONE".to_string());
        });

        remember_latest_task_handle(&state, task.abort_handle()).await;

        (
            StatusCode::OK,
            Json(ApiResponse {
                success: true,
                message: "任务已在后台启动，请查看下方实时日志...".to_string(),
            }),
        )
            .into_response()
    } else {
        let err_msg = "必须提供文本或文件路径".to_string();
        log_error(&tx, err_msg.clone());
        (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: err_msg,
            }),
        )
            .into_response()
    }
}

async fn start_analysis_task(
    state: Arc<AppState>,
    path: PathBuf,
    cli: Cli,
    task_name: String,
    remove_source_when_done: bool,
) -> axum::response::Response {
    let tx = state.tx.clone();
    let file_id = analysis_task_id(&path);

    {
        let mut tasks = state.tasks.lock().await;
        tasks.insert(
            file_id.clone(),
            TaskState {
                id: file_id.clone(),
                file_name: format!("分析: {}", task_name),
                full_path: Some(path.to_string_lossy().to_string()),
                status: "processing".to_string(),
                current: 0,
                total: 0,
                error_msg: None,
                cli_config: Some(cli.clone()),
                start_time: Some(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                ),
                end_time: None,
                size: None,
                eta: None,
                created_at: Some(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64,
                ),
                output_path: None,
                is_hidden: false,
                active_phase: Some("analysis".to_string()),
                analysis_phase: Some(TaskPhaseState {
                    status: "processing".to_string(),
                    start_time: Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    ),
                    ..Default::default()
                }),
                synthesis_phase: None,
            },
        );
        save_tasks_to_disk(&*tasks).await;
    }

    let tx_clone = tx.clone();
    let tasks_state = state.tasks.clone();
    let fid_cb = file_id.clone();
    let fid_control = file_id.clone();

    let task = tokio::spawn(async move {
        let callback_tx = tx_clone.clone();
        let tasks_clone = tasks_state.clone();
        let callback = move |event: process::ProcessEvent| match event {
            process::ProcessEvent::Log(msg) => log_info(&callback_tx, msg),
            process::ProcessEvent::Progress { current, total } => {
                let tasks = tasks_clone.clone();
                let f_id = fid_cb.clone();
                tokio::spawn(async move {
                    let mut lock = tasks.lock().await;
                    if let Some(task) = lock.get_mut(&f_id) {
                        task.current = current;
                        task.total = total;
                    }
                    save_tasks_to_disk(&*lock).await;
                });
            }
            process::ProcessEvent::Success { .. } => {}
        };

        log_info(&tx_clone, format!("后台分析任务启动: {:?}...", path));
        let alloc_table = create_live_allocation_table(&path, &cli.ai_dialogue);
        let check_cancel = {
            let tasks = tasks_state.clone();
            let fid = fid_control.clone();
            move || {
                let tasks = tasks.clone();
                let fid = fid.clone();
                async move { task_cancelled_or_missing(tasks, fid).await }
            }
        };
        let wait_if_paused = {
            let tasks = tasks_state.clone();
            let fid = fid_control.clone();
            move || {
                let tasks = tasks.clone();
                let fid = fid.clone();
                async move { wait_task_if_paused(tasks, fid).await }
            }
        };

        match process::analyze_file_dialogues(
            &path,
            &cli,
            &Option::<Regex>::None,
            callback,
            check_cancel,
            wait_if_paused,
            alloc_table.clone(),
        )
        .await
        {
            Ok(_) => {
                if let Some(ref at) = alloc_table {
                    save_allocation_table(&at.lock().unwrap());
                }
                log_info(&tx_clone, format!("后台分析任务完成: {:?}", path));
                update_task_status_by_id(&tasks_state, &fid_control, "completed", None).await;
                if remove_source_when_done {
                    let _ = std::fs::remove_file(&path);
                }
            }
            Err(e) => {
                log_error(&tx_clone, format!("后台分析任务出错: {}", e));
                if e.to_string() == "任务已取消" {
                    update_task_status_by_id(&tasks_state, &fid_control, "cancelled", None).await;
                } else {
                    update_task_status_by_id(
                        &tasks_state,
                        &fid_control,
                        "error",
                        Some(e.to_string()),
                    )
                    .await;
                }
            }
        }
        let _ = tx_clone.send("__STATUS__:DONE".to_string());
    });

    remember_latest_task_handle(&state, task.abort_handle()).await;

    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: "AI 分析任务已在后台启动，请查看下方实时日志。".to_string(),
        }),
    )
        .into_response()
}

async fn analyze_book_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AnalyzeBookRequest>,
) -> impl IntoResponse {
    let tx = state.tx.clone();
    let path = resolve_input_file_path(&req.file_path);
    if !path.exists() {
        let err_msg = "文件不存在".to_string();
        log_error(&tx, err_msg.clone());
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: err_msg,
            }),
        )
            .into_response();
    }

    let mut ai_dialogue = match req.ai_dialogue {
        Some(config) => config,
        None => state.ai_dialogue_config.lock().await.clone(),
    };
    ai_dialogue.chapter_analysis_enabled = true;
    if let Err(e) = crate::ai::check_ai_config(&ai_dialogue) {
        let err_msg = format!("AI 配置不可用: {}", e);
        log_error(&tx, err_msg.clone());
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: err_msg,
            }),
        )
            .into_response();
    }

    let task_name = derive_task_title(&path, &ai_dialogue).await;
    let output_dir = if PathBuf::from("/output").exists() {
        PathBuf::from("/output")
    } else {
        PathBuf::from("output")
    };
    let cli = Cli {
        list: false,
        file: None,
        dir: None,
        api: None,
        out: output_dir,
        output_name: Some(task_name.clone()),
        voice: None,
        voice_dialogue: None,
        volume_dialogue: None,
        speed_dialogue: None,
        pitch_dialogue: None,
        volume: state.default_volume,
        speed: state.default_speed,
        pitch: state.default_pitch,
        sub: 0,
        blacklist: None,
        ignore_regex: req
            .ignore_regex
            .unwrap_or_else(|| r"\*{3,}|#{2,}".to_string()),
        concurrency: 1,
        preserve_structure: false,
        analysis_only: true,
        web: false,
        ai_dialogue,
    };

    start_analysis_task(state, path, cli, task_name, false).await
}

async fn analyze_book_upload_handler(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let tx = state.tx.clone();
    let book_dir = if PathBuf::from("/book").exists() {
        PathBuf::from("/book")
    } else {
        PathBuf::from("book")
    };
    let upload_dir = book_dir.join("upload");
    let mut ignore_regex = r"\*{3,}|#{2,}".to_string();
    let mut ai_dialogue = AiDialogueConfig::default();
    let mut uploaded_file_path: Option<PathBuf> = None;

    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            let file_name = field.file_name().unwrap_or("uploaded_file").to_string();
            if let Err(e) = std::fs::create_dir_all(&upload_dir) {
                let err_msg = format!("无法创建上传目录: {}", e);
                log_error(&tx, err_msg.clone());
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiResponse {
                        success: false,
                        message: err_msg,
                    }),
                )
                    .into_response();
            }

            let temp_path = upload_dir.join(&file_name);
            let mut file = match File::create(&temp_path).await {
                Ok(f) => f,
                Err(e) => {
                    let err_msg = format!("无法创建上传文件: {}", e);
                    log_error(&tx, err_msg.clone());
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiResponse {
                            success: false,
                            message: err_msg,
                        }),
                    )
                        .into_response();
                }
            };

            let mut stream = field;
            while let Some(chunk) = stream.chunk().await.unwrap_or(None) {
                if let Err(e) = file.write_all(&chunk).await {
                    let err_msg = format!("写入上传文件失败: {}", e);
                    log_error(&tx, err_msg.clone());
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiResponse {
                            success: false,
                            message: err_msg,
                        }),
                    )
                        .into_response();
                }
            }
            uploaded_file_path = Some(temp_path);
        } else {
            let value = field.text().await.unwrap_or_default();
            match name.as_str() {
                "ignore_regex" => ignore_regex = value,
                "ai_dialogue" => ai_dialogue = serde_json::from_str(&value).unwrap_or_default(),
                _ => {}
            }
        }
    }

    let Some(path) = uploaded_file_path else {
        let err_msg = "未上传文件".to_string();
        log_error(&tx, err_msg.clone());
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: err_msg,
            }),
        )
            .into_response();
    };

    ai_dialogue.chapter_analysis_enabled = true;
    if let Err(e) = crate::ai::check_ai_config(&ai_dialogue) {
        let err_msg = format!("AI 配置不可用: {}", e);
        log_error(&tx, err_msg.clone());
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: err_msg,
            }),
        )
            .into_response();
    }

    let task_name = derive_task_title(&path, &ai_dialogue).await;
    let output_dir = if PathBuf::from("/output").exists() {
        PathBuf::from("/output")
    } else {
        PathBuf::from("output")
    };
    let cli = Cli {
        list: false,
        file: None,
        dir: None,
        api: None,
        out: output_dir,
        output_name: Some(task_name.clone()),
        voice: None,
        voice_dialogue: None,
        volume_dialogue: None,
        speed_dialogue: None,
        pitch_dialogue: None,
        volume: state.default_volume,
        speed: state.default_speed,
        pitch: state.default_pitch,
        sub: 0,
        blacklist: None,
        ignore_regex,
        concurrency: 1,
        preserve_structure: false,
        analysis_only: true,
        web: false,
        ai_dialogue,
    };

    start_analysis_task(state, path, cli, task_name, false).await
}

async fn synthesize_upload_handler(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let tx = state.tx.clone();
    log_info(&tx, "接收到文件上传请求".to_string());

    let output_dir = if PathBuf::from("/output").exists() {
        PathBuf::from("/output")
    } else {
        PathBuf::from("output")
    };

    let book_dir = if PathBuf::from("/book").exists() {
        PathBuf::from("/book")
    } else {
        PathBuf::from("book")
    };
    let upload_dir = book_dir.join("upload");

    let mut api_url = String::new();
    let mut voice_id = String::new();
    let mut voice_dialogue_id = String::new();
    let mut volume_dialogue: Option<u8> = None;
    let mut speed_dialogue: Option<u8> = None;
    let mut pitch_dialogue: Option<u8> = None;
    let mut volume = state.default_volume;
    let mut speed = state.default_speed;
    let mut pitch = state.default_pitch;
    let mut sub: Option<usize> = None;
    let mut concurrency: usize = 4;
    let mut ignore_regex = r"\*{3,}|#{2,}".to_string();
    let mut analysis_only = false;
    let mut ai_dialogue = AiDialogueConfig::default();
    let mut uploaded_file_path: Option<PathBuf> = None;

    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let name = field.name().unwrap_or("").to_string();

        if name == "file" {
            let file_name = field.file_name().unwrap_or("uploaded_file").to_string();

            if let Err(e) = std::fs::create_dir_all(&upload_dir) {
                let err_msg = format!("无法创建上传目录: {}", e);
                log_error(&tx, err_msg.clone());
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiResponse {
                        success: false,
                        message: err_msg,
                    }),
                )
                    .into_response();
            }

            let temp_path = upload_dir.join(&file_name);
            let mut file = match File::create(&temp_path).await {
                Ok(f) => f,
                Err(e) => {
                    let err_msg = format!("无法创建临时文件: {}", e);
                    log_error(&tx, err_msg.clone());
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiResponse {
                            success: false,
                            message: err_msg,
                        }),
                    )
                        .into_response();
                }
            };

            let mut stream = field;
            while let Some(chunk) = stream.chunk().await.unwrap_or(None) {
                if let Err(e) = file.write_all(&chunk).await {
                    let err_msg = format!("写入临时文件失败: {}", e);
                    log_error(&tx, err_msg.clone());
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiResponse {
                            success: false,
                            message: err_msg,
                        }),
                    )
                        .into_response();
                }
            }

            uploaded_file_path = Some(temp_path);
        } else {
            let value = field.text().await.unwrap_or_default();
            match name.as_str() {
                "api_url" => api_url = value,
                "voice_id" => voice_id = value,
                "voice_dialogue_id" => voice_dialogue_id = value,
                "volume_dialogue" => volume_dialogue = value.parse().ok(),
                "speed_dialogue" => speed_dialogue = value.parse().ok(),
                "pitch_dialogue" => pitch_dialogue = value.parse().ok(),
                "volume" => volume = value.parse().unwrap_or(state.default_volume),
                "speed" => speed = value.parse().unwrap_or(state.default_speed),
                "pitch" => pitch = value.parse().unwrap_or(state.default_pitch),
                "sub" => sub = value.parse().ok(),
                "concurrency" => concurrency = value.parse().unwrap_or(4),
                "ignore_regex" => ignore_regex = value,
                "analysis_only" => analysis_only = value == "true" || value == "1",
                "ai_dialogue" => {
                    ai_dialogue = serde_json::from_str(&value).unwrap_or_default();
                }
                _ => {}
            }
        }
    }

    if api_url.is_empty() || voice_id.is_empty() {
        let err_msg = "缺少必要的参数 (api_url, voice_id)".to_string();
        log_error(&tx, err_msg.clone());
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: err_msg,
            }),
        )
            .into_response();
    }

    if let Some(path) = uploaded_file_path {
        let mut cli = Cli {
            list: false,
            file: None,
            dir: None,
            api: Some(api_url.clone()),
            out: output_dir.clone(),
            output_name: None,
            voice: Some(voice_id.clone()),
            voice_dialogue: if voice_dialogue_id.is_empty() {
                None
            } else {
                Some(voice_dialogue_id)
            },
            volume_dialogue: volume_dialogue,
            speed_dialogue: speed_dialogue,
            pitch_dialogue: pitch_dialogue,
            volume: volume,
            speed: speed,
            pitch: pitch,
            sub: sub.unwrap_or(0),
            blacklist: None,
            ignore_regex: ignore_regex,
            concurrency: concurrency,
            preserve_structure: false,
            analysis_only,
            web: false,
            ai_dialogue,
        };
        let task_name = derive_task_title(&path, &cli.ai_dialogue).await;
        cli.output_name = Some(task_name.clone());
        if cli.analysis_only {
            return start_analysis_task(state, path, cli, task_name, false).await;
        }

        let client = match ApiClient::new(api_url) {
            Ok(c) => c,
            Err(e) => {
                let err_msg = format!("创建 API 客户端失败: {}", e);
                log_error(&tx, err_msg.clone());
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiResponse {
                        success: false,
                        message: err_msg,
                    }),
                )
                    .into_response();
            }
        };

        let file_id = format!("{:x}", md5::compute(path.to_string_lossy().as_bytes()));

        // 添加到任务列表
        {
            let mut tasks = state.tasks.lock().await;
            tasks.insert(
                file_id.clone(),
                TaskState {
                    id: file_id.clone(),
                    file_name: task_name.clone(),
                    full_path: Some(path.to_string_lossy().to_string()),
                    status: "processing".to_string(),
                    current: 0,
                    total: 0,
                    error_msg: None,
                    cli_config: Some(cli.clone()),
                    start_time: Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    ),
                    end_time: None,
                    size: None,
                    eta: None,
                    created_at: Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64,
                    ),
                    output_path: None,
                    is_hidden: false,
                    active_phase: Some("synthesis".to_string()),
                    analysis_phase: None,
                    synthesis_phase: Some(TaskPhaseState {
                        status: "processing".to_string(),
                        start_time: Some(
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs(),
                        ),
                        ..Default::default()
                    }),
                },
            );
            save_tasks_to_disk(&*tasks).await;
        }

        let tx_clone = tx.clone();
        let tasks_state = state.tasks.clone();
        let fid_cb = file_id.clone();
        let fid_control = file_id.clone();

        let task = tokio::spawn(async move {
            let callback_tx = tx_clone.clone();
            let tasks_clone = tasks_state.clone();

            let callback = move |event: process::ProcessEvent| match event {
                process::ProcessEvent::Log(msg) => log_info(&callback_tx, msg),
                process::ProcessEvent::Progress { current, total } => {
                    let tasks = tasks_clone.clone();
                    let f_id = fid_cb.clone();
                    tokio::spawn(async move {
                        let mut lock = tasks.lock().await;
                        if let Some(task) = lock.get_mut(&f_id) {
                            task.current = current;
                            task.total = total;
                        }
                    });
                }
                process::ProcessEvent::Success { size, output_path } => {
                    let tasks = tasks_clone.clone();
                    let f_id = fid_cb.clone();
                    tokio::spawn(async move {
                        let mut lock = tasks.lock().await;
                        if let Some(task) = lock.get_mut(&f_id) {
                            task.size = Some(size);
                            task.output_path = Some(output_path);
                        }
                        save_tasks_to_disk(&*lock).await;
                    });
                }
            };

            log_info(
                &tx_clone,
                format!("后台任务启动: 处理上传文件 {:?}...", path),
            );
            let alloc_table = create_live_allocation_table(&path, &cli.ai_dialogue);
            let check_cancel = {
                let tasks = tasks_state.clone();
                let fid = fid_control.clone();
                move || {
                    let tasks = tasks.clone();
                    let fid = fid.clone();
                    async move { task_cancelled_or_missing(tasks, fid).await }
                }
            };
            let wait_if_paused = {
                let tasks = tasks_state.clone();
                let fid = fid_control.clone();
                move || {
                    let tasks = tasks.clone();
                    let fid = fid.clone();
                    async move { wait_task_if_paused(tasks, fid).await }
                }
            };
            match process::process_file(
                &path,
                &cli,
                &client,
                &Option::<Regex>::None,
                callback,
                check_cancel,
                wait_if_paused,
                alloc_table.clone(),
            )
            .await
            {
                Ok(_) => {
                    if let Some(ref at) = alloc_table {
                        save_allocation_table(&at.lock().unwrap());
                    }
                    log_info(&tx_clone, format!("后台任务完成: {:?} 处理完毕。", path));
                    update_task_status(&tasks_state, &path, "completed", None).await;
                }
                Err(e) => {
                    log_error(&tx_clone, format!("后台任务出错: {}", e));
                    update_task_status(&tasks_state, &path, "error", Some(e.to_string())).await;
                }
            }
            let _ = tx_clone.send("__STATUS__:DONE".to_string());
        });

        remember_latest_task_handle(&state, task.abort_handle()).await;

        (
            StatusCode::OK,
            Json(ApiResponse {
                success: true,
                message: "文件上传成功，转换任务已在后台启动！请查看下方实时日志...".to_string(),
            }),
        )
            .into_response()
    } else {
        let err_msg = "未上传文件".to_string();
        log_error(&tx, err_msg.clone());
        (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: err_msg,
            }),
        )
            .into_response()
    }
}
