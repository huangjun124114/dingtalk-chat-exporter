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
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

/// 一次 HTML 生成计划：一个月对应一个
pub struct HtmlMonth {
    pub year_month: String,
    pub messages: Vec<Message>,
    /// 该月消息对应的附件索引文件
    pub index_path: PathBuf,
}

/// 存档策略：手动与定时唯一的差异分派点
pub trait ArchiveStrategy: Send + Sync {
    /// 群目录路径
    fn group_dir(&self, root: &Path, group: &GroupExportRequest, batch_stamp: &str) -> PathBuf;

    /// 下载附件前：已存在且非空的附件可跳过（增量复用）
    fn reuse_existing_attachment(&self) -> bool;

    /// 落盘本次消息，返回内容发生变化的月份（YYYYMM）集合
    fn persist_messages(
        &self,
        group_dir: &Path,
        messages: &[Message],
        batch_stamp: &str,
    ) -> Result<Vec<String>, String>;

    /// 落盘本次附件索引，返回内容发生变化的月份集合
    fn persist_index(
        &self,
        group_dir: &Path,
        records: &[serde_json::Value],
    ) -> Result<Vec<String>, String>;

    /// 计算需要（重新）生成 HTML 的月份及其数据来源
    fn html_months(
        &self,
        group_dir: &Path,
        group_title: &str,
        changed_months: &[String],
    ) -> Result<Vec<HtmlMonth>, String>;

    /// HTML 文件名（PerRun 处理重名加序号；ScheduledGroup 固定名覆盖重建）
    fn html_filename(&self, group_title: &str, year_month: &str, group_dir: &Path) -> String;

    /// 兼容旧目录布局：把历史数据迁移到当前布局（幂等，默认无需处理）
    fn migrate_legacy(&self, _group_dir: &Path) -> Result<(), String> {
        Ok(())
    }
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

    fn reuse_existing_attachment(&self) -> bool {
        false
    }

    /// 每次运行独立目录：消息与索引各写单文件（与抽离前行为一致）
    fn persist_messages(
        &self,
        group_dir: &Path,
        messages: &[Message],
        _batch_stamp: &str,
    ) -> Result<Vec<String>, String> {
        write_json(&group_dir.join("messages.json"), &messages)?;
        Ok(Vec::new())
    }

    fn persist_index(
        &self,
        group_dir: &Path,
        records: &[serde_json::Value],
    ) -> Result<Vec<String>, String> {
        write_json(&group_dir.join("attachments_index.json"), records)?;
        Ok(Vec::new())
    }

    fn html_months(
        &self,
        group_dir: &Path,
        _group_title: &str,
        _changed_months: &[String],
    ) -> Result<Vec<HtmlMonth>, String> {
        let messages = read_messages(&group_dir.join("messages.json"))?;
        let index_path = group_dir.join("attachments_index.json");
        Ok(
            group_by_month(messages, |message| extract_year_month(&message.create_time))
                .into_iter()
                .map(|(year_month, messages)| HtmlMonth {
                    year_month,
                    messages,
                    index_path: index_path.clone(),
                })
                .collect(),
        )
    }

    fn html_filename(&self, group_title: &str, year_month: &str, group_dir: &Path) -> String {
        resolve_html_filename(group_title, year_month, group_dir)
    }
}

/// 定时调度存档：固定群目录 + 批次消息 + 月度 HTML 合并重建
pub struct ScheduledGroupArchive;

impl ScheduledGroupArchive {
    /// 合并写入某月的消息文件（按消息 ID 去重），返回内容是否发生变化
    fn write_month_messages(
        group_dir: &Path,
        year_month: &str,
        incoming: Vec<Message>,
    ) -> Result<bool, String> {
        let path = month_messages_path(group_dir, year_month);
        let mut merged = read_messages(&path)?;
        merge_messages(&mut merged, incoming);
        write_json_if_changed(&path, &merged)
    }

    /// 合并写入某月的附件索引（按 mediaId+file 去重），返回内容是否发生变化
    fn write_month_records(
        group_dir: &Path,
        year_month: &str,
        incoming: Vec<serde_json::Value>,
    ) -> Result<bool, String> {
        let path = month_index_path(group_dir, year_month);
        let mut merged = read_records(&path)?;
        merge_records(&mut merged, incoming);
        write_json_if_changed(&path, &merged)
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

    fn reuse_existing_attachment(&self) -> bool {
        true
    }

    /// 消息按月落盘：messages/{YYYYMM}.json（与 HTML 同粒度，避免单文件无限膨胀）
    fn persist_messages(
        &self,
        group_dir: &Path,
        messages: &[Message],
        _batch_stamp: &str,
    ) -> Result<Vec<String>, String> {
        let grouped = group_by_month(messages.to_vec(), |message| {
            extract_year_month(&message.create_time)
        });
        let mut changed = Vec::new();
        for (year_month, month_messages) in grouped {
            if Self::write_month_messages(group_dir, &year_month, month_messages)? {
                changed.push(year_month);
            }
        }
        Ok(changed)
    }

    /// 附件索引按月落盘：attachments_index/{YYYYMM}.json
    fn persist_index(
        &self,
        group_dir: &Path,
        records: &[serde_json::Value],
    ) -> Result<Vec<String>, String> {
        let grouped = group_by_month(records.to_vec(), record_month);
        let mut changed = Vec::new();
        for (year_month, month_records) in grouped {
            if Self::write_month_records(group_dir, &year_month, month_records)? {
                changed.push(year_month);
            }
        }
        Ok(changed)
    }

    /// 只重建「本次有变化 / HTML 缺失 / 数据比 HTML 新」的月份，避免每次全量重写
    fn html_months(
        &self,
        group_dir: &Path,
        group_title: &str,
        changed_months: &[String],
    ) -> Result<Vec<HtmlMonth>, String> {
        let messages_root = group_dir.join("messages");
        let mut months: BTreeSet<String> = changed_months.iter().cloned().collect();
        if messages_root.is_dir() {
            for entry in fs::read_dir(&messages_root)
                .map_err(|error| format!("读取 {} 失败: {}", messages_root.display(), error))?
            {
                let path = entry
                    .map_err(|error| format!("读取月度消息目录失败: {error}"))?
                    .path();
                if !path.is_file() {
                    continue;
                }
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
                    months.insert(stem.to_string());
                }
            }
        }

        let mut plan = Vec::new();
        for year_month in months {
            let message_path = month_messages_path(group_dir, &year_month);
            if !message_path.is_file() {
                continue; // 只有索引变化的月份（如首次生成索引）无消息可渲染
            }
            let html_path = group_dir.join(self.html_filename(group_title, &year_month, group_dir));
            let index_path = month_index_path(group_dir, &year_month);
            let need_rebuild = changed_months.iter().any(|month| month == &year_month)
                || !html_path.is_file()
                || is_newer(&message_path, &html_path)
                || is_newer(&index_path, &html_path);
            if !need_rebuild {
                continue;
            }
            let messages = read_messages(&message_path)?;
            if messages.is_empty() {
                continue;
            }
            plan.push(HtmlMonth {
                year_month,
                messages,
                index_path,
            });
        }
        Ok(plan)
    }

    fn html_filename(&self, group_title: &str, year_month: &str, _group_dir: &Path) -> String {
        // 固定文件名：合并重建时直接覆盖同名月度 HTML
        format!("{}-{}.html", sanitize_filename(group_title), year_month)
    }

    /// 旧布局（messages/{批次}/messages.json + 根 attachments_index.json）
    /// 一次性合并进月度文件；确认写入成功后删除旧数据。
    fn migrate_legacy(&self, group_dir: &Path) -> Result<(), String> {
        let messages_root = group_dir.join("messages");
        if !messages_root.is_dir() {
            return Ok(());
        }
        let mut batches: Vec<PathBuf> = fs::read_dir(&messages_root)
            .map_err(|error| format!("读取 {} 失败: {}", messages_root.display(), error))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        if batches.is_empty() {
            return Ok(());
        }
        batches.sort();

        // 1. 读取全部旧批次（消息 + 附件索引），按月归集
        let mut month_messages: BTreeMap<String, Vec<Message>> = BTreeMap::new();
        let mut month_records: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
        for batch in &batches {
            for message in read_messages(&batch.join("messages.json"))? {
                month_messages
                    .entry(extract_year_month(&message.create_time))
                    .or_default()
                    .push(message);
            }
            for record in read_records(&batch.join("attachments_index.json"))? {
                month_records
                    .entry(record_month(&record))
                    .or_default()
                    .push(record);
            }
        }

        // 2. 先合并进月度文件（写入成功后才允许删除旧数据）
        for (year_month, messages) in month_messages {
            Self::write_month_messages(group_dir, &year_month, messages)?;
        }
        for (year_month, records) in month_records {
            Self::write_month_records(group_dir, &year_month, records)?;
        }

        // 3. 删除旧批次目录（内容已完整并入月度文件）
        for batch in &batches {
            if let Err(error) = fs::remove_dir_all(batch) {
                eprintln!(
                    "[exporter] 删除旧批次目录 {} 失败: {error}",
                    batch.display()
                );
            }
        }

        // 4. 根合并索引已由月度索引覆盖，移除避免继续膨胀
        let root_index = group_dir.join("attachments_index.json");
        if root_index.is_file() {
            if let Err(error) = fs::remove_file(&root_index) {
                eprintln!(
                    "[exporter] 移除根附件索引 {} 失败: {error}",
                    root_index.display()
                );
            }
        }
        Ok(())
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
pub fn run_job(job: &ExportJob, progress: &dyn ExportProgress) -> JobOutcome {
    let root = PathBuf::from(&job.output_root);
    let mut outcome = JobOutcome::default();

    if let Err(error) = fs::create_dir_all(&root) {
        outcome.status = "error".into();
        outcome
            .all_errors
            .push(format!("创建输出目录 {} 失败: {}", root.display(), error));
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
                outcome
                    .all_errors
                    .extend(group_outcome.errors.iter().cloned());
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
    let group_dir = job
        .archive
        .group_dir(Path::new(&job.output_root), group, &batch_stamp);

    let attachment_dir = group_dir.join("attachments");
    if let Err(error) = fs::create_dir_all(&attachment_dir) {
        return Err(GroupError::Failed(vec![format!(
            "群「{}」创建输出目录失败: {}",
            group.title, error
        )]));
    }

    // 0. 兼容旧布局：历史批次目录一次性合并进月度 JSON（幂等）
    if let Err(error) = job.archive.migrate_legacy(&group_dir) {
        progress.log(&format!("群「{}」迁移历史批次失败: {}", group.title, error));
    }
    let mut changed_months: Vec<String> = Vec::new();

    // 时间范围提示
    match &job.start_time {
        Some(start) => progress.log(&format!("群「{}」开始时间: {}", group.title, start)),
        None => progress.log(&format!(
            "群「{}」未设置开始时间，从最早消息开始",
            group.title
        )),
    }
    match &job.end_time {
        Some(end) => progress.log(&format!("群「{}」结束时间: {}", group.title, end)),
        None => progress.log(&format!(
            "群「{}」未设置结束时间，到最新消息结束",
            group.title
        )),
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

    // 2. 消息落盘（按月合并去重：messages/{YYYYMM}.json）
    match job
        .archive
        .persist_messages(&group_dir, &messages, &batch_stamp)
    {
        Ok(months) => {
            if !months.is_empty() {
                progress.log(&format!("消息更新月份: {}", months.join(", ")));
            }
            changed_months.extend(months);
        }
        Err(error) => {
            return Err(GroupError::Failed(vec![format!(
                "群「{}」写消息 JSON 失败: {}",
                group.title, error
            )]));
        }
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
        if let Err(error) = job.archive.persist_index(&group_dir, &attachments) {
            progress.log(&format!("保存断点附件索引失败: {error}"));
        }
        return Err(GroupError::Cancelled);
    }

    // 附件索引按月落盘：attachments_index/{YYYYMM}.json
    match job.archive.persist_index(&group_dir, &attachments) {
        Ok(months) => changed_months.extend(months),
        Err(error) => {
            errors.push(format!(
                "群「{}」写附件索引 JSON 失败: {}",
                group.title, error
            ));
            return Err(GroupError::Failed(errors));
        }
    }
    let successful_attachments = attachments
        .iter()
        .filter(|attachment| attachment["status"] == "ok")
        .count();
    if reused_count > 0 {
        progress.log(&format!(
            "附件增量复用: {} 个已存在，跳过下载",
            reused_count
        ));
    }
    progress.log(&format!(
        "附件下载完成: 成功 {}/{}",
        successful_attachments, media_count
    ));

    // 4. 月度 HTML 生成（只重建「有变化 / HTML 缺失 / 数据比 HTML 新」的月份）
    progress.progress("准备生成聊天记录页面...".to_string());

    let plan = match job
        .archive
        .html_months(&group_dir, &group.title, &changed_months)
    {
        Ok(plan) => plan,
        Err(error) => {
            errors.push(format!(
                "群「{}」规划 HTML 生成失败: {}",
                group.title, error
            ));
            Vec::new()
        }
    };
    progress.log(&format!("本次需生成/更新 {} 个月份", plan.len()));

    let mut html_files_info: Vec<HtmlFileInfo> = Vec::new();
    for month in &plan {
        if job.cancel.load(Ordering::Relaxed) {
            break;
        }
        let year_month = &month.year_month;
        progress.progress(format!(
            "生成 {} 年 {} 月聊天记录...",
            &year_month[0..4.min(year_month.len())],
            if year_month.len() >= 6 {
                &year_month[4..6]
            } else {
                "?"
            }
        ));
        let month_attachment_count: usize = month
            .messages
            .iter()
            .map(|message| media::extract_media_ids(&message.content).len())
            .sum();
        let html_file_name = job
            .archive
            .html_filename(&group.title, year_month, &group_dir);
        let html_path = group_dir.join(&html_file_name);
        match viewer::generate_html(
            &month.messages,
            &group.title,
            &attachment_dir,
            &month.index_path,
            &job.self_name,
            &html_path,
            &job.cancel,
        ) {
            Ok(()) => {
                let file_size = html_path.metadata().map(|meta| meta.len()).unwrap_or(0);
                progress.log(&format!(
                    "已生成: {}（{} 条消息, {} 个附件, {:.1} MB）",
                    html_file_name,
                    month.messages.len(),
                    month_attachment_count,
                    file_size as f64 / 1_048_576.0
                ));
                html_files_info.push(HtmlFileInfo {
                    filename: html_file_name.clone(),
                    year_month: year_month.clone(),
                    message_count: month.messages.len(),
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
        (
            stamp[5..10].replace('-', ""),
            stamp[11..19].replace(':', ""),
        )
    } else {
        ("0000".to_string(), "000000".to_string())
    }
}

/// 原子写入 JSON（先写临时文件，成功后重命名）
pub(crate) fn write_json(path: &Path, value: &(impl Serialize + ?Sized)) -> Result<(), String> {
    let serialized = serde_json::to_string_pretty(value)
        .map_err(|error| format!("序列化 {} 失败: {}", path.display(), error))?;
    atomic_write(path, serialized.as_bytes())
}

/// 原子写入：先写临时文件，成功后重命名（自动创建父目录）
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("创建目录 {} 失败: {}", parent.display(), error))?;
        }
    }
    let temp_path = path.with_extension("tmp");
    let file = fs::File::create(&temp_path)
        .map_err(|error| format!("创建 {} 失败: {}", temp_path.display(), error))?;
    let mut writer = std::io::BufWriter::new(file);
    writer
        .write_all(bytes)
        .map_err(|error| format!("写入 {} 失败: {}", temp_path.display(), error))?;
    writer
        .flush()
        .map_err(|error| format!("刷新 {} 失败: {}", temp_path.display(), error))?;
    drop(writer);
    fs::rename(&temp_path, path)
        .map_err(|error| format!("重命名 {} 失败: {}", path.display(), error))
}

/// 序列化写入；内容与现有文件完全一致时跳过写入（返回 false，避免无意义刷新 mtime）
fn write_json_if_changed(path: &Path, value: &(impl Serialize + ?Sized)) -> Result<bool, String> {
    let serialized = serde_json::to_string_pretty(value)
        .map_err(|error| format!("序列化 {} 失败: {}", path.display(), error))?;
    if let Ok(existing) = fs::read_to_string(path) {
        if existing == serialized {
            return Ok(false);
        }
    }
    atomic_write(path, serialized.as_bytes())?;
    Ok(true)
}

// ===== 月度分文件布局：messages/{YYYYMM}.json + attachments_index/{YYYYMM}.json =====

/// 月度消息文件路径
fn month_messages_path(group_dir: &Path, year_month: &str) -> PathBuf {
    group_dir
        .join("messages")
        .join(format!("{year_month}.json"))
}

/// 月度附件索引文件路径
fn month_index_path(group_dir: &Path, year_month: &str) -> PathBuf {
    group_dir
        .join("attachments_index")
        .join(format!("{year_month}.json"))
}

/// 读取消息 JSON（文件不存在视为空）
fn read_messages(path: &Path) -> Result<Vec<Message>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)
        .map_err(|error| format!("读取 {} 失败: {}", path.display(), error))?;
    serde_json::from_str(&content)
        .map_err(|error| format!("解析 {} 失败: {}", path.display(), error))
}

/// 读取附件索引 JSON（文件不存在视为空）
fn read_records(path: &Path) -> Result<Vec<serde_json::Value>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)
        .map_err(|error| format!("读取 {} 失败: {}", path.display(), error))?;
    serde_json::from_str(&content)
        .map_err(|error| format!("解析 {} 失败: {}", path.display(), error))
}

/// 合并消息并按消息 ID 去重（已有记录优先保留），结果按时间排序
fn merge_messages(base: &mut Vec<Message>, incoming: impl IntoIterator<Item = Message>) {
    let mut seen: HashSet<String> = base
        .iter()
        .map(|message| message.open_message_id.clone())
        .collect();
    for message in incoming {
        if seen.insert(message.open_message_id.clone()) {
            base.push(message);
        }
    }
    base.sort_by(|left, right| {
        left.create_time
            .cmp(&right.create_time)
            .then_with(|| left.open_message_id.cmp(&right.open_message_id))
    });
}

/// 附件索引去重键：mediaId + file
fn record_key(record: &serde_json::Value) -> String {
    format!(
        "{}|{}",
        record
            .get("mediaId")
            .and_then(|value| value.as_str())
            .unwrap_or(""),
        record
            .get("file")
            .and_then(|value| value.as_str())
            .unwrap_or("")
    )
}

/// 合并附件索引并按 mediaId+file 去重：成功记录覆盖同键的失败记录
fn merge_records(
    base: &mut Vec<serde_json::Value>,
    incoming: impl IntoIterator<Item = serde_json::Value>,
) {
    let mut position_of: HashMap<String, usize> = base
        .iter()
        .enumerate()
        .map(|(index, record)| (record_key(record), index))
        .collect();
    for record in incoming {
        let key = record_key(&record);
        match position_of.get(&key) {
            Some(&position) => {
                let existing_ok = base[position]
                    .get("status")
                    .and_then(|value| value.as_str())
                    == Some("ok");
                let incoming_ok =
                    record.get("status").and_then(|value| value.as_str()) == Some("ok");
                if !existing_ok && incoming_ok {
                    base[position] = record;
                }
            }
            None => {
                position_of.insert(key, base.len());
                base.push(record);
            }
        }
    }
}

/// 附件记录归属月份：优先取 file 相对路径首段（YYYYMM），回退到 createTime
fn record_month(record: &serde_json::Value) -> String {
    if let Some(file) = record.get("file").and_then(|value| value.as_str()) {
        if let Some((head, _)) = file.split_once('/') {
            if head.len() == 6 && head.chars().all(|character| character.is_ascii_digit()) {
                return head.to_string();
            }
        }
    }
    record
        .get("createTime")
        .and_then(|value| value.as_str())
        .map(extract_year_month)
        .unwrap_or_else(|| "unknown".to_string())
}

/// 按月份分组（月份升序）
fn group_by_month<T>(
    items: impl IntoIterator<Item = T>,
    month_of: impl Fn(&T) -> String,
) -> BTreeMap<String, Vec<T>> {
    let mut grouped: BTreeMap<String, Vec<T>> = BTreeMap::new();
    for item in items {
        let month = month_of(&item);
        grouped.entry(month).or_default().push(item);
    }
    grouped
}

/// 左侧文件修改时间是否晚于右侧（任一不可读时返回 false）
fn is_newer(candidate: &Path, reference: &Path) -> bool {
    let modified_at = |path: &Path| path.metadata().ok().and_then(|meta| meta.modified().ok());
    match (modified_at(candidate), modified_at(reference)) {
        (Some(left), Some(right)) => left > right,
        _ => false,
    }
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
pub(crate) fn resolve_html_filename(
    group_title: &str,
    year_month: &str,
    group_dir: &Path,
) -> String {
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
        assert_eq!(dir, PathBuf::from("D:/exports").join("测试群_0115_103000"));
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

    fn sample_message(id: &str, time: &str) -> Message {
        Message {
            content: format!("内容-{id}"),
            create_time: time.into(),
            open_message_id: id.into(),
            sender: "张三".into(),
            sender_open_dingtalk_id: None,
            open_conv_thread_id: None,
        }
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dingtalk-exporter-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scheduled_messages_are_split_by_month_and_deduped() {
        let temp_dir = unique_dir("month-messages");
        let archive = ScheduledGroupArchive;

        // 第一次：1 月两条 → 只生成 202601 文件
        let changed = archive
            .persist_messages(
                &temp_dir,
                &[
                    sample_message("m1", "2026-01-01 10:00:00"),
                    sample_message("m2", "2026-01-15 10:00:00"),
                ],
                "2026-01-20 10:00:00",
            )
            .unwrap();
        assert_eq!(changed, vec!["202601".to_string()]);
        assert!(temp_dir.join("messages").join("202601.json").is_file());
        assert!(!temp_dir.join("messages").join("202602.json").exists());

        // 第二次：重复 m2（1 月内容不变）+ 新增 2 月 m3
        let changed = archive
            .persist_messages(
                &temp_dir,
                &[
                    sample_message("m2", "2026-01-15 10:00:00"),
                    sample_message("m3", "2026-02-01 10:00:00"),
                ],
                "2026-02-01 10:00:00",
            )
            .unwrap();
        // 内容未变化的月份不重写、不算变化
        assert_eq!(changed, vec!["202602".to_string()]);

        let january = read_messages(&temp_dir.join("messages").join("202601.json")).unwrap();
        let january_ids: Vec<&str> = january
            .iter()
            .map(|message| message.open_message_id.as_str())
            .collect();
        assert_eq!(january_ids, vec!["m1", "m2"]);
        assert_eq!(
            read_messages(&temp_dir.join("messages").join("202602.json"))
                .unwrap()
                .len(),
            1
        );

        // 第三次：完全相同的输入 → 无任何变化
        let changed = archive
            .persist_messages(
                &temp_dir,
                &[sample_message("m3", "2026-02-01 10:00:00")],
                "2026-02-02 10:00:00",
            )
            .unwrap();
        assert!(changed.is_empty());

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn scheduled_index_is_split_by_month_and_prefers_ok_status() {
        let temp_dir = unique_dir("month-index");
        let archive = ScheduledGroupArchive;

        // 1 月：同一附件先失败
        archive
            .persist_index(
                &temp_dir,
                &[serde_json::json!({ "mediaId": "media1", "file": "202601/a.jpg", "status": "fail" })],
            )
            .unwrap();

        // 重试成功（1 月内容变化）+ 2 月新增
        let changed = archive
            .persist_index(
                &temp_dir,
                &[
                    serde_json::json!({ "mediaId": "media1", "file": "202601/a.jpg", "status": "ok" }),
                    serde_json::json!({ "mediaId": "media2", "file": "202602/b.jpg", "status": "ok" }),
                ],
            )
            .unwrap();
        assert_eq!(changed, vec!["202601".to_string(), "202602".to_string()]);

        let january =
            read_records(&temp_dir.join("attachments_index").join("202601.json")).unwrap();
        assert_eq!(january.len(), 1);
        assert_eq!(january[0]["status"], "ok"); // 成功记录覆盖失败记录
        let february =
            read_records(&temp_dir.join("attachments_index").join("202602.json")).unwrap();
        assert_eq!(february.len(), 1);

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn legacy_batches_are_migrated_into_monthly_files_then_removed() {
        let temp_dir = unique_dir("legacy-migration");
        let batch_a = temp_dir.join("messages").join("20260101_023000");
        let batch_b = temp_dir.join("messages").join("20260201_023000");
        fs::create_dir_all(&batch_a).unwrap();
        fs::create_dir_all(&batch_b).unwrap();
        write_json(
            &batch_a.join("messages.json"),
            &vec![
                sample_message("m1", "2026-01-01 10:00:00"),
                sample_message("m2", "2026-01-15 10:00:00"),
            ],
        )
        .unwrap();
        write_json(
            &batch_a.join("attachments_index.json"),
            &vec![
                serde_json::json!({ "mediaId": "media1", "file": "202601/a.jpg", "status": "ok" }),
            ],
        )
        .unwrap();
        write_json(
            &batch_b.join("messages.json"),
            &vec![sample_message("m3", "2026-02-01 10:00:00")],
        )
        .unwrap();
        write_json(
            &batch_b.join("attachments_index.json"),
            &vec![
                serde_json::json!({ "mediaId": "media2", "file": "202602/b.jpg", "status": "ok" }),
            ],
        )
        .unwrap();
        // 旧布局的根合并索引
        write_json(
            &temp_dir.join("attachments_index.json"),
            &vec![serde_json::json!({ "mediaId": "legacy" })],
        )
        .unwrap();

        ScheduledGroupArchive.migrate_legacy(&temp_dir).unwrap();

        // 旧批次目录与根索引已清理
        assert!(!batch_a.exists());
        assert!(!batch_b.exists());
        assert!(!temp_dir.join("attachments_index.json").exists());
        // 月度文件已生成
        assert_eq!(
            read_messages(&temp_dir.join("messages").join("202601.json"))
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            read_messages(&temp_dir.join("messages").join("202602.json"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            read_records(&temp_dir.join("attachments_index").join("202601.json"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            read_records(&temp_dir.join("attachments_index").join("202602.json"))
                .unwrap()
                .len(),
            1
        );

        // 幂等：无旧批次时再跑一次不报错
        ScheduledGroupArchive.migrate_legacy(&temp_dir).unwrap();

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn html_plan_only_includes_changed_or_missing_months() {
        let temp_dir = unique_dir("html-plan");
        let archive = ScheduledGroupArchive;
        let group = "测试群";

        archive
            .persist_messages(
                &temp_dir,
                &[
                    sample_message("m1", "2026-01-01 10:00:00"),
                    sample_message("m3", "2026-02-01 10:00:00"),
                ],
                "2026-02-02 10:00:00",
            )
            .unwrap();
        archive
            .persist_index(
                &temp_dir,
                &[
                    serde_json::json!({ "mediaId": "media1", "file": "202601/a.jpg", "status": "ok" }),
                    serde_json::json!({ "mediaId": "media2", "file": "202602/b.jpg", "status": "ok" }),
                ],
            )
            .unwrap();

        // 两个月的 HTML 都不存在 → 都要生成
        let plan = archive.html_months(&temp_dir, group, &[]).unwrap();
        let months: Vec<&str> = plan.iter().map(|month| month.year_month.as_str()).collect();
        assert_eq!(months, vec!["202601", "202602"]);

        // 补上 HTML（时间晚于 JSON）后，无变化月份不再重建
        for year_month in ["202601", "202602"] {
            fs::write(
                temp_dir.join(format!("{group}-{year_month}.html")),
                b"<html></html>",
            )
            .unwrap();
        }
        assert!(archive
            .html_months(&temp_dir, group, &[])
            .unwrap()
            .is_empty());
        let plan = archive
            .html_months(&temp_dir, group, &["202602".to_string()])
            .unwrap();
        let months: Vec<&str> = plan.iter().map(|month| month.year_month.as_str()).collect();
        assert_eq!(months, vec!["202602"]);

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
        assert_eq!(
            resolve_html_filename("CON", "202601", &temp_dir),
            "_CON-202601.html"
        );
        assert_eq!(
            resolve_html_filename("... ", "202601", &temp_dir),
            "群聊-202601.html"
        );
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
        let scheduled = Trigger::Scheduled {
            schedule_id: "sch_1".into(),
        };
        assert_eq!(scheduled.trigger_type(), "scheduled");
        assert_eq!(scheduled.schedule_id(), Some("sch_1"));
    }
}
