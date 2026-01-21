use axum::{
    extract::{Query, Json, Multipart, State, DefaultBodyLimit},
    http::StatusCode,
    response::{Html, IntoResponse, sse::{Event, Sse}},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::File;
use std::collections::HashMap;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Mutex};
use tokio::task::AbortHandle;
use tower_http::cors::CorsLayer;
use walkdir::WalkDir;
use std::path::Component;

use crate::api::{ApiClient, Voice};
use crate::args::Cli;
use crate::process;
use futures::stream::Stream;
use regex::Regex;
use tokio_stream::StreamExt;
use std::io::Cursor;
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::atomic::{AtomicBool, Ordering};


// 新增：用于存储预加载的数据
#[derive(Serialize, Clone, Default)]
struct InitialData {
    api_url: Option<String>,
    voices: Option<Vec<Voice>>,
    default_volume: u8,
    default_speed: u8,
    default_pitch: u8,
}

// 新增：任务状态结构体
#[derive(Serialize, Deserialize, Clone, Debug)]
struct TaskState {
    id: String,
    file_name: String,
    full_path: Option<String>, // 新增：完整路径，用于重试
    status: String, // pending, processing, completed, error
    current: usize,
    total: usize,
    error_msg: Option<String>,
    cli_config: Option<Cli>,   // 新增：保存任务配置，用于重试
    start_time: Option<u64>,   // 新增：任务开始时间戳
    end_time: Option<u64>,     // 新增：任务结束时间戳
    size: Option<u64>,         // 新增：任务输出大小
    eta: Option<u64>,          // 新增：预计剩余时间(秒)
    #[serde(default)]
    created_at: Option<u64>,   // 新增：任务创建时间戳(毫秒)，用于排序
    #[serde(default)]
    output_path: Option<String>, // 新增：输出路径
    #[serde(default)]
    is_hidden: bool,           // 新增：是否在前端隐藏
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
}

#[derive(Deserialize)]
struct CancelTaskRequest {
    id: String,
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

#[derive(Serialize)]
struct AutorunStatus {
    enabled: bool,
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
}

pub async fn start_server(port: u16, api_url: Option<String>) -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let (tx, _rx) = broadcast::channel(100);

    let default_volume = std::env::var("DEFAULT_VOLUME").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
    let default_speed = std::env::var("DEFAULT_SPEED").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
    let default_pitch = std::env::var("DEFAULT_PITCH").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
    let is_autorun_env = std::env::var("AUTORUN").map(|v| v.to_lowercase() == "true").unwrap_or(false);
    
    let is_autorun = Arc::new(AtomicBool::new(is_autorun_env));
    let autorun_config = Arc::new(Mutex::new(None));

    if is_autorun_env {
        // 如果环境变量开启了自动运行，创建一个默认配置
        let cli = Cli {
            list: false,
            file: None,
            dir: None,
            api: api_url.clone(),
            out: if PathBuf::from("/output").exists() { PathBuf::from("/output") } else { PathBuf::from("output") },
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
            web: false,
        };
        *autorun_config.lock().await = Some(cli);
    }

    // 启动时加载任务状态
    let tasks_map = load_tasks_from_disk().await;

    let initial_data = Arc::new(Mutex::new(InitialData {
        api_url: api_url.clone(),
        voices: None,
        default_volume,
        default_speed,
        default_pitch,
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
    });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/api/initial_data", get(initial_data_handler))
        .route("/api/tasks", get(get_tasks_handler)) // 新增：获取任务列表
        .route("/api/voices", get(get_voices_handler))
        .route("/api/synthesize", post(synthesize_handler))
        .route("/api/synthesize_upload", post(synthesize_upload_handler))
        .route("/api/batch_convert", post(batch_convert_handler)) // 新增
        .route("/api/autorun", post(set_autorun_handler)) // 新增：设置自动运行
        .route("/api/autorun/status", get(get_autorun_status_handler)) // 新增：获取自动运行状态
        .route("/api/cancel_task", post(cancel_task_handler)) // 新增：取消任务
        .route("/api/retry_task", post(retry_task_handler))   // 新增：重试任务
        .route("/api/retry_all_failed", post(retry_all_failed_handler)) // 新增：重试所有失败
        .route("/api/clear_all_tasks", post(clear_all_tasks_handler)) // 新增：清空所有任务
        .route("/api/clear_completed_tasks", post(clear_completed_tasks_handler)) // 新增：清除已完成
        .route("/api/reset_history", post(reset_history_handler)) // 新增：彻底重置历史
        .route("/api/files/list", get(list_files_handler)) // 新增：列出文件
        .route("/api/files/delete", post(delete_file_handler)) // 新增：删除文件
        .route("/api/files/download", get(download_file_handler)) // 新增：下载文件
        .route("/api/files/upload", post(upload_file_manager_handler)) // 新增：上传文件(文件管理)
        .route("/api/preview", post(preview_handler)) // 新增预览接口
        .route("/api/test_regex", post(test_regex_handler)) // 新增：测试正则
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
            let out_dir = if PathBuf::from("/output").exists() { PathBuf::from("/output") } else { PathBuf::from("output") };
            
            let cli = Cli {
                list: false,
                file: None,
                dir: None,
                api: api_url,
                out: out_dir,
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
                ignore_regex: config.ignore_regex.unwrap_or_else(|| r"\*{3,}|#{2,}".to_string()),
                concurrency: 4,
                preserve_structure: config.preserve_structure,
                web: false,
            };
            *state.autorun_config.lock().await = Some(cli);
            log_info(&state.tx, "🤖 自动检测已开启，将每 10 秒扫描一次 /book 目录。".to_string());
        } else {
             // 如果没有提供配置但开启了，尝试使用现有配置，如果没有则报错
             let guard = state.autorun_config.lock().await;
             if guard.is_none() {
                 return (StatusCode::BAD_REQUEST, Json(ApiResponse { success: false, message: "开启自动检测需要提供配置参数".to_string() })).into_response();
             }
             log_info(&state.tx, "🤖 自动检测已开启 (使用已有配置)。".to_string());
        }
    } else {
        log_info(&state.tx, "🤖 自动检测已停止。".to_string());
    }

    (StatusCode::OK, Json(ApiResponse { success: true, message: if req.enabled { "自动检测已开启" } else { "自动检测已关闭" }.to_string() })).into_response()
}

async fn get_autorun_status_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let enabled = state.is_autorun.load(Ordering::Relaxed);
    Json(AutorunStatus { enabled })
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

    let book_dir = if PathBuf::from("/book").exists() { PathBuf::from("/book") } else { PathBuf::from("book") };
    if !book_dir.exists() { return; }

    let files: Vec<PathBuf> = WalkDir::new(&book_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.path().extension().map_or(false, |ext| ext == "txt" || ext == "epub"))
        .map(|e| e.path().to_path_buf())
        .collect();

    let client = match ApiClient::new(api_url) {
        Ok(c) => c,
        Err(_) => return,
    };

    // 1. 发现新文件并加入队列 (Pending)
    {
        let mut tasks = state.tasks.lock().await;
        let mut added = false;
        for path in files {
            let file_id = format!("{:x}", md5::compute(path.to_string_lossy().as_bytes()));
            if !tasks.contains_key(&file_id) {
                // 计算子目录结构，保持输出目录层级
                let mut task_cli = cli.clone();
                if cli.preserve_structure {
                    if let Ok(relative) = path.strip_prefix(&book_dir) {
                        if let Some(parent) = relative.parent() {
                            if parent.components().count() > 0 {
                                task_cli.out = task_cli.out.join(parent);
                            }
                        }
                    }
                }

                tasks.insert(file_id.clone(), TaskState {
                    id: file_id,
                    file_name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                    full_path: Some(path.to_string_lossy().to_string()),
                    status: "pending".to_string(), // 默认为等待中
                    current: 0,
                    total: 0,
                    error_msg: None,
                    cli_config: Some(task_cli),
                    start_time: None,
                    end_time: None,
                    size: None,
                    eta: None,
                    created_at: Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64),
                    output_path: None,
                    is_hidden: false,
                });
                added = true;
                log_info(&state.tx, format!("🤖 自动检测到新文件(已加入队列): {:?}", path.file_name().unwrap_or_default()));
            }
        }
        if added {
            save_tasks_to_disk(&*tasks).await;
        }
    }

    // 2. 调度逻辑：检查是否有正在运行的任务，如果没有则启动下一个 Pending 任务
    let next_task = {
        let mut tasks = state.tasks.lock().await;
        
        // 如果有任务正在进行中，则跳过本次调度
        if tasks.values().any(|t| t.status == "processing") {
            return;
        }

        // 按加入时间排序找到下一个 pending 任务
        let target = tasks.values_mut()
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
            t.start_time = Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs());
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
            
            let callback = move |event: process::ProcessEvent| {
                 match event {
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
                                     let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
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

            let tasks_state_cancel = tasks_state.clone();
            let check_cancel = move || {
                let is_autorun = is_autorun.clone();
                let tasks = tasks_state_cancel.clone();
                let fid = fid.clone();
                async move {
                    if !is_autorun.load(Ordering::Relaxed) {
                        return true;
                    }
                    let tasks = tasks.lock().await;
                    if let Some(t) = tasks.get(&fid) {
                        return t.status == "cancelled";
                    }
                    false
                }
            };

            log_info(&tx, format!("🤖 自动检测任务开始: {:?}", path_clone.file_name().unwrap_or_default()));
            match process::process_file(&path_clone, &cli_clone, &client_clone, &None, callback, check_cancel).await {
                Ok(_) => update_task_status(&tasks_state, &path_clone, "completed", None).await,
                Err(e) => {
                    if e.to_string() == "任务已取消" {
                        update_task_status(&tasks_state, &path_clone, "cancelled", None).await;
                    } else {
                        update_task_status(&tasks_state, &path_clone, "error", Some(e.to_string())).await;
                    }
                }
            }
        });
    }
}

async fn initial_data_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let data = state.initial_data.lock().await;
    Json(data.clone())
}

// 新增：获取任务列表 Handler
async fn get_tasks_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let tasks_map = state.tasks.lock().await;
    let tasks: Vec<TaskState> = tasks_map.values().filter(|t| !t.is_hidden).cloned().collect();
    Json(tasks)
}

async fn stop_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut handle_lock = state.task_handle.lock().await;
    
    // 更新所有进行中或等待的任务状态为已取消
    {
        let mut tasks = state.tasks.lock().await;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        for task in tasks.values_mut() {
            if task.status == "processing" || task.status == "pending" {
                task.status = "cancelled".to_string();
                task.error_msg = Some("用户手动停止".to_string());
                task.end_time = Some(now);
            }
        }
        save_tasks_to_disk(&*tasks).await;
    }

    if let Some(handle) = handle_lock.take() {
        handle.abort();
        let _ = state.tx.send("🛑 用户已手动停止任务。".to_string());
        (StatusCode::OK, Json(ApiResponse { success: true, message: "任务已停止".to_string() })).into_response()
    } else {
        (StatusCode::OK, Json(ApiResponse { success: true, message: "任务已停止 (清理了残留状态)".to_string() })).into_response()
    }
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("static/index.html"))
}

async fn sse_handler(State(state): State<Arc<AppState>>) -> Sse<impl Stream<Item = Result<Event, axum::Error>>> {
    let rx = state.tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).map(|msg| {
        match msg {
            Ok(msg) => Ok(Event::default().data(msg)),
            Err(_) => Ok(Event::default().comment("skipped")),
        }
    });
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

// 修改：成功后持久化 API URL 和声音列表
async fn get_voices_handler(
    Query(params): Query<VoicesQuery>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    log_info(&state.tx, format!("正在从 {} 获取声音列表...", params.api_url));
    match ApiClient::new(params.api_url.clone()) {
        Ok(client) => match client.fetch_voices().await {
            Ok(voices) => {
                // 持久化
                let mut data = state.initial_data.lock().await;
                data.api_url = Some(params.api_url);
                data.voices = Some(voices.clone());
                log_info(&state.tx, "API 地址和声音列表已更新。".to_string());

                (StatusCode::OK, Json(serde_json::to_value(voices).unwrap())).into_response()
            }
            Err(e) => {
                log_error(&state.tx, format!("获取声音列表失败: {}", e));
                (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response()
            }
        },
        Err(e) => {
            log_error(&state.tx, format!("创建 API 客户端失败: {}", e));
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("无法创建客户端: {}", e) }))).into_response()
        }
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
        return (StatusCode::BAD_REQUEST, Json(ApiResponse { success: false, message: err_msg })).into_response();
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
            log_error(&tx, format!("输入目录 {:?} 不存在。请确认 Docker 卷已正确挂载或当前目录下存在 book 文件夹。", book_dir));
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
                    && e.path().extension().map_or(false, |ext| {
                        ext == "txt" || ext == "epub"
                    })
            })
            .map(|e| e.path().to_path_buf())
            .collect();
        
        if files_to_process.is_empty() {
            log_info(&tx, format!("在 {:?} 目录中未找到 .txt 或 .epub 文件。", book_dir));
            let _ = tx.send("__STATUS__:DONE".to_string());
            return;
        }

        log_info(&tx, format!("找到 {} 个文件，开始批量转换...", files_to_process.len()));

        let cli = Cli {
            list: false,
            file: None,
            dir: None,
            api: Some(api_url.clone()),
            out: out_dir,
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
            ignore_regex: req.ignore_regex.clone().unwrap_or_else(|| r"\*{3,}|#{2,}".to_string()),
            concurrency: 4,
            preserve_structure: req.preserve_structure,
            web: false,
        };

        // 初始化任务列表
        {
            let mut tasks_lock = tasks_state.lock().await;
            for path in &files_to_process {
                let id = format!("{:x}", md5::compute(path.to_string_lossy().as_bytes()));
                
                // 如果任务不存在，或者状态不是 completed/cancelled，则重置为 pending
                let should_reset = if let Some(task) = tasks_lock.get(&id) {
                    task.status != "completed" && task.status != "cancelled"
                } else {
                    true
                };

                if should_reset {
                    // 计算子目录结构，保持输出目录层级
                    let mut task_cli = cli.clone();
                    if req.preserve_structure {
                        if let Ok(relative) = path.strip_prefix(&book_dir) {
                            if let Some(parent) = relative.parent() {
                                if parent.components().count() > 0 {
                                    task_cli.out = task_cli.out.join(parent);
                                }
                            }
                        }
                    }

                    tasks_lock.insert(id.clone(), TaskState {
                        id,
                        file_name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                        full_path: Some(path.to_string_lossy().to_string()),
                        status: "pending".to_string(),
                        current: 0,
                        total: 0,
                        error_msg: None,
                        cli_config: Some(task_cli),
                        start_time: None,
                        end_time: None,
                        size: None,
                        eta: None,
                        created_at: Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64),
                        output_path: None,
                        is_hidden: false,
                    });
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
             let file_id = format!("{:x}", md5::compute(file_path.to_string_lossy().as_bytes()));
             
             // 检查任务状态，如果是 completed 或 cancelled 则跳过
             {
                 let tasks = tasks_state.lock().await;
                 if let Some(t) = tasks.get(&file_id) {
                     if t.status == "completed" || t.status == "cancelled" {
                         log_info(&tx, format!("跳过任务 ({}): {:?}", t.status, file_path.file_name().unwrap_or_default()));
                         continue;
                     }
                 }
             }

             // 更新状态为 processing
             {
                 let mut tasks = tasks_clone.lock().await;
                 if let Some(task) = tasks.get_mut(&file_id) {
                     task.status = "processing".to_string();
                     task.start_time = Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs());
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
                                     let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
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
                 async move {
                     let tasks = tasks.lock().await;
                     if let Some(t) = tasks.get(&fid) {
                         return t.status == "cancelled";
                     }
                     false
                 }
             };

             log_info(&tx, format!("▶️ 开始处理文件: {:?}", file_path.file_name().unwrap_or_default()));
             
             match process::process_file(&file_path, &cli, &client, &None, callback, check_cancel).await {
                Ok(_) => update_task_status(&tasks_state, &file_path, "completed", None).await,
                Err(e) => {
                    if e.to_string() == "任务已取消" {
                        update_task_status(&tasks_state, &file_path, "cancelled", None).await;
                    } else {
                        update_task_status(&tasks_state, &file_path, "error", Some(e.to_string())).await;
                    }
                }
             };
        }
        
        log_info(&tx, "所有文件处理完毕。".to_string());
        let _ = tx.send("__STATUS__:DONE".to_string());
    });
    
    {
        let mut handle_lock = state.task_handle.lock().await;
        if let Some(old_handle) = handle_lock.take() {
            old_handle.abort();
        }
        *handle_lock = Some(task.abort_handle());
    }

    (StatusCode::OK, Json(ApiResponse { success: true, message: "批量转换任务已在后台启动，请查看下方实时日志...".to_string() })).into_response()
}

async fn cancel_task_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CancelTaskRequest>,
) -> impl IntoResponse {
    let mut tasks = state.tasks.lock().await;
    if let Some(task) = tasks.get_mut(&req.id) {
        task.status = "cancelled".to_string();
        save_tasks_to_disk(&tasks).await;
    }
    (StatusCode::OK, Json(ApiResponse { success: true, message: "任务已取消".to_string() }))
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
                 return (StatusCode::BAD_REQUEST, Json(ApiResponse { success: false, message: "原文件不存在，无法重试".to_string() })).into_response();
            }

            let client = match ApiClient::new(cli.api.clone().unwrap_or_default()) {
                Ok(c) => c,
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: e.to_string() })).into_response(),
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
                    t.start_time = Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs());
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
                                         let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
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
                 async move {
                     let tasks = tasks.lock().await;
                     if let Some(t) = tasks.get(&fid) {
                         return t.status == "cancelled";
                     }
                     false
                 }
             };

                 log_info(&tx, format!("▶️ 重试任务: {:?}", path.file_name().unwrap_or_default()));
             match process::process_file(&path, &cli, &client, &None, callback, check_cancel).await {
                    Ok(_) => update_task_status(&tasks_state, &path, "completed", None).await,
                Err(e) => {
                    if e.to_string() == "任务已取消" {
                        update_task_status(&tasks_state, &path, "cancelled", None).await;
                    } else {
                        update_task_status(&tasks_state, &path, "error", Some(e.to_string())).await;
                    }
                }
                 };
            });

            return (StatusCode::OK, Json(ApiResponse { success: true, message: "任务已开始重试".to_string() })).into_response();
        }
    }
    (StatusCode::BAD_REQUEST, Json(ApiResponse { success: false, message: "任务无效或缺少配置信息".to_string() })).into_response()
}

async fn retry_all_failed_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let tasks_map = state.tasks.lock().await;
    let failed_tasks: Vec<(String, String, Cli)> = tasks_map.values()
        .filter(|t| t.status == "error" || t.status == "cancelled")
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
         return (StatusCode::OK, Json(ApiResponse { success: true, message: "没有需要重试的任务".to_string() })).into_response();
    }

    let tx = state.tx.clone();
    let tasks_state = state.tasks.clone();

    // 启动后台任务依次处理
    tokio::spawn(async move {
        log_info(&tx, format!("开始重试 {} 个失败/已取消的任务...", failed_tasks.len()));

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
                    t.start_time = Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs());
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
                                     let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
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
                 async move {
                     let tasks = tasks.lock().await;
                     if let Some(t) = tasks.get(&fid) {
                         return t.status == "cancelled";
                     }
                     false
                 }
             };

             log_info(&tx, format!("▶️ 重试任务: {:?}", path.file_name().unwrap_or_default()));
             match process::process_file(&path, &cli, &client, &None, callback, check_cancel).await {
                Ok(_) => update_task_status(&tasks_state, &path, "completed", None).await,
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

    (StatusCode::OK, Json(ApiResponse { success: true, message: "已启动后台重试任务".to_string() })).into_response()
}

async fn clear_all_tasks_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut tasks = state.tasks.lock().await;
    let mut ids_to_remove = Vec::new();

    for (id, task) in tasks.iter_mut() {
        if task.status == "completed" {
            // 已完成的任务：标记隐藏（保留记录以防自动检测重复添加）
            task.is_hidden = true;
        } else {
            // 未完成的任务：删除源文件、删除输出文件、删除记录
            if let Some(path_str) = &task.full_path {
                let path = PathBuf::from(path_str);
                if path.exists() {
                    let _ = tokio::fs::remove_file(path).await;
                }
            }
            if let Some(out_str) = &task.output_path {
                let out_path = PathBuf::from(out_str);
                if out_path.exists() {
                    let _ = tokio::fs::remove_dir_all(out_path).await;
                }
            }
            ids_to_remove.push(id.clone());
        }
    }

    for id in ids_to_remove {
        tasks.remove(&id);
    }

    save_tasks_to_disk(&tasks).await;
    (StatusCode::OK, Json(ApiResponse { success: true, message: "已清空所有任务".to_string() }))
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
    (StatusCode::OK, Json(ApiResponse { success: true, message: "已清除已完成任务".to_string() }))
}

async fn reset_history_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut tasks = state.tasks.lock().await;
    tasks.clear();
    save_tasks_to_disk(&tasks).await;
    (StatusCode::OK, Json(ApiResponse { success: true, message: "已彻底重置所有任务历史".to_string() }))
}

// --- 文件管理相关 Handler ---

fn get_safe_path(root: &str, sub_path: Option<&str>) -> Option<PathBuf> {
    let base = match root {
        "book" => if PathBuf::from("/book").exists() { PathBuf::from("/book") } else { PathBuf::from("book") },
        "output" => if PathBuf::from("/output").exists() { PathBuf::from("/output") } else { PathBuf::from("output") },
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
                let modified = meta.modified().unwrap_or(UNIX_EPOCH).duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
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
        None => return (StatusCode::BAD_REQUEST, Json(ApiResponse { success: false, message: "非法路径".to_string() })).into_response(),
    };

    if path.is_dir() {
        if let Err(e) = tokio::fs::remove_dir_all(path).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: format!("删除目录失败: {}", e) })).into_response();
        }
    } else {
        if let Err(e) = tokio::fs::remove_file(path).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: format!("删除文件失败: {}", e) })).into_response();
        }
    }

    (StatusCode::OK, Json(ApiResponse { success: true, message: "删除成功".to_string() })).into_response()
}

async fn download_file_handler(Query(req): Query<FileActionRequest>) -> impl IntoResponse {
    let path = match get_safe_path(&req.root, Some(&req.path)) {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, "非法路径".to_string()).into_response(),
    };

    if !path.exists() || !path.is_file() {
        return (StatusCode::NOT_FOUND, "文件不存在".to_string()).into_response();
    }

    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            let content_type = mime_guess::from_path(&path).first_or_octet_stream().to_string();
            (
                StatusCode::OK,
                [
                    ("Content-Type", content_type),
                    ("Content-Disposition", format!("attachment; filename=\"{}\"", filename)),
                ],
                bytes
            ).into_response()
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("读取文件失败: {}", e)).into_response()
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
            if let Ok(val) = field.text().await { target_dir_str = val; }
        } else if name == "path" {
            if let Ok(val) = field.text().await { sub_path = val; }
        } else if name == "file" {
            let file_name = field.file_name().unwrap_or("uploaded_file").to_string();
            let root_path = get_safe_path(&target_dir_str, Some(&sub_path)).unwrap_or_else(|| PathBuf::from("book"));
            
            if let Err(_) = tokio::fs::create_dir_all(&root_path).await {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: "无法创建目录".to_string() })).into_response();
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
        (StatusCode::OK, Json(ApiResponse { success: true, message: "上传成功".to_string() })).into_response()
    } else {
        (StatusCode::BAD_REQUEST, Json(ApiResponse { success: false, message: "上传失败".to_string() })).into_response()
    }
}

async fn update_task_status(tasks: &Arc<Mutex<HashMap<String, TaskState>>>, path: &PathBuf, status: &str, err: Option<String>) {
    let id = format!("{:x}", md5::compute(path.to_string_lossy().as_bytes()));
    let mut lock = tasks.lock().await;
    if let Some(task) = lock.get_mut(&id) {
        task.status = status.to_string();
        if let Some(e) = err {
            task.error_msg = Some(e);
        }
        if status == "completed" {
            task.current = task.total;
        }
        if status == "completed" || status == "error" || status == "cancelled" {
            task.end_time = Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs());
            task.eta = None;
        }
    }
    save_tasks_to_disk(&lock).await;
}

async fn save_tasks_to_disk(tasks: &HashMap<String, TaskState>) {
    let data_dir = if PathBuf::from("/data").exists() {
        PathBuf::from("/data")
    } else {
        PathBuf::from("data")
    };
    if !data_dir.exists() {
        let _ = tokio::fs::create_dir_all(&data_dir).await;
    }
    let path = data_dir.join("baitts_tasks.json");
    if let Ok(content) = serde_json::to_string_pretty(tasks) {
        let _ = tokio::fs::write(path, content).await;
    }
}

async fn load_tasks_from_disk() -> HashMap<String, TaskState> {
    let data_dir = if PathBuf::from("/data").exists() {
        PathBuf::from("/data")
    } else {
        PathBuf::from("data")
    };
    let path = data_dir.join("baitts_tasks.json");
    if path.exists() {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            if let Ok(tasks) = serde_json::from_str(&content) {
                return tasks;
            }
        }
    }
    HashMap::new()
}

// 新增：预览 Handler
async fn preview_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TtsRequest>,
) -> impl IntoResponse {
    // 简单的参数校验
    if req.text_content.is_none() || req.text_content.as_ref().unwrap().trim().is_empty() {
         return (StatusCode::BAD_REQUEST, Json(ApiResponse { success: false, message: "预览文本不能为空".to_string() })).into_response();
    }

    let client = match ApiClient::new(req.api_url.clone()) {
        Ok(c) => c,
        Err(e) => {
             return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: format!("创建客户端失败: {}", e) })).into_response();
        }
    };

    let text = req.text_content.unwrap();
    
    // 1. 文本切分逻辑 (复用 process.rs 中的逻辑)
    let dialogue_regex = Regex::new(r"“[^”]*”|「[^」]*」").unwrap();
    let ignore_regex_str = req.ignore_regex.as_deref().unwrap_or(r"\*{3,}|#{2,}");
    let ignore_regex = Regex::new(ignore_regex_str).unwrap_or_else(|_| Regex::new(r"\*{3,}|#{2,}").unwrap());
    
    struct BatchData {
        text: String,
        is_dialogue: bool,
    }
    
    let mut batches: Vec<BatchData> = Vec::new();
    let mut current_batch = BatchData { text: String::new(), is_dialogue: false };
    let mut is_batch_empty = true;
    const MAX_BATCH_CHARS: usize = 300;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        
        let processed_line = ignore_regex.replace_all(trimmed, "").to_string();
        if processed_line.trim().is_empty() { continue; }
        
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
            if seg_text.trim().is_empty() { continue; }

            if !is_batch_empty && current_batch.is_dialogue != is_dialogue {
                batches.push(current_batch);
                current_batch = BatchData { text: String::new(), is_dialogue };
            } else if is_batch_empty {
                current_batch.is_dialogue = is_dialogue;
            }

            if !current_batch.text.is_empty() && (current_batch.text.len() + seg_text.len() > MAX_BATCH_CHARS) {
                batches.push(current_batch);
                current_batch = BatchData { text: String::new(), is_dialogue };
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
                req.volume_dialogue.or(req.volume).or(Some(state.default_volume)),
                req.speed_dialogue.or(req.speed).or(Some(state.default_speed)),
                req.pitch_dialogue.or(req.pitch).or(Some(state.default_pitch)),
            )
        } else {
            (
                Some(req.voice_id.clone()),
                req.volume.or(Some(state.default_volume)),
                req.speed.or(Some(state.default_speed)),
                req.pitch.or(Some(state.default_pitch)),
            )
        };

        match client.generate_speech(&batch.text, &target_voice, &volume, &speed, &pitch).await {
            Ok(audio_data) => {
                if audio_data.is_empty() { continue; }
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
            },
            Err(e) => {
                 return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: format!("合成失败: {}", e) })).into_response();
            }
        }
    }

    if all_samples.is_empty() || wav_spec.is_none() {
         return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: "生成音频为空".to_string() })).into_response();
    }

    // 3. 重新编码为单个 WAV
    let spec = wav_spec.unwrap();
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut writer = match hound::WavWriter::new(&mut buffer, spec) {
             Ok(w) => w,
             Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: format!("创建 WAV Writer 失败: {}", e) })).into_response(),
        };
    
        for sample in all_samples {
            if writer.write_sample(sample).is_err() { break; }
        }
        if let Err(e) = writer.finalize() {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: format!("WAV Finalize 失败: {}", e) })).into_response();
        }
    }

    let final_wav_bytes = buffer.into_inner();
    (StatusCode::OK, [("Content-Type", "audio/wav")], final_wav_bytes).into_response()
}

async fn test_regex_handler(Json(req): Json<TestRegexRequest>) -> impl IntoResponse {
    match Regex::new(&req.regex) {
        Ok(re) => {
            let result = re.replace_all(&req.text, "").to_string();
            (StatusCode::OK, Json(TestRegexResponse { success: true, result: Some(result), error: None })).into_response()
        }
        Err(e) => {
            (StatusCode::OK, Json(TestRegexResponse { success: false, result: None, error: Some(e.to_string()) })).into_response()
        }
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

//... (synthesize_handler and synthesize_upload_handler remain mostly the same, but omitted for brevity)
async fn synthesize_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TtsRequest>
) -> impl IntoResponse {
    let tx = state.tx.clone();
    log_info(&tx, "接收到合成请求".to_string());
    
    let output_dir = PathBuf::from("output");
    
    let cli = Cli {
        list: false,
        file: None,
        dir: None,
        api: Some(req.api_url.clone()),
        out: output_dir.clone(),
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
        ignore_regex: req.ignore_regex.clone().unwrap_or_else(|| r"\*{3,}|#{2,}".to_string()),
        concurrency: req.concurrency.unwrap_or(4),
        preserve_structure: false,
        web: false,
    };

    let client = match ApiClient::new(req.api_url.clone()) {
        Ok(c) => c,
        Err(e) => {
            let err_msg = format!("创建 API 客户端失败: {}", e);
            log_error(&tx, err_msg.clone());
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: err_msg })).into_response();
        }
    };

    if let Some(text) = req.text_content {
        let file_name = req.output_name.unwrap_or_else(|| "web_task".to_string());
        if let Err(e) = std::fs::create_dir_all(&output_dir) {
            let err_msg = format!("无法创建输出目录: {}", e);
            log_error(&tx, err_msg.clone());
             return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: err_msg })).into_response();
        }
        let temp_path = output_dir.join(format!("{}.txt", file_name));
        
        if let Err(e) = std::fs::write(&temp_path, text) {
            let err_msg = format!("无法写入临时文件: {}", e);
            log_error(&tx, err_msg.clone());
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: err_msg })).into_response();
        }
        
        let file_id = format!("{:x}", md5::compute(temp_path.to_string_lossy().as_bytes()));
        
        // 添加到任务列表
        {
            let mut tasks = state.tasks.lock().await;
            tasks.insert(file_id.clone(), TaskState {
                id: file_id.clone(),
                file_name: file_name.clone(),
                full_path: Some(temp_path.to_string_lossy().to_string()),
                status: "processing".to_string(),
                current: 0,
                total: 0,
                error_msg: None,
                cli_config: Some(cli.clone()),
                start_time: Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()),
                end_time: None,
                size: None,
                eta: None,
                created_at: Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64),
                output_path: None,
                is_hidden: false,
            });
            save_tasks_to_disk(&*tasks).await;
        }

        let tx_clone = tx.clone();
        let tasks_state = state.tasks.clone();
        let fid_cb = file_id.clone();

        let task = tokio::spawn(async move {
            let callback_tx = tx_clone.clone();
            let tasks_clone = tasks_state.clone();
            
            let callback = move |event: process::ProcessEvent| {
                match event {
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
                    },
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
            
            log_info(&tx_clone, "后台任务启动: 处理文本...".to_string());
            match process::process_file(&temp_path, &cli, &client, &Option::<Regex>::None, callback, || async { false }).await {
                Ok(_) => {
                    log_info(&tx_clone, "后台任务完成: 文本处理完毕。".to_string());
                    update_task_status(&tasks_state, &temp_path, "completed", None).await;
                    let _ = std::fs::remove_file(temp_path);
                },
                Err(e) => {
                    log_error(&tx_clone, format!("后台任务出错: {}", e));
                    update_task_status(&tasks_state, &temp_path, "error", Some(e.to_string())).await;
                }
            }
            let _ = tx_clone.send("__STATUS__:DONE".to_string());
        });
        
        {
            let mut handle_lock = state.task_handle.lock().await;
            if let Some(old_handle) = handle_lock.take() {
                old_handle.abort();
            }
            *handle_lock = Some(task.abort_handle());
        }

        (StatusCode::OK, Json(ApiResponse { success: true, message: "任务已在后台启动，请查看下方实时日志...".to_string() })).into_response()

    } else if let Some(path_str) = req.file_path {
        let path = PathBuf::from(path_str);
        if !path.exists() {
            let err_msg = "文件不存在".to_string();
            log_error(&tx, err_msg.clone());
             return (StatusCode::BAD_REQUEST, Json(ApiResponse { success: false, message: err_msg })).into_response();
        }
        
        let file_id = format!("{:x}", md5::compute(path.to_string_lossy().as_bytes()));
        
        // 添加到任务列表
        {
            let mut tasks = state.tasks.lock().await;
            tasks.insert(file_id.clone(), TaskState {
                id: file_id.clone(),
                file_name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                full_path: Some(path.to_string_lossy().to_string()),
                status: "processing".to_string(),
                current: 0,
                total: 0,
                error_msg: None,
                cli_config: Some(cli.clone()),
                start_time: Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()),
                end_time: None,
                size: None,
                eta: None,
                created_at: Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64),
                output_path: None,
                is_hidden: false,
            });
            save_tasks_to_disk(&*tasks).await;
        }

        let tx_clone = tx.clone();
        let tasks_state = state.tasks.clone();
        let fid_cb = file_id.clone();

        let task = tokio::spawn(async move {
            let callback_tx = tx_clone.clone();
            let tasks_clone = tasks_state.clone();
            
            let callback = move |event: process::ProcessEvent| {
                match event {
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
                    },
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

            log_info(&tx_clone, format!("后台任务启动: 处理本地文件 {:?}...", path));
            match process::process_file(&path, &cli, &client, &Option::<Regex>::None, callback, || async { false }).await {
                Ok(_) => {
                    log_info(&tx_clone, format!("后台任务完成: {:?} 处理完毕。", path));
                    update_task_status(&tasks_state, &path, "completed", None).await;
                    if let Err(e) = std::fs::remove_file(&path) {
                        log_error(&tx_clone, format!("无法删除上传文件: {}", e));
                    }
                },
                Err(e) => {
                    log_error(&tx_clone, format!("后台任务出错: {}", e));
                    update_task_status(&tasks_state, &path, "error", Some(e.to_string())).await;
                }
            }
            let _ = tx_clone.send("__STATUS__:DONE".to_string());
        });

        {
            let mut handle_lock = state.task_handle.lock().await;
            if let Some(old_handle) = handle_lock.take() {
                old_handle.abort();
            }
            *handle_lock = Some(task.abort_handle());
        }

        (StatusCode::OK, Json(ApiResponse { success: true, message: "任务已在后台启动，请查看下方实时日志...".to_string() })).into_response()
    } else {
        let err_msg = "必须提供文本或文件路径".to_string();
        log_error(&tx, err_msg.clone());
        (StatusCode::BAD_REQUEST, Json(ApiResponse { success: false, message: err_msg })).into_response()
    }
}

async fn synthesize_upload_handler(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart
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
    let mut uploaded_file_path: Option<PathBuf> = None;

    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let name = field.name().unwrap_or("").to_string();
        
        if name == "file" {
            let file_name = field.file_name().unwrap_or("uploaded_file").to_string();
            
            if let Err(e) = std::fs::create_dir_all(&upload_dir) {
                let err_msg = format!("无法创建上传目录: {}", e);
                log_error(&tx, err_msg.clone());
                 return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: err_msg })).into_response();
            }
            
            let temp_path = upload_dir.join(&file_name);
            let mut file = match File::create(&temp_path).await {
                Ok(f) => f,
                Err(e) => {
                    let err_msg = format!("无法创建临时文件: {}", e);
                    log_error(&tx, err_msg.clone());
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: err_msg })).into_response();
                }
            };

            let mut stream = field;
            while let Some(chunk) = stream.chunk().await.unwrap_or(None) {
                if let Err(e) = file.write_all(&chunk).await {
                    let err_msg = format!("写入临时文件失败: {}", e);
                    log_error(&tx, err_msg.clone());
                     return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: err_msg })).into_response();
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
                _ => {}
            }
        }
    }

    if api_url.is_empty() || voice_id.is_empty() {
        let err_msg = "缺少必要的参数 (api_url, voice_id)".to_string();
        log_error(&tx, err_msg.clone());
        return (StatusCode::BAD_REQUEST, Json(ApiResponse { success: false, message: err_msg })).into_response();
    }

    if let Some(path) = uploaded_file_path {
        let cli = Cli {
            list: false,
            file: None,
            dir: None,
            api: Some(api_url.clone()),
            out: output_dir.clone(),
            voice: Some(voice_id.clone()),
            voice_dialogue: if voice_dialogue_id.is_empty() { None } else { Some(voice_dialogue_id) },
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
            web: false,
        };

        let client = match ApiClient::new(api_url) {
            Ok(c) => c,
            Err(e) => {
                let err_msg = format!("创建 API 客户端失败: {}", e);
                log_error(&tx, err_msg.clone());
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse { success: false, message: err_msg })).into_response();
            }
        };

        let file_id = format!("{:x}", md5::compute(path.to_string_lossy().as_bytes()));
        
        // 添加到任务列表
        {
            let mut tasks = state.tasks.lock().await;
            tasks.insert(file_id.clone(), TaskState {
                id: file_id.clone(),
                file_name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                full_path: Some(path.to_string_lossy().to_string()),
                status: "processing".to_string(),
                current: 0,
                total: 0,
                error_msg: None,
                cli_config: Some(cli.clone()),
                start_time: Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()),
                end_time: None,
                size: None,
                eta: None,
                created_at: Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64),
                output_path: None,
                is_hidden: false,
            });
            save_tasks_to_disk(&*tasks).await;
        }

        let tx_clone = tx.clone();
        let tasks_state = state.tasks.clone();
        let fid_cb = file_id.clone();

        let task = tokio::spawn(async move {
            let callback_tx = tx_clone.clone();
            let tasks_clone = tasks_state.clone();
            
            let callback = move |event: process::ProcessEvent| {
                match event {
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
                    },
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

            log_info(&tx_clone, format!("后台任务启动: 处理上传文件 {:?}...", path));
            match process::process_file(&path, &cli, &client, &Option::<Regex>::None, callback, || async { false }).await {
                Ok(_) => {
                    log_info(&tx_clone, format!("后台任务完成: {:?} 处理完毕。", path));
                    update_task_status(&tasks_state, &path, "completed", None).await;
                },
                Err(e) => {
                    log_error(&tx_clone, format!("后台任务出错: {}", e));
                    update_task_status(&tasks_state, &path, "error", Some(e.to_string())).await;
                }
            }
            let _ = tx_clone.send("__STATUS__:DONE".to_string());
        });

        {
            let mut handle_lock = state.task_handle.lock().await;
            if let Some(old_handle) = handle_lock.take() {
                old_handle.abort();
            }
            *handle_lock = Some(task.abort_handle());
        }

        (StatusCode::OK, Json(ApiResponse { success: true, message: "文件上传成功，转换任务已在后台启动！请查看下方实时日志...".to_string() })).into_response()
    } else {
        let err_msg = "未上传文件".to_string();
        log_error(&tx, err_msg.clone());
        (StatusCode::BAD_REQUEST, Json(ApiResponse { success: false, message: err_msg })).into_response()
    }
}
