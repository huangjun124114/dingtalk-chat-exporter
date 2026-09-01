// 钉钉群聊导出器 - Tauri 后端

mod date;
mod dws;
mod export_log;
mod media;
mod settings;
mod viewer;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
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
    inner: Arc<Mutex<AppInner>>,
    cancel_requested: Arc<AtomicBool>,
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupExportRequest {
    pub title: String,
    pub open_conversation_id: String,
    #[serde(default)]
    pub create_at: Option<String>,
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
    let root = PathBuf::from(&output_root);
    let log = |message: &str| append_log(&state, message);
    let set_progress = |text: String| set_task_progress(&state, text);
    let mut errors = Vec::new();
    let mut published_groups = 0usize;

    if let Err(error) = fs::create_dir_all(&root) {
        finish_task(
            &state,
            "error",
            format!("创建输出目录失败: {}", error),
            Some(format!("创建输出目录 {} 失败: {}", root.display(), error)),
            None,
        );
        return;
    }

    for (group_index, group) in groups.iter().enumerate() {
        if cancel_requested.load(Ordering::Relaxed) {
            break;
        }
        log(&format!(
            "\n===== [{}/{}] 导出群: {} =====",
            group_index + 1,
            groups.len(),
            group.title
        ));
        set_progress(format!(
            "[{}/{}] 正在导出: {}",
            group_index + 1,
            groups.len(),
            group.title
        ));

        // 生成目录名称：{群名}_{MMDD}_{HHMMSS}
        let now = dws::current_time_str();
        let (month_day, hour_min_sec) = if now.len() >= 19 {
            // 格式: "2026-01-15 10:30:00"
            let md = now[5..10].replace("-", ""); // "0115"
            let hms = now[11..19].replace(":", ""); // "103000"
            (md, hms)
        } else {
            ("0000".to_string(), "000000".to_string())
        };
        let group_directory_name = format!(
            "{}_{}_{}",
            sanitize_filename(&group.title),
            month_day,
            hour_min_sec
        );
        let group_dir = root.join(&group_directory_name);
        let attachment_dir = group_dir.join("attachments");
        
        if let Err(error) = fs::create_dir_all(&attachment_dir) {
            record_error(
                &log,
                &mut errors,
                format!("群「{}」创建目录失败: {}", group.title, error),
            );
            continue;
        }
        let group_error_start = errors.len();

        let progress_state = state.clone();
        let diagnostic_state = state.clone();
        
        // 记录本次导出的时间范围信息
        if let Some(ref st) = start_time {
            log(&format!("群「{}」开始时间: {}", group.title, st));
        } else {
            log(&format!("群「{}」未设置开始时间，从最早消息开始", group.title));
        }
        if let Some(ref et) = end_time {
            log(&format!("群「{}」结束时间: {}", group.title, et));
        } else {
            log(&format!("群「{}」未设置结束时间，到最新消息结束", group.title));
        }

        let messages = match dws::fetch_all_messages(
            &group.open_conversation_id,
            start_time.as_deref(),
            end_time.as_deref(),
            &|count, earliest| {
                set_task_progress(
                    &progress_state,
                    format!("拉取消息: {} 条（至 {}）", count, earliest),
                );
            },
            &|message| append_log(&diagnostic_state, message),
            &cancel_requested,
        ) {
            Ok(messages) => messages,
            Err(error) if error == dws::CANCELLED_ERROR => break,
            Err(error) => {
                record_error(
                    &log,
                    &mut errors,
                    format!("群「{}」拉取消息失败: {}", group.title, error),
                );
                continue;
            }
        };
        log(&format!("共拉取 {} 条消息（含话题回复）", messages.len()));
        
        // 记录实际的消息时间范围
        let actual_earliest = messages.first().map(|m| m.create_time.clone());
        let actual_latest = messages.last().map(|m| m.create_time.clone());
        if let Some(ref earliest) = actual_earliest {
            log(&format!("实际最早消息: {}", earliest));
        }
        if let Some(ref latest) = actual_latest {
            log(&format!("实际最新消息: {}", latest));
        }

        if let Err(error) = write_json(&group_dir.join("messages.json"), &messages) {
            record_error(
                &log,
                &mut errors,
                format!("群「{}」写 messages.json 失败: {}", group.title, error),
            );
            continue;
        }

        let media_count: usize = messages
            .iter()
            .map(|message| media::extract_media_ids(&message.content).len())
            .sum();
        let mut media_index = 0usize;
        let mut attachments = Vec::with_capacity(media_count);
        if media_count > 0 {
            log(&format!("开始下载 {} 个附件", media_count));
        }

        for message in &messages {
            for (file_index, media_id) in media::extract_media_ids(&message.content)
                .into_iter()
                .enumerate()
            {
                if cancel_requested.load(Ordering::Relaxed) {
                    break;
                }
                media_index += 1;
                let extension = media_extension(&message.content);
                let file_name = format!(
                    "{}_{}_{}_{}.{}",
                    timestamp_fragment(&message.create_time),
                    safe_id_fragment(&message.open_message_id, 16),
                    file_index + 1,
                    safe_id_fragment(&media_id, 10),
                    extension
                );
                
                // 按年月分目录存储附件
                let year_month = extract_year_month(&message.create_time);
                let attachment_subdir = attachment_dir.join(&year_month);
                if let Err(e) = fs::create_dir_all(&attachment_subdir) {
                    let detail = format!("创建附件目录失败 {}: {}", attachment_subdir.display(), e);
                    log(&detail);
                    errors.push(detail);
                    continue;
                }
                let output_path = attachment_subdir.join(&file_name);
                
                set_progress(format!(
                    "[{}/{}] 下载附件: {}",
                    media_index, media_count, file_name
                ));

                let result = dws::download_media(
                    &group.open_conversation_id,
                    &message.open_message_id,
                    &media_id,
                    &output_path,
                    &cancel_requested,
                );
                let (status, error_text) = match result {
                    Ok(()) => ("ok", None),
                    Err(error) if error == dws::CANCELLED_ERROR => ("cancelled", Some(error)),
                    Err(error) => {
                        let detail = format!(
                            "群「{}」附件 #{} 下载失败: {}",
                            group.title, media_index, error
                        );
                        log(&detail);
                        errors.push(detail);
                        ("fail", Some(error))
                    }
                };
                // 记录相对路径（包含年月子目录）
                let relative_file_path = format!("{}/{}", year_month, file_name);
                attachments.push(serde_json::json!({
                    "openMessageId": message.open_message_id,
                    "createTime": message.create_time,
                    "sender": message.sender,
                    "mediaId": media_id,
                    "file": relative_file_path,
                    "originalFileName": media::extract_original_file_name(&message.content)
                        .map(|name| sanitize_filename(&name)),
                    "status": status,
                    "error": error_text,
                }));
                if status == "cancelled" {
                    break;
                }
            }
            if cancel_requested.load(Ordering::Relaxed) {
                break;
            }
        }

        if cancel_requested.load(Ordering::Relaxed) {
            if let Err(error) =
                write_json(&group_dir.join("attachments_index.json"), &attachments)
            {
                log(&format!("保存断点附件索引失败: {error}"));
            }
            break;
        }

        if let Err(error) = write_json(&group_dir.join("attachments_index.json"), &attachments) {
            record_error(
                &log,
                &mut errors,
                format!(
                    "群「{}」写 attachments_index.json 失败: {}",
                    group.title, error
                ),
            );
            continue;
        }
        let successful_attachments = attachments
            .iter()
            .filter(|attachment| attachment["status"] == "ok")
            .count();
        log(&format!(
            "附件下载完成: 成功 {}/{}",
            successful_attachments, media_count
        ));

        // 按月份分组消息
        set_progress("按月份分组消息...".to_string());
        let mut messages_by_month: BTreeMap<String, Vec<&dws::Message>> = BTreeMap::new();
        for message in &messages {
            let year_month = extract_year_month(&message.create_time);
            messages_by_month.entry(year_month).or_default().push(message);
        }
        
        log(&format!("消息分布在 {} 个月份", messages_by_month.len()));
        
        // 为每个月生成独立的 HTML 文件
        let mut html_files_info: Vec<export_log::HtmlFileInfo> = Vec::new();
        let mut html_file_count = 0;
        
        for (year_month, month_messages) in &messages_by_month {
            if cancel_requested.load(Ordering::Relaxed) {
                break;
            }
            
            html_file_count += 1;
            set_progress(format!("生成 {} 年 {} 月聊天记录...", 
                &year_month[0..4], &year_month[4..6]));
            
            // 解析为 Message 向量
            let messages_vec: Vec<dws::Message> = month_messages.iter().map(|m| (*m).clone()).collect();
            
            // 生成文件名（处理重名）
            let html_file_name = resolve_html_filename(&group.title, year_month, &group_dir);
            let html_path = group_dir.join(&html_file_name);
            
            // 统计该月的附件数
            let month_attachment_count: usize = month_messages.iter()
                .map(|m| media::extract_media_ids(&m.content).len())
                .sum();
            
            match viewer::generate_html(
                &messages_vec,
                &group.title,
                &attachment_dir,
                &self_name,
                &html_path,
                &cancel_requested,
            ) {
                Ok(()) => {
                    let file_size = html_path.metadata()
                        .map(|m| m.len())
                        .unwrap_or(0);
                    log(&format!(
                        "已生成: {}（{} 条消息, {} 个附件, {:.1} MB）",
                        html_file_name,
                        month_messages.len(),
                        month_attachment_count,
                        file_size as f64 / 1_048_576.0
                    ));
                    html_files_info.push(export_log::HtmlFileInfo {
                        filename: html_file_name.clone(),
                        year_month: year_month.clone(),
                        message_count: month_messages.len(),
                        attachment_count: month_attachment_count,
                        file_size_bytes: file_size,
                    });
                },
                Err(error) if error == dws::CANCELLED_ERROR => break,
                Err(error) => {
                    record_error(
                        &log,
                        &mut errors,
                        format!("群「{}」生成 {} 年 {} 月 HTML 失败: {}", 
                            group.title, &year_month[0..4], &year_month[4..6], error),
                    );
                }
            }
        }
        
        log(&format!("共生成 {} 个月度 HTML 文件", html_file_count));

        if cancel_requested.load(Ordering::Relaxed) {
            break;
        }

        published_groups += 1;
        log(&format!(
            "群「{}」已完整发布到 {}",
            group.title,
            group_dir.display()
        ));
        
        // 写入导出日志
        let success_attachments: usize = attachments.iter()
            .filter(|a| a["status"] == "ok")
            .count();
        let failed_attachments: usize = attachments.iter()
            .filter(|a| a["status"] == "fail")
            .count();
        
        // 获取当前时间字符串（复用 dws 模块的函数）
        let export_time = dws::current_time_str();
        
        // 收集日志行
        let log_lines: Vec<String> = state.lock()
            .ok()
            .and_then(|inner| inner.task.as_ref().map(|t| t.log.clone()))
            .unwrap_or_default();
        
        let log_entry = export_log::ExportLogEntry {
            id: export_log::generate_timestamp_id(),
            group_name: group.title.clone(),
            group_id: group.open_conversation_id.clone(),
            directory_name: group_directory_name.clone(),
            export_time: export_time.clone(),
            start_time: start_time.clone(),
            end_time: end_time.clone(),
            actual_earliest: actual_earliest.clone(),
            actual_latest: actual_latest.clone(),
            message_count: messages.len(),
            attachment_total: success_attachments + failed_attachments,
            attachment_success: success_attachments,
            attachment_failed: failed_attachments,
            html_files: html_files_info.clone(),
            status: if errors.len() == group_error_start { "success".to_string() } else { "partial".to_string() },
            error_message: if errors.len() > group_error_start {
                Some(errors[group_error_start..].join("; "))
            } else {
                None
            },
            log_lines: log_lines.clone(),
        };
        
        if let Err(e) = export_log::append_export_log(&group_dir, &log_entry) {
            log(&format!("警告: 写入导出日志失败: {}", e));
        }
    }

    if cancel_requested.load(Ordering::Relaxed) {
        log("导出已由用户取消；已导出内容保留在对应目录中");
        finish_task(
            &state,
            "cancelled",
            "导出已取消".into(),
            None,
            Some(output_root),
        );
    } else if errors.is_empty() {
        finish_task(
            &state,
            "done",
            format!("导出完成，共 {} 个群", published_groups),
            None,
            Some(output_root),
        );
    } else {
        finish_task(
            &state,
            "error",
            format!("导出结束，有 {} 个错误", errors.len()),
            Some(errors.join("\n")),
            Some(output_root),
        );
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

fn record_error(log: &dyn Fn(&str), errors: &mut Vec<String>, error: String) {
    log(&error);
    errors.push(error);
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    // 原子写入：先写入临时文件，成功后再重命名到目标路径
    let temp_path = path.with_extension("tmp");
    let file = fs::File::create(&temp_path)
        .map_err(|error| format!("创建 {} 失败: {}", temp_path.display(), error))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)
        .map_err(|error| format!("序列化 {} 失败: {}", temp_path.display(), error))?;
    writer
        .flush()
        .map_err(|error| format!("刷新 {} 失败: {}", temp_path.display(), error))?;
    drop(writer); // 确保文件句柄关闭
    fs::rename(&temp_path, path)
        .map_err(|error| format!("重命名 {} 失败: {}", temp_path.display(), error))
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

fn media_extension(content: &str) -> String {
    if let Some(file_name) = media::extract_original_file_name(content) {
        if let Some(extension) = Path::new(&file_name)
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| {
                !extension.is_empty()
                    && extension.len() <= 12
                    && extension
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric())
            })
        {
            return extension.to_ascii_lowercase();
        }
    }
    if content.contains("[视频消息]") {
        "mp4".into()
    } else if content.contains("[图片消息]") {
        "jpg".into()
    } else if content.contains("[语音消息]") || content.contains("[音频消息]") {
        "m4a".into()
    } else {
        "bin".into()
    }
}

fn timestamp_fragment(timestamp: &str) -> String {
    let fragment: String = timestamp
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    fragment.trim_matches('_').to_string()
}

fn sanitize_filename(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
        })
        .take(80)
        .collect::<String>()
        .trim()
        .trim_end_matches(['.', ' '])
        .to_string();
    if sanitized.is_empty() {
        sanitized = "群聊".into();
    }
    let stem = sanitized
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "COM¹"
            | "COM²"
            | "COM³"
            | "LPT¹"
            | "LPT²"
            | "LPT³"
    );
    if reserved {
        sanitized.insert(0, '_');
    }
    sanitized
}

/// 从日期时间字符串提取年月（YYYYMM 格式）
/// 输入格式: "2025-01-15 10:30:00" → 输出: "202501"
fn extract_year_month(datetime: &str) -> String {
    crate::date::extract_year_month(datetime).unwrap_or_else(|| "unknown".to_string())
}

/// 生成 HTML 文件名，处理重名情况
/// 规则：群名-YYYYMM.html，重名时添加序号 -01, -02...
fn resolve_html_filename(group_title: &str, year_month: &str, group_dir: &Path) -> String {
    let safe_title = sanitize_filename(group_title);
    let base_filename = format!("{}-{}.html", safe_title, year_month);
    
    // 如果文件不存在，直接返回基础文件名
    if !group_dir.join(&base_filename).exists() {
        return base_filename;
    }
    
    // 文件已存在，尝试添加序号
    for seq in 1..=999 {
        let candidate = format!("{}-{}-{:02}.html", safe_title, year_month, seq);
        if !group_dir.join(&candidate).exists() {
            return candidate;
        }
    }
    
    // 极端情况：生成带时间戳的文件名
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{}-{}-{}.html", safe_title, year_month, timestamp)
}

fn safe_id_fragment(id: &str, max_length: usize) -> String {
    let fragment: String = id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(max_length)
        .collect();
    if fragment.is_empty() {
        "unknown".into()
    } else {
        fragment
    }
}

pub fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
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
    fn unsafe_ids_cannot_create_paths() {
        assert_eq!(safe_id_fragment("../../a/b+c=", 20), "abc");
        assert_eq!(safe_id_fragment("中文", 20), "unknown");
        assert_ne!(
            stable_hash("cid/same-prefix/a"),
            stable_hash("cid/same-prefix/b")
        );
    }

    #[test]
    fn sanitizes_windows_reserved_names() {
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("COM¹.txt"), "_COM¹.txt");
        assert_eq!(sanitize_filename("LPT³"), "_LPT³");
        assert_eq!(sanitize_filename("../测试:*?"), "..测试");
    }

    #[test]
    fn resolve_html_filename_uses_sanitized_group_title() {
        let temp_dir = std::path::PathBuf::from("temp");
        assert_eq!(resolve_html_filename("研发/值班:日报*?", "202601", &temp_dir), "研发值班日报-202601.html");
        assert_eq!(resolve_html_filename("CON", "202601", &temp_dir), "_CON-202601.html");
        assert_eq!(resolve_html_filename("... ", "202601", &temp_dir), "群聊-202601.html");
    }

    #[test]
    fn preserves_declared_file_extension() {
        assert_eq!(
            media_extension("[文件消息](fileName=季度报告.PDF, mediaId=x)"),
            "pdf"
        );
        assert_eq!(media_extension("[语音消息](mediaId=x)"), "m4a");
        assert_eq!(
            media_extension("[文件消息](fileName=报告(终版).PDF, mediaId=x)"),
            "pdf"
        );
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
