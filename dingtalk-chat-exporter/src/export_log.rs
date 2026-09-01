// 导出日志管理 - 记录每次导出的详细信息，便于追溯和查看历史

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

/// 单个 HTML 文件的统计信息
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HtmlFileInfo {
    /// 文件名（如 "群聊-202601.html"）
    pub filename: String,
    /// 年月标识（如 "202601"）
    pub year_month: String,
    /// 该文件包含的消息数
    pub message_count: usize,
    /// 该文件包含的附件数
    pub attachment_count: usize,
    /// 文件大小（字节）
    pub file_size_bytes: u64,
}

/// 单次导出记录的完整日志条目
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ExportLogEntry {
    /// 唯一标识（时间戳生成）
    pub id: String,
    /// 群名称
    pub group_name: String,
    /// 群 ID（open_conversation_id）
    pub group_id: String,
    /// 导出目录名称（如 "悠租云运营群_0831_123456"）
    pub directory_name: String,
    /// 导出时间（ISO 格式）
    pub export_time: String,
    /// 用户指定的开始时间
    pub start_time: Option<String>,
    /// 用户指定的结束时间
    pub end_time: Option<String>,
    /// 实际消息最早时间
    pub actual_earliest: Option<String>,
    /// 实际消息最晚时间
    pub actual_latest: Option<String>,
    /// 消息总数
    pub message_count: usize,
    /// 附件总数
    pub attachment_total: usize,
    /// 附件下载成功数
    pub attachment_success: usize,
    /// 附件下载失败数
    pub attachment_failed: usize,
    /// 生成的 HTML 文件列表
    pub html_files: Vec<HtmlFileInfo>,
    /// 导出状态（success / partial / error）
    pub status: String,
    /// 错误信息（如有）
    pub error_message: Option<String>,
    /// 导出过程的日志行
    pub log_lines: Vec<String>,
}

/// 导出日志文件名
const EXPORT_LOGS_FILENAME: &str = "export_logs.json";

/// 向群目录追加一条导出日志。
/// 如果 export_logs.json 不存在，则创建空数组后追加；
/// 如果已存在，则读取、追加、写回。
/// 使用原子写入：先写入 .tmp 文件，再 rename 到目标路径，防止写入过程中断导致数据损坏。
pub fn append_export_log(group_dir: &Path, log: &ExportLogEntry) -> Result<(), String> {
    let log_path = group_dir.join(EXPORT_LOGS_FILENAME);
    let mut logs = if log_path.exists() {
        let content = fs::read_to_string(&log_path)
            .map_err(|error| format!("读取导出日志失败: {}", error))?;
        serde_json::from_str::<Vec<ExportLogEntry>>(&content)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    logs.push(log.clone());
    let json = serde_json::to_string_pretty(&logs)
        .map_err(|error| format!("序列化导出日志失败: {}", error))?;
    // 原子写入：先写 .tmp，再 rename
    let tmp_path = log_path.with_extension("json.tmp");
    fs::write(&tmp_path, &json)
        .map_err(|error| format!("写入导出日志临时文件失败: {}", error))?;
    fs::rename(&tmp_path, &log_path)
        .map_err(|error| format!("重命名导出日志临时文件失败: {}", error))
}

/// 读取指定群目录的所有导出日志
pub fn read_export_logs(group_dir: &Path) -> Result<Vec<ExportLogEntry>, String> {
    let log_path = group_dir.join(EXPORT_LOGS_FILENAME);
    if !log_path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&log_path)
        .map_err(|error| format!("读取导出日志失败: {}", error))?;
    let logs: Vec<ExportLogEntry> = serde_json::from_str(&content)
        .map_err(|error| format!("解析导出日志失败: {}", error))?;
    Ok(logs)
}

/// 扫描输出根目录下所有群目录，聚合所有 export_logs.json，
/// 按 export_time 倒序排序。
pub fn list_all_export_logs(output_root: &Path) -> Result<Vec<ExportLogEntry>, String> {
    let mut all_logs: Vec<ExportLogEntry> = Vec::new();

    if !output_root.exists() {
        return Ok(all_logs);
    }

    let entries = fs::read_dir(output_root)
        .map_err(|error| format!("读取输出根目录失败: {}", error))?;

    for entry in entries {
        let entry = entry.map_err(|error| format!("读取目录项失败: {}", error))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // 跳过隐藏目录（如 .partial 临时目录）
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') {
                continue;
            }
        }
        let log_path = path.join(EXPORT_LOGS_FILENAME);
        if !log_path.exists() {
            continue;
        }
        match read_export_logs(&path) {
            Ok(logs) => all_logs.extend(logs),
            Err(error) => {
                // 跳过损坏的日志文件，继续扫描其他群目录
                eprintln!("警告：读取 {} 失败: {}", log_path.display(), error);
            }
        }
    }

    // 按 export_time 倒序排序（最新的在前）
    all_logs.sort_by(|a, b| b.export_time.cmp(&a.export_time));
    Ok(all_logs)
}

/// 创建一条新的导出日志条目。
/// id 基于当前时间戳生成，export_time 使用传入的时间字符串。
#[cfg(test)]
pub fn create_log_entry(
    group_name: String,
    group_id: String,
    directory_name: String,
    export_time: String,
    start_time: Option<String>,
    end_time: Option<String>,
) -> ExportLogEntry {
    // 使用时间戳作为唯一 ID
    let id = generate_timestamp_id();
    ExportLogEntry {
        id,
        group_name,
        group_id,
        directory_name,
        export_time,
        start_time,
        end_time,
        actual_earliest: None,
        actual_latest: None,
        message_count: 0,
        attachment_total: 0,
        attachment_success: 0,
        attachment_failed: 0,
        html_files: Vec::new(),
        status: "running".to_string(),
        error_message: None,
        log_lines: Vec::new(),
    }
}

/// 基于当前系统时间生成唯一 ID
pub fn generate_timestamp_id() -> String {
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    // 使用毫秒时间戳作为 ID，格式如 "20260115_103000_123"
    // 转换为北京时间（UTC+8），与 dws::current_time_str() 保持一致
    let secs = duration.as_secs() + crate::date::CHINA_STANDARD_TIME_OFFSET_SECS;
    let millis = duration.subsec_millis();

    // 将秒数转换为可读时间格式
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    // 计算年月日（简化算法，基于 Unix 纪元）
    let total_days = secs / 86400;
    let (year, month, day) = crate::date::epoch_days_to_ymd(total_days as i64);

    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}_{:03}",
        year, month, day, hours, minutes, seconds, millis
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn create_log_entry_generates_unique_ids() {
        let log1 = create_log_entry(
            "测试群".to_string(),
            "cid001".to_string(),
            "测试群_0115_103000".to_string(),
            "2026-01-15 10:30:00".to_string(),
            None,
            None,
        );
        // 确保 ID 非空
        assert!(!log1.id.is_empty());
        assert_eq!(log1.group_name, "测试群");
        assert_eq!(log1.group_id, "cid001");
        assert_eq!(log1.status, "running");
    }

    #[test]
    fn append_and_read_export_logs_round_trip() {
        let temp_dir = std::env::temp_dir().join(format!(
            "dingtalk-export-log-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let log = create_log_entry(
            "测试群".to_string(),
            "cid001".to_string(),
            "测试群_0115_103000".to_string(),
            "2026-01-15 10:30:00".to_string(),
            Some("2026-01-01 00:00:00".to_string()),
            Some("2026-01-15 23:59:59".to_string()),
        );
        let mut log = log;
        log.message_count = 42;
        log.status = "success".to_string();
        log.html_files = vec![HtmlFileInfo {
            filename: "测试群-202601.html".to_string(),
            year_month: "202601".to_string(),
            message_count: 42,
            attachment_count: 3,
            file_size_bytes: 102400,
        }];

        // 追加日志
        append_export_log(&temp_dir, &log).unwrap();

        // 读取验证
        let logs = read_export_logs(&temp_dir).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].group_name, "测试群");
        assert_eq!(logs[0].message_count, 42);
        assert_eq!(logs[0].html_files.len(), 1);

        // 再追加一条
        let log2 = create_log_entry(
            "测试群".to_string(),
            "cid001".to_string(),
            "测试群_0201_140000".to_string(),
            "2026-02-01 14:00:00".to_string(),
            None,
            None,
        );
        append_export_log(&temp_dir, &log2).unwrap();

        let logs = read_export_logs(&temp_dir).unwrap();
        assert_eq!(logs.len(), 2);

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn read_export_logs_returns_empty_for_missing_file() {
        let temp_dir = std::env::temp_dir().join(format!(
            "dingtalk-export-log-empty-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let logs = read_export_logs(&temp_dir).unwrap();
        assert!(logs.is_empty());

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn list_all_export_logs_aggregates_and_sorts() {
        let temp_dir = std::env::temp_dir().join(format!(
            "dingtalk-export-log-list-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_dir);

        // 创建两个群目录
        let group_a = temp_dir.join("group_a");
        let group_b = temp_dir.join("group_b");
        fs::create_dir_all(&group_a).unwrap();
        fs::create_dir_all(&group_b).unwrap();

        // 群 A 有两条日志
        let mut log_a1 = create_log_entry(
            "群A".to_string(),
            "cid_a".to_string(),
            "群A_0115_100000".to_string(),
            "2026-01-15 10:00:00".to_string(),
            None,
            None,
        );
        log_a1.status = "success".to_string();
        append_export_log(&group_a, &log_a1).unwrap();

        let mut log_a2 = create_log_entry(
            "群A".to_string(),
            "cid_a".to_string(),
            "群A_0201_090000".to_string(),
            "2026-02-01 09:00:00".to_string(),
            None,
            None,
        );
        log_a2.status = "success".to_string();
        append_export_log(&group_a, &log_a2).unwrap();

        // 群 B 有一条日志（时间最早）
        let mut log_b1 = create_log_entry(
            "群B".to_string(),
            "cid_b".to_string(),
            "群B_1201_080000".to_string(),
            "2025-12-01 08:00:00".to_string(),
            None,
            None,
        );
        log_b1.status = "success".to_string();
        append_export_log(&group_b, &log_b1).unwrap();

        // 聚合查询
        let all_logs = list_all_export_logs(&temp_dir).unwrap();
        assert_eq!(all_logs.len(), 3);
        // 应按 export_time 倒序：最新的在前
        assert_eq!(all_logs[0].group_name, "群A");
        assert!(all_logs[0].export_time >= all_logs[1].export_time);
        assert!(all_logs[1].export_time >= all_logs[2].export_time);

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn list_all_export_logs_skips_hidden_directories() {
        let temp_dir = std::env::temp_dir().join(format!(
            "dingtalk-export-log-hidden-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_dir);

        // 正常群目录
        let group_dir = temp_dir.join("group_normal");
        fs::create_dir_all(&group_dir).unwrap();
        let mut log = create_log_entry(
            "正常群".to_string(),
            "cid_normal".to_string(),
            "正常群_0115_100000".to_string(),
            "2026-01-15 10:00:00".to_string(),
            None,
            None,
        );
        log.status = "success".to_string();
        append_export_log(&group_dir, &log).unwrap();

        // 隐藏目录（模拟 .partial）
        let hidden_dir = temp_dir.join(".group.partial");
        fs::create_dir_all(&hidden_dir).unwrap();
        let mut hidden_log = create_log_entry(
            "隐藏群".to_string(),
            "cid_hidden".to_string(),
            "隐藏群_0115_100000".to_string(),
            "2026-01-15 10:00:00".to_string(),
            None,
            None,
        );
        hidden_log.status = "running".to_string();
        append_export_log(&hidden_dir, &hidden_log).unwrap();

        let all_logs = list_all_export_logs(&temp_dir).unwrap();
        // 只应包含正常群的日志
        assert_eq!(all_logs.len(), 1);
        assert_eq!(all_logs[0].group_name, "正常群");

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn export_log_entry_serializes_correctly() {
        let mut entry = create_log_entry(
            "测试群".to_string(),
            "cid001".to_string(),
            "测试群_0115_103000".to_string(),
            "2026-01-15 10:30:00".to_string(),
            Some("2026-01-01 00:00:00".to_string()),
            None,
        );
        entry.message_count = 100;
        entry.attachment_total = 10;
        entry.attachment_success = 8;
        entry.attachment_failed = 2;
        entry.status = "partial".to_string();
        entry.error_message = Some("部分附件下载失败".to_string());
        entry.html_files = vec![
            HtmlFileInfo {
                filename: "测试群-202601.html".to_string(),
                year_month: "202601".to_string(),
                message_count: 60,
                attachment_count: 6,
                file_size_bytes: 51200,
            },
            HtmlFileInfo {
                filename: "测试群-202602.html".to_string(),
                year_month: "202602".to_string(),
                message_count: 40,
                attachment_count: 4,
                file_size_bytes: 30720,
            },
        ];

        let json = serde_json::to_string_pretty(&entry).unwrap();
        // 验证 camelCase 字段名
        assert!(json.contains("\"groupName\""));
        assert!(json.contains("\"groupId\""));
        assert!(json.contains("\"exportTime\""));
        assert!(json.contains("\"startTime\""));
        assert!(json.contains("\"endTime\""));
        assert!(json.contains("\"actualEarliest\""));
        assert!(json.contains("\"actualLatest\""));
        assert!(json.contains("\"messageCount\""));
        assert!(json.contains("\"attachmentTotal\""));
        assert!(json.contains("\"attachmentSuccess\""));
        assert!(json.contains("\"attachmentFailed\""));
        assert!(json.contains("\"htmlFiles\""));
        assert!(json.contains("\"errorMessage\""));
        assert!(json.contains("\"logLines\""));

        // 反序列化验证
        let deserialized: ExportLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.group_name, "测试群");
        assert_eq!(deserialized.message_count, 100);
        assert_eq!(deserialized.html_files.len(), 2);
    }
}
