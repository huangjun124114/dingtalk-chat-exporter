// 定时导出功能开发中：cron/schedule 模块将被 scheduler（后台调度引擎）使用
#![allow(dead_code)]

// 钉钉群聊导出器 - Tauri 后端

mod cron;
mod date;
mod dws;
mod export_log;
mod exporter;
mod media;
mod schedule;
mod scheduler;
mod settings;
mod viewer;

pub use exporter::stable_hash;
use exporter::GroupExportRequest;

use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::State;
use tauri_plugin_dialog::DialogExt;

const MAX_LOG_LINES: usize = 500;

#[tauri::command]
fn get_app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub struct AppState {
    pub(crate) inner: Arc<Mutex<AppInner>>,
    pub(crate) cancel_requested: Arc<AtomicBool>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(AppInner::default())),
            cancel_requested: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Default)]
struct AppInner {
    auth: Option<dws::AuthInfo>,
    task: Option<TaskState>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSnapshot {
    pub authed: bool,
    pub auth_info: Option<dws::AuthInfo>,
    pub task: Option<TaskState>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskState {
    pub kind: String,
    pub status: String, // running | done | error | cancelled
    pub progress_text: String,
    pub log: Vec<String>,
    pub log_start: usize,
    pub output_path: Option<String>,
    pub error: Option<String>,
}

#[tauri::command]
async fn check_dws_installed() -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(|| dws::find_dws().is_some())
        .await
        .map_err(|error| format!("检测 dws 失败: {error}"))
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    let parsed = url::Url::parse(&url).map_err(|e| format!("无效 URL: {}", e))?;
    if parsed.scheme() != "https" {
        return Err("仅允许打开 HTTPS 链接".into());
    }

    #[cfg(target_os = "macos")]
    Command::new("open")
        .arg(&url)
        .spawn()
        .map_err(|e| format!("打开浏览器失败: {}", e))?;

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", &url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("打开浏览器失败: {}", e))?;
    }

    #[cfg(target_os = "linux")]
    Command::new("xdg-open")
        .arg(&url)
        .spawn()
        .map_err(|e| format!("打开浏览器失败: {}", e))?;

    Ok(())
}

#[tauri::command]
async fn check_env(state: State<'_, AppState>) -> Result<AppSnapshot, String> {
    let auth_result = tauri::async_runtime::spawn_blocking(dws::check_auth)
        .await
        .map_err(|error| format!("检查登录状态失败: {error}"))?;
    match auth_result {
        Ok(info) => {
            let mut inner = state.inner.lock().map_err(lock_error)?;
            inner.auth = Some(info);
            Ok(build_snapshot(&inner, None))
        }
        Err(error) => {
            let mut inner = state.inner.lock().map_err(lock_error)?;
            inner.auth = None;
            Err(error)
        }
    }
}

#[tauri::command]
async fn login_dws() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(launch_login_dws)
        .await
        .map_err(|error| format!("打开登录窗口失败: {error}"))?
}

fn launch_login_dws() -> Result<(), String> {
    let dws_path = dws::find_dws().ok_or("未找到 dws 命令")?;

    #[cfg(target_os = "macos")]
    {
        let command = format!("{} auth login", shell_quote(&dws_path));
        let apple_script_command = command.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
            apple_script_command
        );
        Command::new("osascript")
            .args(["-e", &script])
            .spawn()
            .map_err(|e| format!("打开终端失败: {}", e))?;
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        Command::new(&dws_path)
            .args(["auth", "login"])
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .map_err(|e| format!("打开登录窗口失败: {}", e))?;
    }

    #[cfg(target_os = "linux")]
    {
        Command::new("x-terminal-emulator")
            .args(["-e", &dws_path, "auth", "login"])
            .spawn()
            .or_else(|_| {
                Command::new("gnome-terminal")
                    .args(["--", &dws_path, "auth", "login"])
                    .spawn()
            })
            .map_err(|e| format!("打开终端失败: {}", e))?;
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn executable_output_dir(executable: &Path) -> Option<PathBuf> {
    let executable_dir = executable.parent()?;
    let contents_dir = executable_dir.parent();
    let bundle_dir = contents_dir.and_then(Path::parent);

    if executable_dir.file_name() == Some(std::ffi::OsStr::new("MacOS"))
        && contents_dir.and_then(Path::file_name) == Some(std::ffi::OsStr::new("Contents"))
        && bundle_dir.and_then(Path::extension) == Some(std::ffi::OsStr::new("app"))
    {
        return bundle_dir.and_then(Path::parent).map(Path::to_path_buf);
    }

    Some(executable_dir.to_path_buf())
}

fn is_app_translocation_path(path: &Path) -> bool {
    let path_text = path.to_string_lossy();
    (path_text.starts_with("/private/var/folders/") || path_text.starts_with("/var/folders/"))
        && path
            .components()
            .any(|component| component.as_os_str() == "AppTranslocation")
}

fn default_output_dir(
    executable: Option<&Path>,
    current_dir: Option<&Path>,
    home_dir: Option<&Path>,
) -> PathBuf {
    if let Some(executable_dir) = executable.and_then(executable_output_dir) {
        if !is_app_translocation_path(&executable_dir) {
            return executable_dir;
        }

        // Gatekeeper 的 App Translocation 只暴露随机只读挂载路径，无法通过公开 API
        // 稳定反查原始 .app 位置。此时使用用户下载目录，避免输出到会消失的临时目录。
        if let Some(home_dir) = home_dir {
            return home_dir.join("Downloads");
        }
    }

    current_dir
        .filter(|path| !is_app_translocation_path(path))
        .map(Path::to_path_buf)
        .or_else(|| home_dir.map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 默认导出到应用或 EXE 所在文件夹；macOS 隔离运行时改用用户下载目录。
#[tauri::command]
fn get_default_output_dir() -> String {
    let executable = std::env::current_exe().ok();
    let current_dir = std::env::current_dir().ok();
    let home_dir = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    default_output_dir(
        executable.as_deref(),
        current_dir.as_deref(),
        home_dir.as_deref(),
    )
    .to_string_lossy()
    .into_owned()
}

#[tauri::command]
async fn search_groups(query: String) -> Result<Vec<dws::GroupInfo>, String> {
    let query = query.trim().to_string();
    if query.is_empty() {
        return Err("请输入群名称关键词".into());
    }
    tauri::async_runtime::spawn_blocking(move || dws::search_groups(&query))
        .await
        .map_err(|error| format!("搜索群任务失败: {error}"))?
}

#[tauri::command]
async fn choose_output_dir(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let dialog_app = app.clone();
    let selected = tauri::async_runtime::spawn_blocking(move || {
        dialog_app.dialog().file().blocking_pick_folder()
    })
    .await
    .map_err(|e| format!("选择目录失败: {}", e))?;
    Ok(selected.map(|path| path.to_string()))
}

#[tauri::command]
async fn export_diagnostic_log(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    suggested_dir: Option<String>,
) -> Result<Option<String>, String> {
    let snapshot = {
        let inner = state.inner.lock().map_err(lock_error)?;
        build_snapshot(&inner, None)
    };
    let dialog_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<Option<String>, String> {
        let home_paths = diagnostic_home_paths();
        let dws_path = dws::find_dws();
        let dws_version =
            dws::diagnostic_version().unwrap_or_else(|error| format!("无法读取: {}", error));
        let report = build_diagnostic_report(
            &snapshot,
            dws_path.as_deref(),
            &dws_version,
            &home_paths,
            &dws::current_time_str(),
        );

        let mut dialog = dialog_app
            .dialog()
            .file()
            .set_title("保存诊断日志")
            .set_file_name("dingtalk-chat-exporter-diagnostic.txt")
            .add_filter("文本日志", &["txt"]);
        if let Some(directory) = suggested_dir
            .as_deref()
            .map(Path::new)
            .filter(|path| path.is_dir())
        {
            dialog = dialog.set_directory(directory);
        }
        let Some(file_path) = dialog.blocking_save_file() else {
            return Ok(None);
        };
        let path = file_path
            .into_path()
            .map_err(|error| format!("诊断日志保存路径无效: {}", error))?;
        fs::write(&path, report)
            .map_err(|error| format!("写入诊断日志 {} 失败: {}", path.display(), error))?;
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
    .map_err(|error| format!("导出诊断日志失败: {}", error))?
}

#[tauri::command]
fn export_groups(
    state: State<'_, AppState>,
    groups: Vec<GroupExportRequest>,
    output_root: String,
    start_time: Option<String>,
    end_time: Option<String>,
) -> Result<(), String> {
    if groups.is_empty() {
        return Err("请至少选择一个群".into());
    }
    if output_root.trim().is_empty() {
        return Err("请选择有效的输出目录".into());
    }
    if groups
        .iter()
        .any(|group| group.open_conversation_id.trim().is_empty())
    {
        return Err("群会话 ID 不能为空".into());
    }
    // 验证时间格式
    let start_time = start_time
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            let value = value.trim().to_string();
            crate::date::parse_dws_datetime(&value)
                .map(|_| value)
                .ok_or_else(|| "开始时间格式无效，预期格式为 yyyy-MM-dd HH:mm:ss".to_string())
        })
        .transpose()?;
    let end_time = end_time
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            let value = value.trim().to_string();
            crate::date::parse_dws_datetime(&value)
                .map(|_| value)
                .ok_or_else(|| "结束时间格式无效，预期格式为 yyyy-MM-dd HH:mm:ss".to_string())
        })
        .transpose()?;
    // 验证时间范围
    if let (Some(ref start), Some(ref end)) = (&start_time, &end_time) {
        if end < start {
            return Err("结束时间不能早于开始时间".into());
        }
    }
    if let Some(ref end) = end_time {
        if end > &dws::current_time_str() {
            return Err("结束时间不能晚于当前北京时间".into());
        }
    }

    let self_name = {
        let mut inner = state.inner.lock().map_err(lock_error)?;
        if inner
            .task
            .as_ref()
            .is_some_and(|task| task.status == "running")
        {
            return Err("已有导出任务正在运行".into());
        }
        let self_name = inner
            .auth
            .as_ref()
            .map(|auth| auth.user_name.clone())
            .ok_or("dws 尚未登录，请重新检测登录状态")?;
        state.cancel_requested.store(false, Ordering::Relaxed);
        inner.task = Some(TaskState {
            kind: "export".into(),
            status: "running".into(),
            progress_text: format!("准备导出 {} 个群", groups.len()),
            log: Vec::new(),
            log_start: 0,
            output_path: None,
            error: None,
        });
        self_name
    };

    let state_inner = state.inner.clone();
    let cancel_requested = state.cancel_requested.clone();
    std::thread::spawn(move || {
        run_export(
            state_inner,
            cancel_requested,
            groups,
            output_root,
            self_name,
            start_time,
            end_time,
        );
    });
    Ok(())
}

#[tauri::command]
fn cancel_export(state: State<'_, AppState>) -> Result<(), String> {
    let mut inner = state.inner.lock().map_err(lock_error)?;
    let task = inner.task.as_mut().ok_or("当前没有导出任务")?;
    if task.status != "running" {
        return Err("当前没有正在运行的导出任务".into());
    }
    state.cancel_requested.store(true, Ordering::Relaxed);
    task.progress_text = "正在取消导出…".into();
    Ok(())
}

#[tauri::command]
fn snapshot(state: State<'_, AppState>, log_from: Option<usize>) -> AppSnapshot {
    match state.inner.lock() {
        Ok(inner) => build_snapshot(&inner, log_from),
        Err(_) => AppSnapshot {
            authed: false,
            auth_info: None,
            task: Some(TaskState {
                kind: "export".into(),
                status: "error".into(),
                progress_text: "应用状态异常".into(),
                log: Vec::new(),
                log_start: 0,
                output_path: None,
                error: Some("应用状态锁已损坏，请重启程序".into()),
            }),
        },
    }
}

#[tauri::command]
fn open_output(path: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    if !path.is_dir() {
        return Err(format!("输出目录不存在: {}", path.display()));
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        Command::new("explorer.exe")
            .arg(&path)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("打开输出目录失败: {}", e))?;
    }

    #[cfg(target_os = "macos")]
    Command::new("open")
        .arg(&path)
        .spawn()
        .map_err(|e| format!("打开输出目录失败: {}", e))?;

    #[cfg(target_os = "linux")]
    Command::new("xdg-open")
        .arg(&path)
        .spawn()
        .map_err(|e| format!("打开输出目录失败: {}", e))?;

    Ok(())
}

fn run_export(
    state: Arc<Mutex<AppInner>>,
    cancel_requested: Arc<AtomicBool>,
    groups: Vec<GroupExportRequest>,
    output_root: String,
    self_name: String,
    start_time: Option<String>,
    end_time: Option<String>,
) {
    let job = exporter::ExportJob {
        groups,
        output_root: output_root.clone(),
        self_name,
        start_time,
        end_time,
        trigger: exporter::Trigger::Manual,
        archive: Box::new(exporter::PerRunArchive),
        cancel: cancel_requested,
    };
    let progress = TaskProgress {
        state: state.clone(),
    };
    let outcome = exporter::run_job(&job, &progress);

    match outcome.status.as_str() {
        "cancelled" => {
            append_log(&state, "导出已由用户取消；已导出内容保留在对应目录中");
            finish_task(
                &state,
                "cancelled",
                "导出已取消".into(),
                None,
                Some(output_root),
            );
        }
        "done" => finish_task(
            &state,
            "done",
            format!("导出完成，共 {} 个群", outcome.groups.len()),
            None,
            Some(output_root),
        ),
        _ => finish_task(
            &state,
            "error",
            format!("导出结束，有 {} 个错误", outcome.all_errors.len()),
            Some(outcome.all_errors.join("\n")),
            Some(output_root),
        ),
    }
}

/// 将 exporter 核心的进度/日志回写到 Tauri 任务状态
struct TaskProgress {
    state: Arc<Mutex<AppInner>>,
}

impl exporter::ExportProgress for TaskProgress {
    fn log(&self, message: &str) {
        append_log(&self.state, message);
    }

    fn progress(&self, text: String) {
        set_task_progress(&self.state, text);
    }

    fn log_snapshot(&self) -> Vec<String> {
        self.state
            .lock()
            .ok()
            .and_then(|inner| inner.task.as_ref().map(|task| task.log.clone()))
            .unwrap_or_default()
    }
}

fn append_log(state: &Arc<Mutex<AppInner>>, message: &str) {
    if let Ok(mut inner) = state.lock() {
        if let Some(task) = &mut inner.task {
            task.log.push(message.to_string());
            if task.log.len() > MAX_LOG_LINES {
                let remove_count = task.log.len() - MAX_LOG_LINES;
                task.log.drain(0..remove_count);
                task.log_start += remove_count;
            }
        }
    }
}

fn set_task_progress(state: &Arc<Mutex<AppInner>>, text: String) {
    if let Ok(mut inner) = state.lock() {
        if let Some(task) = &mut inner.task {
            task.progress_text = text;
        }
    }
}

fn finish_task(
    state: &Arc<Mutex<AppInner>>,
    status: &str,
    progress_text: String,
    error: Option<String>,
    output_path: Option<String>,
) {
    if let Ok(mut inner) = state.lock() {
        if let Some(task) = &mut inner.task {
            task.status = status.into();
            task.progress_text = progress_text;
            task.error = error;
            task.output_path = output_path;
        }
    }
}

fn diagnostic_home_paths() -> Vec<String> {
    let mut paths: Vec<String> = ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .filter(|path| !path.is_empty())
        .collect();
    paths.sort_by_key(|path| std::cmp::Reverse(path.len()));
    paths.dedup();
    paths
}

fn sanitize_diagnostic_text(text: &str, home_paths: &[String]) -> String {
    let lower = text.to_ascii_lowercase();
    let normalized: String = lower
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect();
    let sensitive_markers = [
        "accesstoken",
        "refreshtoken",
        "clientsecret",
        "privatetoken",
        "password",
        "passwd",
        "apikey",
        "authorization",
        "bearer",
        "cookie",
        "setcookie",
        "authtoken",
        "sessiontoken",
        "sessionid",
        "signature",
        "credential",
    ];
    if sensitive_markers
        .iter()
        .any(|marker| normalized.contains(marker))
        || lower.contains("token=")
        || lower.contains("token:")
        || lower.contains("\"token\"")
        || lower.contains("'token'")
    {
        return "[已脱敏：该日志行包含凭据字段]".into();
    }

    let mut sanitized = text.to_string();
    for home_path in home_paths {
        sanitized = sanitized.replace(home_path, "<HOME>");
    }
    sanitized
}

fn build_diagnostic_report(
    snapshot: &AppSnapshot,
    dws_path: Option<&str>,
    dws_version: &str,
    home_paths: &[String],
    generated_at: &str,
) -> String {
    let sanitize = |text: &str| sanitize_diagnostic_text(text, home_paths);
    let mut lines = vec![
        "钉钉群聊导出器诊断日志".to_string(),
        "说明: 不包含聊天正文或附件内容；敏感字段已自动整行脱敏，发送前仍请人工复核。".to_string(),
        String::new(),
        format!("generatedAt: {}", generated_at),
        format!("appVersion: {}", env!("CARGO_PKG_VERSION")),
        format!(
            "platform: {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        format!("dwsPath: {}", sanitize(dws_path.unwrap_or("未找到"))),
        format!("dwsVersion: {}", sanitize(dws_version)),
        format!("authenticated: {}", snapshot.authed),
    ];

    if let Some(auth) = &snapshot.auth_info {
        lines.push(format!("organization: {}", sanitize(&auth.corp_name)));
        lines.push(format!("user: {}", sanitize(&auth.user_name)));
    }

    lines.push(String::new());
    lines.push("[任务状态]".into());
    if let Some(task) = &snapshot.task {
        lines.push(format!("kind: {}", sanitize(&task.kind)));
        lines.push(format!("status: {}", sanitize(&task.status)));
        lines.push(format!("progress: {}", sanitize(&task.progress_text)));
        lines.push(format!(
            "outputPath: {}",
            sanitize(task.output_path.as_deref().unwrap_or("无"))
        ));
        lines.push(format!(
            "error: {}",
            sanitize(task.error.as_deref().unwrap_or("无"))
        ));
        lines.push(String::new());
        lines.push("[任务日志]".into());
        if task.log_start > 0 {
            lines.push(format!("…较早的 {} 行日志已从内存中淘汰…", task.log_start));
        }
        if task.log.is_empty() {
            lines.push("无".into());
        } else {
            for (index, line) in task.log.iter().enumerate() {
                lines.push(format!(
                    "{:04} {}",
                    task.log_start + index + 1,
                    sanitize(line)
                ));
            }
        }
    } else {
        lines.push("尚无导出任务".into());
    }

    lines.push(String::new());
    lines.join("\n")
}

fn build_snapshot(inner: &AppInner, log_from: Option<usize>) -> AppSnapshot {
    let task = inner.task.as_ref().map(|task| {
        let available_end = task.log_start + task.log.len();
        let requested_start = log_from
            .unwrap_or(task.log_start)
            .clamp(task.log_start, available_end);
        TaskState {
            kind: task.kind.clone(),
            status: task.status.clone(),
            progress_text: task.progress_text.clone(),
            log: task.log[requested_start - task.log_start..].to_vec(),
            log_start: requested_start,
            output_path: task.output_path.clone(),
            error: task.error.clone(),
        }
    });
    AppSnapshot {
        authed: inner.auth.is_some(),
        auth_info: inner.auth.clone(),
        task,
    }
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> String {
    "应用状态异常，请重启程序".into()
}

#[tauri::command]
fn get_settings() -> Result<settings::AppSettings, String> {
    settings::load_settings()
}

#[tauri::command]
fn save_settings(settings: settings::AppSettings) -> Result<(), String> {
    settings::save_settings(&settings)
}

#[tauri::command]
fn list_export_logs(output_root: String) -> Result<Vec<export_log::ExportLogEntry>, String> {
    export_log::list_all_export_logs(Path::new(&output_root))
}

#[tauri::command]
fn get_diagnostics_info() -> Result<serde_json::Value, String> {
    let app_version = env!("CARGO_PKG_VERSION").to_string();
    let default_dir = get_default_output_dir();
    let auth = dws::check_auth().ok();
    let dws_version = dws::diagnostic_version().unwrap_or_else(|_| "unknown".into());
    let dws_path = dws::find_dws().unwrap_or_else(|| "not found".into());
    let settings_path = settings::settings_file_path().to_string_lossy().to_string();

    Ok(serde_json::json!({
        "appVersion": app_version,
        "defaultOutputDir": default_dir,
        "settingsFile": settings_path,
        "dws": {
            "path": dws_path,
            "version": dws_version,
            "authenticated": auth.is_some(),
            "userName": auth.as_ref().map(|a| a.user_name.clone()),
            "corpName": auth.as_ref().map(|a| a.corp_name.clone()),
        },
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    }))
}

// ===== 定时导出任务命令 =====

/// 定时任务视图：在持久化模型基础上附加自然语言描述（由后端 describe 统一生成，避免前端重复实现）
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScheduleView {
    #[serde(flatten)]
    schedule: schedule::Schedule,
    description: String,
}

impl ScheduleView {
    fn from(schedule: schedule::Schedule) -> Self {
        let description = schedule.schedule.describe();
        ScheduleView {
            schedule,
            description,
        }
    }
}

#[tauri::command]
fn list_schedules() -> Result<Vec<ScheduleView>, String> {
    let _guard = scheduler::store_guard();
    let store = schedule::load_store()?;
    Ok(store
        .schedules
        .into_iter()
        .map(ScheduleView::from)
        .collect())
}

#[tauri::command]
fn get_schedule_runs(id: String) -> Result<Vec<schedule::ScheduleRun>, String> {
    let _guard = scheduler::store_guard();
    let store = schedule::load_store()?;
    let schedule = store.find(&id).ok_or("定时任务不存在")?;
    Ok(schedule.runs.clone())
}

/// 预览未来 3 次触发时间（尊重调度开始时间下限）
#[tauri::command]
fn preview_schedule(config: schedule::ScheduleConfig) -> Result<Vec<String>, String> {
    let cron = config.validate()?;
    let now = dws::current_time_str();
    let first = schedule::compute_next_run(&config, &now).ok_or("cron 表达式无有效触发时间")?;
    let mut result = vec![first.clone()];
    let mut cursor = first;
    for _ in 0..2 {
        match cron.next_after(&cursor) {
            Some(next) => {
                cursor = next.clone();
                result.push(next);
            }
            None => break,
        }
    }
    Ok(result)
}

/// 仅校验（cron 合法性 + 同群冲突），不落盘；界面创建时即时提示
#[tauri::command]
fn validate_schedule(schedule: schedule::Schedule) -> Result<(), String> {
    let _guard = scheduler::store_guard();
    let store = schedule::load_store()?;
    store.validate(&schedule)
}

/// 保存（新建/编辑）任务：校验 → 保留既有水位线与运行历史 → 重算下次触发 → 落盘
#[tauri::command]
fn save_schedule(mut schedule: schedule::Schedule) -> Result<ScheduleView, String> {
    let _guard = scheduler::store_guard();
    let mut store = schedule::load_store()?;

    let is_new = schedule.id.trim().is_empty();
    if is_new {
        schedule.id = schedule::generate_schedule_id(&dws::current_time_str());
    }

    // 校验：cron + 时间格式 + 同群冲突（编辑自身不算冲突）
    store.validate(&schedule)?;

    // 编辑既有任务：保留运行态字段（水位线、运行历史、计数），只更新配置
    if let Some(existing) = store.find(&schedule.id) {
        schedule.last_success_at = existing.last_success_at.clone();
        schedule.last_run_at = existing.last_run_at.clone();
        schedule.run_count = existing.run_count;
        schedule.runs = existing.runs.clone();
    }

    // 配置可能已变更，统一从当前时间重算下次触发
    let now = dws::current_time_str();
    schedule.next_run_at = schedule::compute_next_run(&schedule.schedule, &now);

    store.upsert(schedule.clone());
    schedule::save_store(&store)?;
    Ok(ScheduleView::from(schedule))
}

#[tauri::command]
fn delete_schedule(id: String) -> Result<(), String> {
    let _guard = scheduler::store_guard();
    let mut store = schedule::load_store()?;
    if !store.remove(&id) {
        return Err("定时任务不存在".into());
    }
    schedule::save_store(&store)
}

#[tauri::command]
fn toggle_schedule(id: String, enabled: bool) -> Result<(), String> {
    let _guard = scheduler::store_guard();
    let mut store = schedule::load_store()?;
    let now = dws::current_time_str();
    let schedule = store.find_mut(&id).ok_or("定时任务不存在")?;
    schedule.enabled = enabled;
    if enabled {
        schedule.next_run_at = schedule::compute_next_run(&schedule.schedule, &now);
    }
    schedule::save_store(&store)
}

/// 立即运行一次（不受 enabled / next_run_at 限制），区间从水位线到当前时刻。
/// 同步预检任务存在与忙闲，随后在独立线程执行（与手动导出共享任务槽）。
#[tauri::command]
fn run_schedule_now(state: State<'_, AppState>, id: String) -> Result<(), String> {
    {
        let _guard = scheduler::store_guard();
        let store = schedule::load_store()?;
        if store.find(&id).is_none() {
            return Err("定时任务不存在".into());
        }
    }
    {
        let inner = state.inner.lock().map_err(lock_error)?;
        if inner
            .task
            .as_ref()
            .is_some_and(|task| task.status == "running")
        {
            return Err("已有导出任务正在运行".into());
        }
    }
    let inner = state.inner.clone();
    let cancel = state.cancel_requested.clone();
    std::thread::spawn(move || {
        if let Err(error) = scheduler::run_now(&inner, &cancel, &id) {
            // 预检后到执行前的竞态（如手动导出抢占）：显式写入错误任务状态供界面反馈
            if let Ok(mut guard) = inner.lock() {
                guard.task = Some(TaskState {
                    kind: "schedule".into(),
                    status: "error".into(),
                    progress_text: "定时任务运行失败".into(),
                    log: Vec::new(),
                    log_start: 0,
                    output_path: None,
                    error: Some(error),
                });
            }
        }
    });
    Ok(())
}

/// 终止正在运行的定时任务：设置 cancel 标志 + 更新运行记录为 cancelled。
#[tauri::command]
fn cancel_schedule_run(state: State<'_, AppState>, id: String) -> Result<(), String> {
    {
        let _guard = scheduler::store_guard();
        let store = schedule::load_store()?;
        if store.find(&id).is_none() {
            return Err("定时任务不存在".into());
        }
    }
    scheduler::cancel_schedule_run(&state.inner, &state.cancel_requested, &id)
        .map(|_| ())
        .ok_or_else(|| "当前没有正在运行的该定时任务".to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app_state = AppState::default();
    // 启动后台调度线程（定时导出），与界面手动导出共享任务槽互斥
    scheduler::start_scheduler(app_state.inner.clone(), app_state.cancel_requested.clone());

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            get_app_version,
            check_env,
            check_dws_installed,
            open_url,
            login_dws,
            search_groups,
            get_default_output_dir,
            choose_output_dir,
            export_diagnostic_log,
            export_groups,
            cancel_export,
            snapshot,
            open_output,
            get_settings,
            save_settings,
            list_export_logs,
            get_diagnostics_info,
            list_schedules,
            get_schedule_runs,
            preview_schedule,
            validate_schedule,
            save_schedule,
            delete_schedule,
            toggle_schedule,
            run_schedule_now,
            cancel_schedule_run,
        ])
        .run(tauri::generate_context!())
        .expect("error while building tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_version_comes_from_cargo_metadata() {
        assert_eq!(get_app_version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn executable_uses_its_containing_directory() {
        let executable = PathBuf::from("install").join("dingtalk-chat-exporter");
        assert_eq!(
            executable_output_dir(&executable),
            Some(PathBuf::from("install"))
        );
    }

    #[test]
    fn app_bundle_uses_the_directory_containing_the_bundle() {
        let executable = PathBuf::from("install")
            .join("钉钉群聊导出器.app")
            .join("Contents")
            .join("MacOS")
            .join("dingtalk-chat-exporter");
        assert_eq!(
            executable_output_dir(&executable),
            Some(PathBuf::from("install"))
        );
    }

    #[test]
    fn app_translocation_uses_downloads_instead_of_temporary_mount() {
        let executable = PathBuf::from(
            "/private/var/folders/v9/random/T/AppTranslocation/UUID/d/钉钉群聊导出器.app/Contents/MacOS/dingtalk-chat-exporter",
        );
        let home = PathBuf::from("/Users/tester");
        assert_eq!(
            default_output_dir(
                Some(&executable),
                Some(Path::new("/")),
                Some(home.as_path())
            ),
            home.join("Downloads")
        );
    }

    #[test]
    fn similarly_named_regular_directory_is_not_treated_as_translocation() {
        let executable = PathBuf::from(
            "/Users/tester/AppTranslocation/钉钉群聊导出器.app/Contents/MacOS/dingtalk-chat-exporter",
        );
        assert_eq!(
            default_output_dir(
                Some(&executable),
                Some(Path::new("/")),
                Some(Path::new("/Users/tester"))
            ),
            PathBuf::from("/Users/tester/AppTranslocation")
        );
    }

    #[test]
    fn media_ids_stop_at_metadata_delimiters_and_deduplicate() {
        let content = "[图片消息](mediaId=abc==, mediaId=def+/==) mediaId=abc==";
        assert_eq!(media::extract_media_ids(content), vec!["abc==", "def+/=="]);
    }

    #[test]
    fn log_offset_tracks_evicted_lines() {
        let state = Arc::new(Mutex::new(AppInner {
            auth: None,
            task: Some(TaskState {
                kind: "export".into(),
                status: "running".into(),
                progress_text: String::new(),
                log: Vec::new(),
                log_start: 0,
                output_path: None,
                error: None,
            }),
        }));
        for index in 0..700 {
            append_log(&state, &format!("line-{index}"));
        }
        let inner = state.lock().unwrap();
        let task = inner.task.as_ref().unwrap();
        assert_eq!(task.log.len(), MAX_LOG_LINES);
        assert_eq!(task.log_start, 200);
        assert_eq!(task.log.first().map(String::as_str), Some("line-200"));
    }

    #[test]
    fn diagnostic_report_redacts_credentials_and_home_paths() {
        let snapshot = AppSnapshot {
            authed: true,
            auth_info: Some(dws::AuthInfo {
                corp_name: "测试组织".into(),
                user_name: "测试用户".into(),
                corp_id: "corp-id-must-not-be-exported".into(),
            }),
            task: Some(TaskState {
                kind: "export".into(),
                status: "error".into(),
                progress_text: "群「测试群」拉取失败".into(),
                log: vec![
                    "trace_id=trace-123".into(),
                    "Authorization: Bearer secret-token".into(),
                ],
                log_start: 3,
                output_path: Some("/Users/tester/Desktop/export".into()),
                error: Some("AUTH_PERMISSION_DENIED".into()),
            }),
        };
        let report = build_diagnostic_report(
            &snapshot,
            Some("/Users/tester/.local/bin/dws"),
            "v1.0.54 (8f62c19)",
            &["/Users/tester".into()],
            "2026-07-25 16:00:00",
        );

        assert!(report.contains(&format!("appVersion: {}", env!("CARGO_PKG_VERSION"))));
        assert!(report.contains("dwsPath: <HOME>/.local/bin/dws"));
        assert!(report.contains("outputPath: <HOME>/Desktop/export"));
        assert!(report.contains("trace_id=trace-123"));
        assert!(report.contains("[已脱敏：该日志行包含凭据字段]"));
        assert!(!report.contains("secret-token"));
        assert!(!report.contains("corp-id-must-not-be-exported"));
        assert!(!report.contains("/Users/tester"));
    }

    #[test]
    fn diagnostic_redaction_covers_common_credential_spellings() {
        for text in [
            "accessToken=secret",
            "refreshToken=secret",
            "clientSecret=secret",
            "token=secret",
            "api_key=secret",
            "Cookie: sid=secret",
            "X-Amz-Signature=secret",
        ] {
            assert_eq!(
                sanitize_diagnostic_text(text, &[]),
                "[已脱敏：该日志行包含凭据字段]",
                "未脱敏: {text}"
            );
        }
    }

    #[test]
    fn snapshot_returns_only_requested_log_delta() {
        let inner = AppInner {
            auth: None,
            task: Some(TaskState {
                kind: "export".into(),
                status: "running".into(),
                progress_text: String::new(),
                log: vec!["100".into(), "101".into(), "102".into()],
                log_start: 100,
                output_path: None,
                error: None,
            }),
        };
        let snapshot = build_snapshot(&inner, Some(102));
        let task = snapshot.task.unwrap();
        assert_eq!(task.log_start, 102);
        assert_eq!(task.log, vec!["102"]);
    }
}
