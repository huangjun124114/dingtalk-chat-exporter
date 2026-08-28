// dws 命令调用封装
// 通过 std::process::Command 调用系统上的 dws 二进制。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const JSON_COMMAND_TIMEOUT: Duration = Duration::from_secs(45);
const DOWNLOAD_COMMAND_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const FIND_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_PAGE_LIMIT: usize = 500;
const MAX_PAGE_LIMIT: usize = 10_000;
const MAX_PAGES: usize = 100_000;
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const STREAM_FINISH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MESSAGES: usize = 1_000_000;
const MAX_MESSAGE_BYTES: usize = 512 * 1024 * 1024;
pub const CANCELLED_ERROR: &str = "任务已取消";
type RetryReporter<'a> = &'a dyn Fn(u32, u64, &str);

/// 创建一个不会弹出控制台/终端窗口的 Command。
fn silent_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

struct CapturedOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

struct ProcessTree {
    #[cfg(unix)]
    pid: u32,
    #[cfg(target_os = "windows")]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl ProcessTree {
    fn attach(child: &Child) -> Result<Self, String> {
        #[cfg(target_os = "windows")]
        {
            use std::mem::size_of;
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };

            // SAFETY: all handles and structure sizes follow the Win32 Job Object contract.
            // The handle is owned by ProcessTree and closed exactly once in Drop.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(format!(
                        "创建 Windows 子进程 Job Object 失败: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(info).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(format!("配置 Windows Job Object 失败: {error}"));
                }
                if AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0 {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(format!("加入 Windows Job Object 失败: {error}"));
                }
                return Ok(Self { job });
            }
        }

        #[cfg(not(target_os = "windows"))]
        Ok(Self { pid: child.id() })
    }

    fn terminate(&self, child: &mut Child) {
        #[cfg(unix)]
        {
            // SAFETY: silent_command puts the child in a process group whose id is the child pid.
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGKILL);
            }
        }
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            // SAFETY: self.job remains valid for the full ProcessTree lifetime.
            unsafe {
                TerminateJobObject(self.job, 1);
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(target_os = "windows")]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: ProcessTree exclusively owns this non-null handle.
        unsafe {
            CloseHandle(self.job);
        }
    }
}

fn run_command(
    program: &str,
    args: &[String],
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<CapturedOutput, String> {
    if is_cancelled(cancel) {
        return Err(CANCELLED_ERROR.into());
    }

    let mut child = silent_command(program)
        .args(args)
        .spawn()
        .map_err(|e| format!("启动 {} 失败: {}", program, e))?;
    let process_tree = ProcessTree::attach(&child).inspect_err(|_| {
        let _ = child.kill();
        let _ = child.wait();
    })?;
    let stdout = child.stdout.take().ok_or("无法捕获 dws stdout")?;
    let stderr = child.stderr.take().ok_or("无法捕获 dws stderr")?;
    let stdout_reader = spawn_stream_reader(stdout);
    let stderr_reader = spawn_stream_reader(stderr);
    let started = Instant::now();

    let status = loop {
        if is_cancelled(cancel) {
            process_tree.terminate(&mut child);
            return Err(CANCELLED_ERROR.into());
        }
        if started.elapsed() >= timeout {
            process_tree.terminate(&mut child);
            return Err(format!(
                "执行 dws 超时（{} 秒），子进程树已终止",
                timeout.as_secs()
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                process_tree.terminate(&mut child);
                return Err(format!("等待 dws 子进程失败: {}", e));
            }
        }
    };

    let stdout = receive_stream(stdout_reader, "stdout").inspect_err(|_| {
        process_tree.terminate(&mut child);
    })?;
    let stderr = receive_stream(stderr_reader, "stderr").inspect_err(|_| {
        process_tree.terminate(&mut child);
    })?;
    Ok(CapturedOutput {
        status,
        stdout: captured_text(stdout),
        stderr: captured_text(stderr),
    })
}

fn spawn_stream_reader(
    stream: impl Read + Send + 'static,
) -> Receiver<std::io::Result<CapturedStream>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sender.send(read_stream(stream));
    });
    receiver
}

fn read_stream(mut stream: impl Read) -> std::io::Result<CapturedStream> {
    let mut data = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let overflow = data
            .len()
            .saturating_add(read)
            .saturating_sub(MAX_CAPTURE_BYTES);
        if overflow > 0 {
            data.copy_within(overflow.., 0);
            data.truncate(data.len() - overflow);
            truncated = true;
        }
        data.extend_from_slice(&buffer[..read]);
    }
    Ok(CapturedStream {
        bytes: data,
        truncated,
    })
}

fn receive_stream(
    receiver: Receiver<std::io::Result<CapturedStream>>,
    name: &str,
) -> Result<CapturedStream, String> {
    receiver
        .recv_timeout(STREAM_FINISH_TIMEOUT)
        .map_err(|error| format!("等待 dws {name} 结束失败: {error}"))?
        .map_err(|error| format!("读取 dws {name} 失败: {error}"))
}

fn captured_text(captured: CapturedStream) -> String {
    let text = String::from_utf8_lossy(&captured.bytes);
    if captured.truncated {
        format!("[较早输出已截断]\n{text}")
    } else {
        text.into_owned()
    }
}

fn is_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

fn executable_candidate(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// dws 可执行文件路径。
///
/// 不缓存失败结果：用户安装或移动 dws 后，界面的“重新检测”可以立即生效。
pub fn find_dws() -> Option<String> {
    let names: &[&str] = if cfg!(windows) {
        &["dws.exe", "dws.cmd", "dws.bat", "dws"]
    } else {
        &["dws", "dws.exe"]
    };
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    let candidate_dirs: Vec<String> = if cfg!(target_os = "macos") {
        vec![
            format!("{}/.local/bin", home),
            "/usr/local/bin".into(),
            "/opt/homebrew/bin".into(),
        ]
    } else if cfg!(target_os = "windows") {
        let appdata =
            std::env::var("APPDATA").unwrap_or_else(|_| format!("{}\\AppData\\Roaming", home));
        vec![
            format!("{}\\npm", appdata),
            format!("{}\\.local\\bin", home),
        ]
    } else {
        vec![format!("{}/.local/bin", home), "/usr/local/bin".into()]
    };

    for dir in &candidate_dirs {
        for name in names {
            let full = Path::new(dir).join(name);
            if executable_candidate(&full) {
                return Some(full.to_string_lossy().into_owned());
            }
        }
    }

    let finder = if cfg!(windows) { "where" } else { "which" };
    for name in names {
        let args = vec![name.to_string()];
        if let Ok(out) = run_command(finder, &args, FIND_TIMEOUT, None) {
            if out.status.success() {
                if let Some(path) = out
                    .stdout
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                {
                    if executable_candidate(Path::new(path)) {
                        return Some(path.to_string());
                    }
                }
            }
        }
    }

    for name in names {
        let args = vec!["--version".to_string()];
        if let Ok(out) = run_command(name, &args, FIND_TIMEOUT, None) {
            if out.status.success() {
                return Some(name.to_string());
            }
        }
    }
    None
}

pub fn check_auth() -> Result<AuthInfo, String> {
    let dws = find_dws().ok_or_else(missing_dws_error)?;
    let args = strings(&["auth", "status", "--format", "json"]);
    let json = run_json_with_retry(&dws, &args, 1, None, false, None)?;
    if json.get("authenticated").and_then(Value::as_bool) != Some(true) {
        return Err("dws 未登录，请先运行 dws auth login".into());
    }
    Ok(AuthInfo {
        corp_name: json
            .get("corp_name")
            .or_else(|| json.get("corpName"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        user_name: json
            .get("user_name")
            .or_else(|| json.get("userName"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        corp_id: json
            .get("corp_id")
            .or_else(|| json.get("corpId"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

pub fn diagnostic_version() -> Result<String, String> {
    let dws = find_dws().ok_or_else(missing_dws_error)?;
    let args = strings(&["version", "--format", "json"]);
    let json = run_json_with_retry(&dws, &args, 1, None, false, None)?;
    let version = json
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let commit = json
        .get("commit")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    Ok(format!("{} ({})", version, commit))
}

fn missing_dws_error() -> String {
    "未找到 dws 命令。请确认已安装 dws。\n安装方法: npm install -g dingtalk-workspace-cli"
        .to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthInfo {
    pub corp_name: String,
    pub user_name: String,
    pub corp_id: String,
}

/// 搜索全部匹配群聊，直到 dws 返回 hasMore=false。
pub fn search_groups(query: &str) -> Result<Vec<GroupInfo>, String> {
    let dws = find_dws().ok_or_else(missing_dws_error)?;
    let mut cursor = "0".to_string();
    let mut seen_cursors = HashSet::new();
    let mut seen_groups = HashSet::new();
    let mut all_groups = Vec::new();

    for page in 1..=MAX_PAGES {
        if !seen_cursors.insert(cursor.clone()) {
            return Err(format!("群搜索分页游标重复，已在第 {} 页停止", page));
        }
        let args = vec![
            "chat".into(),
            "search".into(),
            "--query".into(),
            query.into(),
            "--limit".into(),
            "100".into(),
            "--cursor".into(),
            cursor.clone(),
            "--format".into(),
            "json".into(),
        ];
        let json = run_json_with_retry(&dws, &args, 3, None, false, None)?;
        let page_data = parse_group_page(&json)?;
        for group in page_data.groups {
            if seen_groups.insert(group.open_conversation_id.clone()) {
                all_groups.push(group);
            }
        }
        if !page_data.has_more {
            return Ok(all_groups);
        }
        cursor = page_data
            .next_cursor
            .filter(|next| !next.is_empty())
            .ok_or_else(|| format!("群搜索第 {} 页声明 hasMore=true，但未返回 nextCursor", page))?;
    }
    Err(format!("群搜索超过最大分页数 {}", MAX_PAGES))
}

struct GroupPage {
    groups: Vec<GroupInfo>,
    has_more: bool,
    next_cursor: Option<String>,
}

fn parse_group_page(json: &Value) -> Result<GroupPage, String> {
    let payload = json
        .get("result")
        .or_else(|| json.get("data"))
        .unwrap_or(json);
    let list = if let Some(list) = payload.as_array() {
        list
    } else {
        ["groups", "list", "items"]
            .iter()
            .find_map(|key| payload.get(key).and_then(Value::as_array))
            .ok_or("解析群搜索结果失败：未找到 groups 数组")?
    };
    let groups = list
        .iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<Result<Vec<GroupInfo>, _>>()
        .map_err(|e| format!("解析群信息失败: {}", e))?;
    Ok(GroupPage {
        groups,
        has_more: required_bool_field(payload, json, &["hasMore", "has_more"])
            .map_err(|error| format!("解析群搜索分页失败: {error}"))?,
        next_cursor: string_field(payload, json, "nextCursor"),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupInfo {
    pub title: String,
    pub open_conversation_id: String,
    #[serde(default)]
    pub member_count: u64,
    #[serde(default)]
    pub create_at: String,
}

/// 拉取群的全部普通消息和话题回复。
pub fn fetch_all_messages(
    group_id: &str,
    cutoff_time: Option<&str>,
    on_progress: &dyn Fn(usize, &str),
    on_diagnostic: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<Vec<Message>, String> {
    let dws = find_dws().ok_or("未找到 dws 命令")?;
    let mut all_messages = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut message_bytes = 0usize;
    fetch_message_pages(
        &dws,
        group_id,
        None,
        &mut all_messages,
        &mut seen_ids,
        &mut message_bytes,
        cutoff_time,
        on_progress,
        on_diagnostic,
        cancel,
    )?;

    let topic_ids: Vec<String> = all_messages
        .iter()
        .filter_map(|message| message.open_conv_thread_id.clone())
        .filter(|topic_id| !topic_id.trim().is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for topic_id in topic_ids {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED_ERROR.into());
        }
        fetch_message_pages(
            &dws,
            group_id,
            Some(&topic_id),
            &mut all_messages,
            &mut seen_ids,
            &mut message_bytes,
            cutoff_time,
            on_progress,
            on_diagnostic,
            cancel,
        )?;
    }

    all_messages.sort_by(|a, b| {
        a.create_time
            .cmp(&b.create_time)
            .then_with(|| a.open_message_id.cmp(&b.open_message_id))
    });
    Ok(all_messages)
}

fn is_within_cutoff(create_time: &str, cutoff_time: Option<&str>) -> bool {
    cutoff_time.is_none_or(|cutoff| create_time >= cutoff)
}

fn crossed_cutoff(earliest: &str, cutoff_time: Option<&str>) -> bool {
    cutoff_time.is_some_and(|cutoff| earliest < cutoff)
}

#[allow(clippy::too_many_arguments)]
fn fetch_message_pages(
    dws: &str,
    group_id: &str,
    topic_id: Option<&str>,
    all_messages: &mut Vec<Message>,
    seen_ids: &mut HashSet<String>,
    message_bytes: &mut usize,
    cutoff_time: Option<&str>,
    on_progress: &dyn Fn(usize, &str),
    on_diagnostic: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut current_time = current_time_str();
    let mut page_limit = INITIAL_PAGE_LIMIT;
    for page in 1..=MAX_PAGES {
        let mut args = vec![
            "chat".into(),
            "message".into(),
            if topic_id.is_some() {
                "list-topic-replies".into()
            } else {
                "list".into()
            },
            "--group".into(),
            group_id.into(),
        ];
        if let Some(topic_id) = topic_id {
            args.extend(["--topic-id".into(), topic_id.into()]);
        }
        args.extend([
            "--time".into(),
            current_time.clone(),
            "--direction".into(),
            "older".into(),
        ]);
        // 接口没有 cursor；从较大单页开始，遇到同秒边界时再自适应扩大。
        args.extend(["--limit".into(), page_limit.to_string()]);
        args.extend(["--format".into(), "json".into()]);

        // 长分页期间，该接口偶发先返回 AUTH_PERMISSION_DENIED、随后同一请求成功。
        // 仅对此消息接口启用有限重试；其他权限错误仍立即返回。
        let message_kind = if topic_id.is_some() {
            "话题回复"
        } else {
            "主消息"
        };
        let retry_reporter = |attempt: u32, delay_seconds: u64, error: &str| {
            on_diagnostic(&format!(
                "{}第 {} 页暂时失败，第 {}/4 次请求后等待 {} 秒重试: {}",
                message_kind,
                page,
                attempt,
                delay_seconds,
                preview(error, 240)
            ));
        };
        let json = run_json_with_retry(dws, &args, 4, Some(cancel), true, Some(&retry_reporter))?;
        let page_data = parse_message_page(&json)
            .map_err(|e| format!("解析消息输出失败（第 {} 页）: {}", page, e))?;
        if page_data.messages.is_empty() {
            if page_data.has_more {
                return Err(format!(
                    "消息第 {} 页为空但 hasMore=true，拒绝静默截断",
                    page
                ));
            }
            return Ok(());
        }

        let mut earliest = current_time.clone();
        let mut new_count = 0;
        for message in page_data.messages {
            if message.create_time < earliest {
                earliest = message.create_time.clone();
            }
            let within_cutoff = is_within_cutoff(&message.create_time, cutoff_time);
            if within_cutoff && seen_ids.insert(message.open_message_id.clone()) {
                *message_bytes = message_bytes.saturating_add(message.approximate_bytes());
                if all_messages.len() >= MAX_MESSAGES || *message_bytes > MAX_MESSAGE_BYTES {
                    return Err(format!(
                        "消息量超过安全上限（最多 {} 条或约 {} MB），已停止以避免内存耗尽",
                        MAX_MESSAGES,
                        MAX_MESSAGE_BYTES / 1024 / 1024
                    ));
                }
                all_messages.push(message);
                new_count += 1;
            }
        }
        on_progress(all_messages.len(), &earliest);
        on_diagnostic(&format!(
            "{}第 {} 页完成: 新增 {} 条，累计 {} 条，最早时间 {}，hasMore={}",
            message_kind,
            page,
            new_count,
            all_messages.len(),
            earliest,
            page_data.has_more
        ));
        if crossed_cutoff(&earliest, cutoff_time) {
            let cutoff = cutoff_time.unwrap_or_default();
            on_diagnostic(&format!(
                "{}已到达截止时间 {}，停止拉取更早消息",
                message_kind, cutoff
            ));
            return Ok(());
        }
        if !page_data.has_more {
            return Ok(());
        }
        if earliest == current_time {
            if page_limit < MAX_PAGE_LIMIT {
                page_limit = page_limit.saturating_mul(2).min(MAX_PAGE_LIMIT);
                on_diagnostic(&format!(
                    "{}第 {} 页停留在同一秒 {}，将单页扩大到 {} 条后重试边界",
                    message_kind, page, current_time, page_limit
                ));
                continue;
            }
            return Err(format!(
                "消息在同一秒 {} 内超过 {} 条，接口没有 cursor，无法保证完整性；已明确失败以避免静默漏消息",
                current_time, MAX_PAGE_LIMIT
            ));
        }
        if new_count == 0 || earliest > current_time {
            return Err(format!(
                "消息分页未向更早时间推进（第 {} 页，边界 {}）。该接口没有 cursor，可能在同一秒内超过单页容量；已明确失败以避免静默漏消息",
                page, earliest,
            ));
        }
        current_time = earliest;
        page_limit = INITIAL_PAGE_LIMIT;
        wait_with_cancel(Duration::from_millis(400), Some(cancel))?;
    }
    Err(format!("消息拉取超过最大分页数 {}", MAX_PAGES))
}

struct MessagePage {
    messages: Vec<Message>,
    has_more: bool,
}

fn parse_message_page(json: &Value) -> Result<MessagePage, String> {
    let payload = json
        .get("result")
        .or_else(|| json.get("data"))
        .unwrap_or(json);
    let list = if let Some(list) = payload.as_array() {
        list
    } else {
        ["messages", "list", "items", "records"]
            .iter()
            .find_map(|key| payload.get(key).and_then(Value::as_array))
            .ok_or("未找到 messages 数组")?
    };
    let messages = list
        .iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<Result<Vec<Message>, _>>()
        .map_err(|e| format!("解析消息列表失败: {}", e))?;
    for message in &messages {
        if message.open_message_id.trim().is_empty() {
            return Err("消息缺少 openMessageId，无法可靠去重".into());
        }
        if crate::date::parse_dws_datetime(&message.create_time).is_none() {
            return Err(format!(
                "消息 {} 的 createTime 格式无效: {:?}，预期 yyyy-MM-dd HH:mm:ss",
                message.open_message_id, message.create_time
            ));
        }
    }
    Ok(MessagePage {
        messages,
        has_more: required_bool_field(payload, json, &["hasMore", "has_more"])
            .map_err(|error| format!("解析消息分页失败: {error}"))?,
    })
}

fn unknown_sender() -> String {
    "未知发送者".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    #[serde(default, alias = "text")]
    pub content: String,
    pub create_time: String,
    #[serde(alias = "messageId", alias = "msgId")]
    pub open_message_id: String,
    #[serde(default = "unknown_sender")]
    pub sender: String,
    #[serde(default)]
    pub sender_open_dingtalk_id: Option<String>,
    #[serde(default, alias = "topicId")]
    pub open_conv_thread_id: Option<String>,
}

impl Message {
    fn approximate_bytes(&self) -> usize {
        self.content
            .len()
            .saturating_add(self.create_time.len())
            .saturating_add(self.open_message_id.len())
            .saturating_add(self.sender.len())
            .saturating_add(self.sender_open_dingtalk_id.as_ref().map_or(0, String::len))
            .saturating_add(self.open_conv_thread_id.as_ref().map_or(0, String::len))
    }
}

pub fn download_media(
    group_id: &str,
    message_id: &str,
    media_id: &str,
    output_path: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let dws = find_dws().ok_or("未找到 dws 命令")?;
    let args = vec![
        "chat".into(),
        "message".into(),
        "download-media".into(),
        "--type".into(),
        "mediaId".into(),
        "--resource-id".into(),
        media_id.into(),
        "--message-id".into(),
        message_id.into(),
        "--open-conversation-id".into(),
        group_id.into(),
        "--output".into(),
        output_path.to_string_lossy().into_owned(),
        "--format".into(),
        "json".into(),
    ];

    let mut last_error = String::new();
    for attempt in 0..3 {
        if output_path.exists() {
            std::fs::remove_file(output_path)
                .map_err(|e| format!("清理未完成附件 {} 失败: {}", output_path.display(), e))?;
        }
        let retryable = match run_command(&dws, &args, DOWNLOAD_COMMAND_TIMEOUT, Some(cancel)) {
            Ok(out)
                if out.status.success()
                    && output_path
                        .metadata()
                        .map(|metadata| metadata.len() > 0)
                        .unwrap_or(false) =>
            {
                return Ok(());
            }
            Ok(out) if out.status.success() => {
                last_error = format!(
                    "dws 进程成功，但附件文件不存在或为空: {}",
                    output_path.display()
                );
                true
            }
            Ok(out) => {
                last_error = command_error(&out);
                is_retryable_error(&last_error, false)
            }
            Err(error) if error == CANCELLED_ERROR => return Err(error),
            Err(error) => {
                last_error = error;
                is_retryable_error(&last_error, false)
            }
        };
        if attempt == 2 || !retryable {
            break;
        }
        wait_with_cancel(Duration::from_secs(2_u64.pow(attempt + 1)), Some(cancel))?;
    }
    Err(format!("下载附件失败: {}", last_error))
}

fn run_json_with_retry(
    dws: &str,
    args: &[String],
    attempts: u32,
    cancel: Option<&AtomicBool>,
    retry_message_permission_denied: bool,
    retry_reporter: Option<RetryReporter<'_>>,
) -> Result<Value, String> {
    let mut last_error = String::new();
    for attempt in 0..attempts {
        last_error = match run_command(dws, args, JSON_COMMAND_TIMEOUT, cancel) {
            Ok(out) if out.status.success() => {
                match parse_json_output(&out.stdout, &out.stderr).and_then(validate_json_success) {
                    Ok(json) => return Ok(json),
                    Err(error) => error,
                }
            }
            Ok(out) => command_error(&out),
            Err(error) if error == CANCELLED_ERROR => return Err(error),
            Err(error) => error,
        };
        if attempt + 1 == attempts
            || !is_retryable_error(&last_error, retry_message_permission_denied)
        {
            return Err(last_error);
        }
        let delay_seconds = 2_u64.pow(attempt + 1).min(8);
        if let Some(reporter) = retry_reporter {
            reporter(attempt + 1, delay_seconds, &last_error);
        }
        wait_with_cancel(Duration::from_secs(delay_seconds), cancel)?;
    }
    Err(last_error)
}

fn command_error(out: &CapturedOutput) -> String {
    let detail = if out.stderr.trim().is_empty() {
        out.stdout.trim()
    } else {
        out.stderr.trim()
    };
    format!("dws 返回非零状态 {}: {}", out.status, preview(detail, 500))
}

fn is_retryable_error(error: &str, retry_message_permission_denied: bool) -> bool {
    let error = error.to_ascii_lowercase();
    let transient = [
        "timeout",
        "timed out",
        "temporarily unavailable",
        "connection reset",
        "connection refused",
        "too many requests",
        "status 429",
        "network",
        "执行 dws 超时",
        "超时",
    ]
    .iter()
    .any(|needle| error.contains(needle));
    transient
        || (retry_message_permission_denied
            && error.contains("auth_permission_denied")
            && error.contains("chat/list_conversation_message_v2"))
}

fn wait_with_cancel(duration: Duration, cancel: Option<&AtomicBool>) -> Result<(), String> {
    let started = Instant::now();
    while started.elapsed() < duration {
        if is_cancelled(cancel) {
            return Err(CANCELLED_ERROR.into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn parse_json_output(stdout: &str, stderr: &str) -> Result<Value, String> {
    let mut best: Option<(u8, usize, Value)> = None;
    for (position, ch) in stdout.char_indices() {
        if ch != '{' && ch != '[' {
            continue;
        }
        if let Some(Ok(value)) = serde_json::Deserializer::from_str(&stdout[position..])
            .into_iter::<Value>()
            .next()
        {
            let score = response_json_score(&value);
            if score > 0
                && best.as_ref().is_none_or(|(best_score, best_position, _)| {
                    score > *best_score || (score == *best_score && position > *best_position)
                })
            {
                best = Some((score, position, value));
            }
        }
    }
    if let Some((_, _, value)) = best {
        return Ok(value);
    }
    Err(format!(
        "dws 输出中未找到有效 JSON\nstdout: {}\nstderr: {}",
        preview(stdout, 200),
        preview(stderr, 200)
    ))
}

fn response_json_score(value: &Value) -> u8 {
    let Some(object) = value.as_object() else {
        return u8::from(value.is_array());
    };
    let mut score = 0;
    if object.contains_key("success") {
        score += 4;
    }
    if object.contains_key("result") || object.contains_key("data") {
        score += 3;
    }
    if object.contains_key("authenticated") {
        score += 3;
    }
    if object.contains_key("version") {
        score += 2;
    }
    score
}

fn validate_json_success(json: Value) -> Result<Value, String> {
    if json.get("success").and_then(Value::as_bool) == Some(false) {
        let detail = ["message", "error", "errorMessage"]
            .iter()
            .find_map(|key| json.get(key).and_then(Value::as_str))
            .unwrap_or("未提供错误详情");
        return Err(format!("dws 返回失败结果: {}", preview(detail, 500)));
    }
    Ok(json)
}

fn preview(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn required_bool_field(payload: &Value, root: &Value, keys: &[&str]) -> Result<bool, String> {
    let value = keys
        .iter()
        .find_map(|key| payload.get(key).or_else(|| root.get(key)))
        .ok_or_else(|| format!("缺少必需字段 {}", keys.join("/")))?;
    value
        .as_bool()
        .ok_or_else(|| format!("字段 {} 必须是布尔值", keys.join("/")))
}

fn string_field(payload: &Value, root: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .or_else(|| root.get(key))
        .and_then(|value| match value {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// dws 的无时区时间参数和返回 createTime 均按钉钉中国时区解释。
pub(crate) fn current_time_str() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 8 * 3600;
    let days = now / 86_400;
    let (year, month, day) = crate::date::epoch_days_to_ymd(days as i64);
    let hour = (now % 86_400) / 3600;
    let minute = (now % 3600) / 60;
    let second = now % 60;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, month, day, hour, minute, second
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutoff_is_inclusive_and_stops_only_after_crossing_boundary() {
        let cutoff = Some("2026-07-26 12:34:56");
        assert!(is_within_cutoff("2026-07-26 12:34:56", cutoff));
        assert!(is_within_cutoff("2026-07-26 12:34:57", cutoff));
        assert!(!is_within_cutoff("2026-07-26 12:34:55", cutoff));
        assert!(!crossed_cutoff("2026-07-26 12:34:56", cutoff));
        assert!(crossed_cutoff("2026-07-26 12:34:55", cutoff));
        assert!(is_within_cutoff("2000-01-01 00:00:00", None));
    }

    #[test]
    fn parses_json_after_multibyte_log_prefix_and_trailing_log() {
        let stdout = "中文日志：开始\nINFO {not-json}\n{\"success\":true,\"result\":[]}\n完成";
        let json = parse_json_output(stdout, "").unwrap();
        assert_eq!(json["success"], true);
    }

    #[test]
    fn json_log_object_is_not_mistaken_for_the_response() {
        let stdout = concat!(
            "{\"level\":\"info\",\"data\":\"starting\"}\n",
            "{\"success\":true,\"result\":{\"groups\":[],\"hasMore\":false}}\n"
        );
        let json = parse_json_output(stdout, "").unwrap();
        assert_eq!(json["success"], true);
        assert!(json["result"]["groups"].is_array());
    }

    #[test]
    fn preview_never_slices_inside_utf8() {
        assert_eq!(preview("中文内容", 3), "中文内");
    }

    #[test]
    fn rejects_success_false_json_even_when_process_succeeded() {
        let error = validate_json_success(serde_json::json!({
            "success": false,
            "message": "permission denied"
        }))
        .unwrap_err();
        assert!(error.contains("permission denied"));
    }

    #[test]
    fn parses_group_page_with_cursor() {
        let json = serde_json::json!({
            "result": {
                "groups": [{
                    "title": "测试群",
                    "openConversationId": "cid-1",
                    "memberCount": 3
                }],
                "hasMore": true,
                "nextCursor": "20"
            }
        });
        let page = parse_group_page(&json).unwrap();
        assert_eq!(page.groups.len(), 1);
        assert!(page.has_more);
        assert_eq!(page.next_cursor.as_deref(), Some("20"));
    }

    #[test]
    fn parses_message_page_and_topic_id() {
        let json = serde_json::json!({
            "result": {
                "messages": [{
                    "content": "hello",
                    "createTime": "2026-07-25 10:00:00",
                    "openMessageId": "msg-1",
                    "sender": "Alice",
                    "openConvThreadId": "topic-1"
                }],
                "hasMore": false
            }
        });
        let page = parse_message_page(&json).unwrap();
        assert_eq!(
            page.messages[0].open_conv_thread_id.as_deref(),
            Some("topic-1")
        );
    }

    #[test]
    fn message_page_requires_pagination_metadata() {
        let json = serde_json::json!({
            "result": {
                "messages": []
            }
        });
        let error = parse_message_page(&json).err().unwrap();
        assert!(error.contains("hasMore"));
    }

    #[test]
    fn optional_message_text_and_sender_have_safe_defaults() {
        let json = serde_json::json!({
            "result": {
                "messages": [{
                    "createTime": "2026-07-25 10:00:00",
                    "openMessageId": "msg-system"
                }],
                "hasMore": false
            }
        });
        let page = parse_message_page(&json).unwrap();
        assert!(page.messages[0].content.is_empty());
        assert_eq!(page.messages[0].sender, "未知发送者");
    }

    #[test]
    fn invalid_message_time_is_rejected_before_pagination() {
        let json = serde_json::json!({
            "result": {
                "messages": [{
                    "content": "hello",
                    "createTime": "2026-07-25T10:00:00",
                    "openMessageId": "msg-1",
                    "sender": "Alice"
                }],
                "hasMore": false
            }
        });
        let error = parse_message_page(&json).err().unwrap();
        assert!(error.contains("createTime 格式无效"));
    }

    #[test]
    fn retries_transient_and_scoped_message_permission_errors() {
        let message_permission_error =
            "AUTH_PERMISSION_DENIED (operation: chat/list_conversation_message_v2)";
        assert!(!is_retryable_error(message_permission_error, false));
        assert!(is_retryable_error(message_permission_error, true));
        assert!(!is_retryable_error(
            "AUTH_PERMISSION_DENIED (operation: chat/download_media)",
            true
        ));
        assert!(is_retryable_error("status 429", false));
        assert!(!is_retryable_error("invalid group id", true));
    }

    #[test]
    fn captured_output_keeps_a_bounded_tail() {
        let input = vec![b'x'; MAX_CAPTURE_BYTES + 1024];
        let captured = read_stream(std::io::Cursor::new(input)).unwrap();
        assert!(captured.truncated);
        assert_eq!(captured.bytes.len(), MAX_CAPTURE_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_descendants_without_waiting_for_inherited_pipes() {
        let started = Instant::now();
        let error = run_command(
            "/bin/sh",
            &["-c".into(), "(sleep 30) & wait".into()],
            Duration::from_millis(200),
            None,
        )
        .err()
        .unwrap();
        assert!(error.contains("超时"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
