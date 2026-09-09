// 导出核心引擎 —— 手动导出与定时调度共用的唯一实现
//
// 分层设计：
// - ExportJob：一次导出作业的全部入参（群列表、时间区间、触发来源、存档策略）
// - ArchiveStrategy trait：手动/定时唯一的差异点（目录布局、消息落盘、HTML 合并策略）
//   - PerRunArchive：现状行为，每次导出新建 {群名}_{MMDD}_{HHMMSS} 目录
//   - ScheduledGroupArchive：固定群目录 + messages/{批次} 增量落盘 + 月度 HTML 合并重建
// - ExportProgress trait：进度/日志回写抽象，解耦 Tauri 状态（便于测试与调度线程复用）
//
// 手动导出路径行为与抽离前完全一致（回归由既有测试与手动冒烟保证）。

use crate::dws::{self, Message};
use crate::export_log::{self, HtmlFileInfo};
use crate::media;
use crate::viewer;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 触发来源
#[derive(Clone, Debug)]
pub enum Trigger {
    /// 界面手动导出
    Manual,
    /// 定时调度触发
    Scheduled { schedule_id: String },
}

impl Trigger {
    pub fn trigger_type(&self) -> &'static str {
        match self {
            Trigger::Manual => "manual",
            Trigger::Scheduled { .. } => "scheduled",
        }
    }

    pub fn schedule_id(&self) -> Option<&str> {
        match self {
            Trigger::Manual => None,
            Trigger::Scheduled { schedule_id } => Some(schedule_id),
        }
    }
}

/// 群导出请求（前端 invoke 参数，camelCase）
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupExportRequest {
    pub title: String,
    pub open_conversation_id: String,
    #[serde(default)]
    pub create_at: Option<String>,
}

/// 进度/日志回写抽象
pub trait ExportProgress: Send + Sync {
    /// 追加一行日志（带北京时间戳由调用方决定，核心引擎只负责透传）
    fn log(&self, message: &str);
    /// 更新进度文本
    fn progress(&self, text: String);
    /// 当前日志快照（写入 export_log.log_lines；定时调度等无 UI 场景可返回空）
    fn log_snapshot(&self) -> Vec<String> {
        Vec::new()
    }
}

/// 无操作进度（测试与静默场景用）
pub struct NoopProgress;

impl ExportProgress for NoopProgress {
    fn log(&self, _message: &str) {}
    fn progress(&self, _text: String) {}
}

/// 单群导出结果（手动路径汇总展示；调度路径写入 ScheduleRun）
#[derive(Clone, Debug, Default)]
pub struct GroupOutcome {
    pub group_title: String,
    pub group_id: String,
    pub group_dir: PathBuf,
    /// success | partial | error | cancelled
    pub status: String,
    pub message_count: usize,
    pub attachment_success: usize,
    pub attachment_failed: usize,
    pub html_files: Vec<HtmlFileInfo>,
    pub actual_earliest: Option<String>,
    pub actual_latest: Option<String>,
    pub errors: Vec<String>,
}

/// 整个作业的运行结果
#[derive(Clone, Debug, Default)]
pub struct JobOutcome {
    /// cancelled | done | error
    pub status: String,
    pub groups: Vec<GroupOutcome>,
    pub all_errors: Vec<String>,
}

/// 存档策略：手动与定时唯一的差异分派点
pub trait ArchiveStrategy: Send + Sync {
    /// 群目录路径
    fn group_dir(&self, root: &Path, group: &GroupExportRequest, batch_stamp: &str) -> PathBuf;

    /// 本次消息 JSON 的落盘路径
    fn messages_path(&self, group_dir: &Path) -> PathBuf;

    /// 本次附件索引的落盘路径
    fn index_path(&self, group_dir: &Path) -> PathBuf;

    /// 下载附件前：已存在且非空的附件可跳过（增量复用）
    fn reuse_existing_attachment(&self) -> bool;

    /// 生成 HTML 前：返回用于渲染的消息集合。
    /// PerRun = 本次消息；ScheduledGroup = 合并全部历史批次并按消息 ID 去重。
    fn messages_for_html(
        &self,
        group_dir: &Path,
        current: &[Message],
    ) -> Result<Vec<Message>, String>;

    /// HTML 生成前：确保群目录根的 attachments_index.json 为合并索引（viewer 依赖）。
    /// PerRun 模式本次索引就在根上，无需处理；ScheduledGroup 需合并历史批次索引。
    fn prepare_root_index(&self, group_dir: &Path) -> Result<(), String>;

    /// HTML 文件名（PerRun 处理重名加序号；ScheduledGroup 固定名覆盖重建）
    fn html_filename(
        &self,
        group_title: &str,
        year_month: &str,
        group_dir: &Path,
    ) -> String;
}

/// 现状手动导出存档：每次运行新建独立目录
pub struct PerRunArchive;

impl ArchiveStrategy for PerRunArchive {
    fn group_dir(&self, root: &Path, group: &GroupExportRequest, batch_stamp: &str) -> PathBuf {
        // 与抽离前完全一致：{群名}_{MMDD}_{HHMMSS}
        let (month_day, time_part) = split_batch_stamp(batch_stamp);
        let name = format!(
            "{}_{}_{}",
            sanitize_filename(&group.title),
            month_day,
            time_part
        );
        root.join(name)
    }

    fn messages_path(&self, group_dir: &Path) -> PathBuf {
        group_dir.join("messages.json")
    }

    fn index_path(&self, group_dir: &Path) -> PathBuf {
        group_dir.join("attachments_index.json")
    }

    fn reuse_existing_attachment(&self) -> bool {
        false
    }

    fn messages_for_html(
        &self,
        _group_dir: &Path,
        current: &[Message],
    ) -> Result<Vec<Message>, String> {
        Ok(current.to_vec())
    }

    fn prepare_root_index(&self, _group_dir: &Path) -> Result<(), String> {
        Ok(())
    }

    fn html_filename(
        &self,
        group_title: &str,
        year_month: &str,
        group_dir: &Path,
    ) -> String {
        resolve_html_filename(group_title, year_month, group_dir)
    }
}

/// 定时调度存档：固定群目录 + 批次消息 + 月度 HTML 合并重建
pub struct ScheduledGroupArchive;

impl ScheduledGroupArchive {
    /// 批次时间戳格式：MMDD_HHMMSS → 批次目录名 YYYYMMDD_HHMMSS（跨月排序友好）
    fn batch_dir_name(batch_stamp: &str) -> String {
        batch_stamp.replace('-', "").replace(' ', "_").replace(':', "")
    }
}

impl ArchiveStrategy for ScheduledGroupArchive {
    fn group_dir(&self, root: &Path, group: &GroupExportRequest, _batch_stamp: &str) -> PathBuf {
        // 固定目录：{群名}_{群ID哈希}，跨次运行同一目录
        let hash = stable_hash(&group.open_conversation_id);
        let name = format!(
            "{}_{:08x}",
            sanitize_filename(&group.title),
            (hash & 0xFFFF_FFFF) as u32
        );
        root.join(name)
    }

    fn messages_path(&self, group_dir: &Path) -> PathBuf {
        // 由 run_job 在调用前把批次目录写入 group_dir 下的 messages/{批次}/
        group_dir.join("messages.json")
    }

    fn index_path(&self, group_dir: &Path) -> PathBuf {
        group_dir.join("attachments_index.json")
    }

    fn reuse_existing_attachment(&self) -> bool {
        true
    }

    fn messages_for_html(
        &self,
        group_dir: &Path,
        current: &[Message],
    ) -> Result<Vec<Message>, String> {
        let mut merged: Vec<Message> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // 收集全部历史批次消息
        let messages_root = group_dir.join("messages");
        if messages_root.is_dir() {
            let mut batches: Vec<PathBuf> = fs::read_dir(&messages_root)
                .map_err(|error| format!("读取批次目录失败: {error}"))?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.is_dir())
                .collect();
            batches.sort();
            for batch in batches {
                let path = batch.join("messages.json");
                if !path.exists() {
                    continue;
                }
                let content = fs::read_to_string(&path)
                    .map_err(|error| format!("读取批次消息失败 {}: {}", path.display(), error))?;
                let batch_messages: Vec<Message> = serde_json::from_str(&content)
                    .map_err(|error| format!("解析批次消息失败 {}: {}", path.display(), error))?;
                for message in batch_messages {
                    if seen.insert(message.open_message_id.clone()) {
                        merged.push(message);
                    }
                }
            }
        }
        // 当前批次若尚未落盘到 messages/（防御），也并入
        for message in current {
            if seen.insert(message.open_message_id.clone()) {
                merged.push(message.clone());
            }
        }
        merged.sort_by(|a, b| {
            a.create_time
                .cmp(&b.create_time)
                .then_with(|| a.open_message_id.cmp(&b.open_message_id))
        });
        Ok(merged)
    }

    fn prepare_root_index(&self, group_dir: &Path) -> Result<(), String> {
        // 合并所有批次索引到群目录根（viewer::build_media_map 读取根索引）
        let mut merged: Vec<serde_json::Value> = Vec::new();
        let mut seen_keys = std::collections::HashSet::new();
        let messages_root = group_dir.join("messages");
        if messages_root.is_dir() {
            let mut batches: Vec<PathBuf> = fs::read_dir(&messages_root)
                .map_err(|error| format!("读取批次目录失败: {error}"))?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.is_dir())
                .collect();
            batches.sort();
            for batch in batches {
                let path = batch.join("attachments_index.json");
                if !path.exists() {
                    continue;
                }
                let content = fs::read_to_string(&path)
                    .map_err(|error| format!("读取批次附件索引失败 {}: {}", path.display(), error))?;
                let records: Vec<serde_json::Value> = serde_json::from_str(&content)
                    .map_err(|error| format!("解析批次附件索引失败 {}: {}", path.display(), error))?;
                for record in records {
                    // 去重键：mediaId + file；同一附件重复拉取时保留 status=ok 的记录
                    let key = format!(
                        "{}|{}",
                        record.get("mediaId").and_then(|v| v.as_str()).unwrap_or(""),
                        record.get("file").and_then(|v| v.as_str()).unwrap_or("")
                    );
                    if seen_keys.contains(&key) {
                        // 已存在记录：若新记录成功而旧记录失败，替换之
                        if record.get("status").and_then(|v| v.as_str()) == Some("ok") {
                            if let Some(existing) = merged
                                .iter_mut()
                                .find(|item| {
                                    format!(
                                        "{}|{}",
                                        item.get("mediaId").and_then(|v| v.as_str()).unwrap_or(""),
                                        item.get("file").and_then(|v| v.as_str()).unwrap_or("")
                                    ) == key
                                })
                            {
                                if existing.get("status").and_then(|v| v.as_str()) != Some("ok") {
                                    *existing = record;
                                }
                            }
                        }
                        continue;
                    }
                    seen_keys.insert(key);
                    merged.push(record);
                }
            }
        }
        write_json(&group_dir.join("attachments_index.json"), &merged)
    }

    fn html_filename(
        &self,
        group_title: &str,
        year_month: &str,
        _group_dir: &Path,
    ) -> String {
        // 固定文件名：合并重建时直接覆盖同名月度 HTML
        format!("{}-{}.html", sanitize_filename(group_title), year_month)
    }
}

/// 一次导出作业
pub struct ExportJob {
    pub groups: Vec<GroupExportRequest>,
    pub output_root: String,
    /// 登录用户名（HTML 抬头）
    pub self_name: String,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub trigger: Trigger,
    pub archive: Box<dyn ArchiveStrategy>,
    pub cancel: Arc<AtomicBool>,
}

/// 执行导出作业（核心实现，手动/定时共用）
pub fn run_job(
    job: &ExportJob,
    progress: &dyn ExportProgress,
) -> JobOutcome {
    let root = PathBuf::from(&job.output_root);
    let mut outcome = JobOutcome::default();

    if let Err(error) = fs::create_dir_all(&root) {
        outcome.status = "error".into();
        outcome.all_errors.push(format!(
            "创建输出目录 {} 失败: {}",
            root.display(),
            error
        ));
        return outcome;
    }

    for (group_index, group) in job.groups.iter().enumerate() {
        if job.cancel.load(Ordering::Relaxed) {
            break;
        }
        progress.log(&format!(
            "\n===== [{}/{}] 导出群: {} =====",
            group_index + 1,
            job.groups.len(),
            group.title
        ));
        progress.progress(format!(
            "[{}/{}] 正在导出: {}",
            group_index + 1,
            job.groups.len(),
            group.title
        ));

        match export_single_group(job, group, progress) {
            Ok(group_outcome) => {
                outcome.all_errors.extend(group_outcome.errors.iter().cloned());
                outcome.groups.push(group_outcome);
            }
            Err(GroupError::Cancelled) => break,
            Err(GroupError::Failed(errors)) => {
                outcome.all_errors.extend(errors);
            }
        }
    }

    outcome.status = if job.cancel.load(Ordering::Relaxed) {
        "cancelled".into()
    } else if outcome.all_errors.is_empty() {
        "done".into()
    } else {
        "error".into()
    };
    outcome
}

enum GroupError {
    Cancelled,
    Failed(Vec<String>),
}

/// 导出单个群：拉消息 → 下附件 → 落盘 → 月度 HTML → 写导出日志
fn export_single_group(
    job: &ExportJob,
    group: &GroupExportRequest,
    progress: &dyn ExportProgress,
) -> Result<GroupOutcome, GroupError> {
    let mut errors: Vec<String> = Vec::new();
    let batch_stamp = dws::current_time_str();
    let group_dir = job.archive.group_dir(
        Path::new(&job.output_root),
        group,
        &batch_stamp,
    );

    // ScheduledGroup 模式：消息与索引落盘到批次子目录
    let (messages_path, index_path, attachment_dir) =
        if matches!(job.trigger, Trigger::Scheduled { .. }) {
            let batch_name = ScheduledGroupArchive::batch_dir_name(&batch_stamp);
            let batch_dir = group_dir.join("messages").join(&batch_name);
            if let Err(error) = fs::create_dir_all(&batch_dir) {
                return Err(GroupError::Failed(vec![format!(
                    "群「{}」创建批次目录失败: {}",
                    group.title, error
                )]));
            }
            (
                batch_dir.join("messages.json"),
                batch_dir.join("attachments_index.json"),
                group_dir.join("attachments"),
            )
        } else {
            if let Err(error) = fs::create_dir_all(&group_dir) {
                return Err(GroupError::Failed(vec![format!(
                    "群「{}」创建目录失败: {}",
                    group.title, error
                )]));
            }
            (
                job.archive.messages_path(&group_dir),
                job.archive.index_path(&group_dir),
                group_dir.join("attachments"),
            )
        };

    if let Err(error) = fs::create_dir_all(&attachment_dir) {
        return Err(GroupError::Failed(vec![format!(
            "群「{}」创建附件目录失败: {}",
            group.title, error
        )]));
    }

    // 时间范围提示
    match &job.start_time {
        Some(start) => progress.log(&format!("群「{}」开始时间: {}", group.title, start)),
        None => progress.log(&format!("群「{}」未设置开始时间，从最早消息开始", group.title)),
    }
    match &job.end_time {
        Some(end) => progress.log(&format!("群「{}」结束时间: {}", group.title, end)),
        None => progress.log(&format!("群「{}」未设置结束时间，到最新消息结束", group.title)),
    }

    // 1. 拉取消息
    let messages = match dws::fetch_all_messages(
        &group.open_conversation_id,
        job.start_time.as_deref(),
        job.end_time.as_deref(),
        &|count, earliest| {
            progress.progress(format!("拉取消息: {} 条（至 {}）", count, earliest));
        },
        &|message| progress.log(message),
        &job.cancel,
    ) {
        Ok(messages) => messages,
        Err(error) if error == dws::CANCELLED_ERROR => return Err(GroupError::Cancelled),
        Err(error) => {
            return Err(GroupError::Failed(vec![format!(
                "群「{}」拉取消息失败: {}",
                group.title, error
            )]));
        }
    };
    progress.log(&format!("共拉取 {} 条消息（含话题回复）", messages.len()));

    let actual_earliest = messages.first().map(|message| message.create_time.clone());
    let actual_latest = messages.last().map(|message| message.create_time.clone());
    if let Some(earliest) = &actual_earliest {
        progress.log(&format!("实际最早消息: {}", earliest));
    }
    if let Some(latest) = &actual_latest {
        progress.log(&format!("实际最新消息: {}", latest));
    }

    // 2. 消息落盘
    if let Err(error) = write_json(&messages_path, &messages) {
        return Err(GroupError::Failed(vec![format!(
            "群「{}」写 messages.json 失败: {}",
            group.title, error
        )]));
    }

    // 3. 下载附件
    let media_count: usize = messages
        .iter()
        .map(|message| media::extract_media_ids(&message.content).len())
        .sum();
    let mut media_index = 0usize;
    let mut reused_count = 0usize;
    let mut attachments: Vec<serde_json::Value> = Vec::with_capacity(media_count);
    if media_count > 0 {
        progress.log(&format!("开始处理 {} 个附件", media_count));
    }

    'messages: for message in &messages {
        for (file_index, media_id) in media::extract_media_ids(&message.content)
            .into_iter()
            .enumerate()
        {
            if job.cancel.load(Ordering::Relaxed) {
                break 'messages;
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
            let year_month = extract_year_month(&message.create_time);
            let attachment_subdir = attachment_dir.join(&year_month);
            if let Err(error) = fs::create_dir_all(&attachment_subdir) {
                let detail = format!(
                    "创建附件目录失败 {}: {}",
                    attachment_subdir.display(),
                    error
                );
                progress.log(&detail);
                errors.push(detail);
                continue;
            }
            let output_path = attachment_subdir.join(&file_name);
            let relative_file_path = format!("{}/{}", year_month, file_name);

            // 增量复用：已存在且非空的附件跳过下载
            if job.archive.reuse_existing_attachment()
                && output_path
                    .metadata()
                    .is_ok_and(|metadata| metadata.len() > 0)
            {
                reused_count += 1;
                attachments.push(serde_json::json!({
                    "openMessageId": message.open_message_id,
                    "createTime": message.create_time,
                    "sender": message.sender,
                    "mediaId": media_id,
                    "file": relative_file_path,
                    "originalFileName": media::extract_original_file_name(&message.content)
                        .map(|name| sanitize_filename(&name)),
                    "status": "ok",
                    "error": serde_json::Value::Null,
                }));
                continue;
            }

            progress.progress(format!(
                "[{}/{}] 下载附件: {}",
                media_index, media_count, file_name
            ));
            let result = dws::download_media(
                &group.open_conversation_id,
                &message.open_message_id,
                &media_id,
                &output_path,
                &job.cancel,
            );
            let (status, error_text) = match result {
                Ok(()) => ("ok", None),
                Err(error) if error == dws::CANCELLED_ERROR => ("cancelled", Some(error)),
                Err(error) => {
                    let detail = format!(
                        "群「{}」附件 #{} 下载失败: {}",
                        group.title, media_index, error
                    );
                    progress.log(&detail);
                    errors.push(detail);
                    ("fail", Some(error))
                }
            };
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
                break 'messages;
            }
        }
    }

    if job.cancel.load(Ordering::Relaxed) {
        // 取消：尽力保存断点索引
        if let Err(error) = write_json(&index_path, &attachments) {
            progress.log(&format!("保存断点附件索引失败: {error}"));
        }
        return Err(GroupError::Cancelled);
    }

    if let Err(error) = write_json(&index_path, &attachments) {
        errors.push(format!(
            "群「{}」写 attachments_index.json 失败: {}",
            group.title, error
        ));
        return Err(GroupError::Failed(errors));
    }
    let successful_attachments = attachments
        .iter()
        .filter(|attachment| attachment["status"] == "ok")
        .count();
    if reused_count > 0 {
        progress.log(&format!("附件增量复用: {} 个已存在，跳过下载", reused_count));
    }
    progress.log(&format!(
        "附件下载完成: 成功 {}/{}",
        successful_attachments, media_count
    ));

    // 4. 月度 HTML 生成
    progress.progress("准备生成聊天记录页面...".to_string());

    // ScheduledGroup：先合并根索引（viewer 依赖群目录根的 attachments_index.json）
    if let Err(error) = job.archive.prepare_root_index(&group_dir) {
        errors.push(format!("群「{}」合并附件索引失败: {}", group.title, error));
    }

    let html_messages = match job.archive.messages_for_html(&group_dir, &messages) {
        Ok(merged) => merged,
        Err(error) => {
            errors.push(format!("群「{}」合并历史消息失败: {}", group.title, error));
            messages.clone()
        }
    };

    let mut messages_by_month: BTreeMap<String, Vec<&Message>> = BTreeMap::new();
    for message in &html_messages {
        let year_month = extract_year_month(&message.create_time);
        messages_by_month.entry(year_month).or_default().push(message);
    }
    progress.log(&format!(
        "HTML 渲染范围: {} 条消息，分布在 {} 个月份",
        html_messages.len(),
        messages_by_month.len()
    ));

    let mut html_files_info: Vec<HtmlFileInfo> = Vec::new();
    for (year_month, month_messages) in &messages_by_month {
        if job.cancel.load(Ordering::Relaxed) {
            break;
        }
        progress.progress(format!(
            "生成 {} 年 {} 月聊天记录...",
            &year_month[0..4.min(year_month.len())],
            if year_month.len() >= 6 { &year_month[4..6] } else { "?" }
        ));
        let month_attachment_count: usize = month_messages
            .iter()
            .map(|message| media::extract_media_ids(&message.content).len())
            .sum();
        let html_file_name =
            job.archive
                .html_filename(&group.title, year_month, &group_dir);
        let html_path = group_dir.join(&html_file_name);
        match viewer::generate_html(
            &month_messages.iter().map(|message| (*message).clone()).collect::<Vec<_>>(),
            &group.title,
            &attachment_dir,
            &job.self_name,
            &html_path,
            &job.cancel,
        ) {
            Ok(()) => {
                let file_size = html_path.metadata().map(|meta| meta.len()).unwrap_or(0);
                progress.log(&format!(
                    "已生成: {}（{} 条消息, {} 个附件, {:.1} MB）",
                    html_file_name,
                    month_messages.len(),
                    month_attachment_count,
                    file_size as f64 / 1_048_576.0
                ));
                html_files_info.push(HtmlFileInfo {
                    filename: html_file_name.clone(),
                    year_month: year_month.clone(),
                    message_count: month_messages.len(),
                    attachment_count: month_attachment_count,
                    file_size_bytes: file_size,
                });
            }
            Err(error) if error == dws::CANCELLED_ERROR => break,
            Err(error) => {
                let detail = format!(
                    "群「{}」生成 {} HTML 失败: {}",
                    group.title, year_month, error
                );
                progress.log(&detail);
                errors.push(detail);
            }
        }
    }

    if job.cancel.load(Ordering::Relaxed) {
        return Err(GroupError::Cancelled);
    }

    progress.log(&format!(
        "群「{}」已完整发布到 {}",
        group.title,
        group_dir.display()
    ));

    // 5. 写导出日志（export_logs.json 追加，含 triggerType）
    let failed_attachments = attachments
        .iter()
        .filter(|attachment| attachment["status"] == "fail")
        .count();
    let directory_name = group_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let log_entry = export_log::ExportLogEntry {
        id: export_log::generate_timestamp_id(),
        group_name: group.title.clone(),
        group_id: group.open_conversation_id.clone(),
        directory_name,
        export_time: batch_stamp.clone(),
        start_time: job.start_time.clone(),
        end_time: job.end_time.clone(),
        actual_earliest: actual_earliest.clone(),
        actual_latest: actual_latest.clone(),
        message_count: messages.len(),
        attachment_total: successful_attachments + failed_attachments,
        attachment_success: successful_attachments,
        attachment_failed: failed_attachments,
        html_files: html_files_info.clone(),
        status: if errors.is_empty() {
            "success".into()
        } else {
            "partial".into()
        },
        error_message: if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        },
        log_lines: progress.log_snapshot(),
        trigger_type: Some(job.trigger.trigger_type().to_string()),
        schedule_id: job.trigger.schedule_id().map(|id| id.to_string()),
    };
    if let Err(error) = export_log::append_export_log(&group_dir, &log_entry) {
        progress.log(&format!("警告: 写入导出日志失败: {}", error));
    }

    Ok(GroupOutcome {
        group_title: group.title.clone(),
        group_id: group.open_conversation_id.clone(),
        group_dir,
        status: if errors.is_empty() {
            "success".into()
        } else {
            "partial".into()
        },
        message_count: messages.len(),
        attachment_success: successful_attachments,
        attachment_failed: failed_attachments,
        html_files: html_files_info,
        actual_earliest,
        actual_latest,
        errors,
    })
}

// ===== 以下为从 lib.rs 迁移的导出辅助函数（行为不变） =====

/// 把 "yyyy-MM-dd HH:mm:ss" 拆成 ("MMDD", "HHMMSS")
fn split_batch_stamp(stamp: &str) -> (String, String) {
    if stamp.len() >= 19 {
        (stamp[5..10].replace('-', ""), stamp[11..19].replace(':', ""))
    } else {
        ("0000".to_string(), "000000".to_string())
    }
}

pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    // 原子写入：先写临时文件，成功后重命名
    let temp_path = path.with_extension("tmp");
    let file = fs::File::create(&temp_path)
        .map_err(|error| format!("创建 {} 失败: {}", temp_path.display(), error))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)
        .map_err(|error| format!("序列化 {} 失败: {}", temp_path.display(), error))?;
    writer
        .flush()
        .map_err(|error| format!("刷新 {} 失败: {}", temp_path.display(), error))?;
    drop(writer);
    fs::rename(&temp_path, path)
        .map_err(|error| format!("重命名 {} 失败: {}", temp_path.display(), error))
}

pub(crate) fn media_extension(content: &str) -> String {
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

pub(crate) fn timestamp_fragment(timestamp: &str) -> String {
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

pub fn sanitize_filename(name: &str) -> String {
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

/// 从日期时间字符串提取年月（YYYYMM）
pub(crate) fn extract_year_month(datetime: &str) -> String {
    crate::date::extract_year_month(datetime).unwrap_or_else(|| "unknown".to_string())
}

/// 生成 HTML 文件名，处理重名：群名-YYYYMM.html，重名加序号 -01, -02...
pub(crate) fn resolve_html_filename(group_title: &str, year_month: &str, group_dir: &Path) -> String {
    let safe_title = sanitize_filename(group_title);
    let base_filename = format!("{}-{}.html", safe_title, year_month);
    if !group_dir.join(&base_filename).exists() {
        return base_filename;
    }
    for seq in 1..=999 {
        let candidate = format!("{}-{}-{:02}.html", safe_title, year_month, seq);
        if !group_dir.join(&candidate).exists() {
            return candidate;
        }
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{}-{}-{}.html", safe_title, year_month, timestamp)
}

pub(crate) fn safe_id_fragment(id: &str, max_length: usize) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_batch_stamp_matches_legacy_format() {
        assert_eq!(
            split_batch_stamp("2026-01-15 10:30:00"),
            ("0115".to_string(), "103000".to_string())
        );
        assert_eq!(
            split_batch_stamp("bad"),
            ("0000".to_string(), "000000".to_string())
        );
    }

    #[test]
    fn per_run_group_dir_matches_legacy_naming() {
        let archive = PerRunArchive;
        let group = GroupExportRequest {
            title: "测试群".into(),
            open_conversation_id: "cid_x".into(),
            create_at: None,
        };
        let dir = archive.group_dir(Path::new("D:/exports"), &group, "2026-01-15 10:30:00");
        assert_eq!(
            dir,
            PathBuf::from("D:/exports").join("测试群_0115_103000")
        );
    }

    #[test]
    fn scheduled_group_dir_is_stable_across_runs() {
        let archive = ScheduledGroupArchive;
        let group = GroupExportRequest {
            title: "测试群".into(),
            open_conversation_id: "cid_x".into(),
            create_at: None,
        };
        let first = archive.group_dir(Path::new("D:/exports"), &group, "2026-01-15 10:30:00");
        let second = archive.group_dir(Path::new("D:/exports"), &group, "2026-02-20 08:00:00");
        assert_eq!(first, second); // 固定目录
        // 不同群 ID → 不同目录
        let other = GroupExportRequest {
            title: "测试群".into(),
            open_conversation_id: "cid_y".into(),
            create_at: None,
        };
        let other_dir = archive.group_dir(Path::new("D:/exports"), &other, "2026-01-15 10:30:00");
        assert_ne!(first, other_dir);
    }

    #[test]
    fn scheduled_html_filename_is_fixed_for_overwrite() {
        let archive = ScheduledGroupArchive;
        assert_eq!(
            archive.html_filename("测试群", "202601", Path::new("x")),
            "测试群-202601.html"
        );
    }

    #[test]
    fn scheduled_messages_merge_dedupes_and_sorts() {
        let temp_dir = std::env::temp_dir().join(format!(
            "dingtalk-exporter-merge-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_dir);
        let batch_a = temp_dir.join("messages").join("20260101_023000");
        let batch_b = temp_dir.join("messages").join("20260201_023000");
        fs::create_dir_all(&batch_a).unwrap();
        fs::create_dir_all(&batch_b).unwrap();

        let message = |id: &str, time: &str| Message {
            content: format!("内容-{id}"),
            create_time: time.into(),
            open_message_id: id.into(),
            sender: "张三".into(),
            sender_open_dingtalk_id: None,
            open_conv_thread_id: None,
        };
        // 批次A: m1, m2；批次B: m2(重复), m3；当前批次: m3(重复), m4
        write_json(&batch_a.join("messages.json"), &vec![
            message("m1", "2026-01-01 10:00:00"),
            message("m2", "2026-01-15 10:00:00"),
        ]).unwrap();
        write_json(&batch_b.join("messages.json"), &vec![
            message("m2", "2026-01-15 10:00:00"),
            message("m3", "2026-02-01 10:00:00"),
        ]).unwrap();

        let archive = ScheduledGroupArchive;
        let merged = archive
            .messages_for_html(&temp_dir, &[
                message("m3", "2026-02-01 10:00:00"),
                message("m4", "2026-02-15 10:00:00"),
            ])
            .unwrap();
        assert_eq!(merged.len(), 4);
        let ids: Vec<&str> = merged
            .iter()
            .map(|message| message.open_message_id.as_str())
            .collect();
        assert_eq!(ids, vec!["m1", "m2", "m3", "m4"]); // 去重且按时间排序

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn scheduled_root_index_merges_and_prefers_ok_status() {
        let temp_dir = std::env::temp_dir().join(format!(
            "dingtalk-exporter-index-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_dir);
        let batch_a = temp_dir.join("messages").join("20260101_023000");
        let batch_b = temp_dir.join("messages").join("20260201_023000");
        fs::create_dir_all(&batch_a).unwrap();
        fs::create_dir_all(&batch_b).unwrap();

        // 批次A：media1 失败；批次B：media1 成功 + media2 成功
        write_json(&batch_a.join("attachments_index.json"), &vec![
            serde_json::json!({ "mediaId": "media1", "file": "202601/a.jpg", "status": "fail" }),
        ]).unwrap();
        write_json(&batch_b.join("attachments_index.json"), &vec![
            serde_json::json!({ "mediaId": "media1", "file": "202601/a.jpg", "status": "ok" }),
            serde_json::json!({ "mediaId": "media2", "file": "202602/b.jpg", "status": "ok" }),
        ]).unwrap();

        let archive = ScheduledGroupArchive;
        archive.prepare_root_index(&temp_dir).unwrap();
        let content = fs::read_to_string(temp_dir.join("attachments_index.json")).unwrap();
        let records: Vec<serde_json::Value> = serde_json::from_str(&content).unwrap();
        assert_eq!(records.len(), 2);
        let media1 = records.iter().find(|r| r["mediaId"] == "media1").unwrap();
        assert_eq!(media1["status"], "ok"); // 成功记录覆盖失败记录

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    // ===== 从 lib.rs 迁移的辅助函数测试 =====

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
        assert_eq!(
            resolve_html_filename("研发/值班:日报*?", "202601", &temp_dir),
            "研发值班日报-202601.html"
        );
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
    fn trigger_type_and_schedule_id() {
        assert_eq!(Trigger::Manual.trigger_type(), "manual");
        assert_eq!(Trigger::Manual.schedule_id(), None);
        let scheduled = Trigger::Scheduled { schedule_id: "sch_1".into() };
        assert_eq!(scheduled.trigger_type(), "scheduled");
        assert_eq!(scheduled.schedule_id(), Some("sch_1"));
    }
}
