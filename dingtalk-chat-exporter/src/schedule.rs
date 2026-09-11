// 定时导出任务模型与持久化
//
// 职责：
// - Schedule 数据结构（界面配置 + cron + 增量水位线 + 运行历史）
// - schedules.json 原子读写（损坏时备份 .corrupt 并以空表启动）
// - 界面配置 → cron 表达式翻译
// - 保存前校验（cron 合法性、同群冲突、时间格式）
// - 增量区间计算（首次 = max(最早聊天日期, 群创建时间)，后续 = lastSuccessAt）

use crate::cron::{parse_hhmm, Cron};
use crate::date::{is_valid_datetime, parse_dws_datetime};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 运行历史最多保留条数（每任务）
const MAX_RUNS_PER_SCHEDULE: usize = 50;

/// 调度周期单位
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleUnit {
    Minute,
    Hour,
    Day,
    Week,
    Month,
}

/// 调度配置（界面简单模式 + cron 高级模式）
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleConfig {
    /// simple = 由界面字段生成 cron；cron = 用户直接填 cron
    pub mode: String,
    /// 周期单位（mode=simple 时必填）
    pub unit: Option<ScheduleUnit>,
    /// 间隔（每 N 个单位），默认 1
    #[serde(default = "default_interval")]
    pub interval: u32,
    /// unit=week 时的星期集合（0-6，0=周日）
    #[serde(default)]
    pub weekdays: Vec<u32>,
    /// unit=month 时的日期集合（1-31）
    #[serde(default)]
    pub month_days: Vec<u32>,
    /// day/week/month 模式的触发时刻 "HH:mm"
    pub at_time: Option<String>,
    /// 调度生效开始时间 "yyyy-MM-dd HH:mm:ss"（北京时间）
    pub start_time: String,
    /// cron 表达式（simple 模式自动生成；cron 模式用户填写）
    pub cron: String,
}

fn default_interval() -> u32 {
    1
}

impl ScheduleConfig {
    /// 由界面配置生成 cron 表达式（mode=simple）
    pub fn build_cron(&self) -> Result<String, String> {
        if self.mode == "cron" {
            let expr = self.cron.trim().to_string();
            if expr.is_empty() {
                return Err("cron 表达式不能为空".into());
            }
            return Ok(expr);
        }

        let unit = self
            .unit
            .clone()
            .ok_or_else(|| "请选择调度周期单位".to_string())?;
        let interval = self.interval;
        if interval == 0 {
            return Err("调度间隔不能为 0".into());
        }

        match unit {
            ScheduleUnit::Minute => {
                if interval > 59 {
                    return Err("分钟间隔不能超过 59".into());
                }
                Ok(format!("*/{interval} * * * *"))
            }
            ScheduleUnit::Hour => {
                if interval > 23 {
                    return Err("小时间隔不能超过 23".into());
                }
                Ok(format!("0 */{interval} * * *"))
            }
            ScheduleUnit::Day => {
                let (hour, minute) = self
                    .at_time
                    .as_deref()
                    .and_then(parse_hhmm)
                    .ok_or_else(|| "请选择每天的触发时刻（HH:mm）".to_string())?;
                if interval > 31 {
                    return Err("天间隔不能超过 31".into());
                }
                Ok(format!("{minute} {hour} */{interval} * *"))
            }
            ScheduleUnit::Week => {
                let (hour, minute) = self
                    .at_time
                    .as_deref()
                    .and_then(parse_hhmm)
                    .ok_or_else(|| "请选择每周的触发时刻（HH:mm）".to_string())?;
                if self.weekdays.is_empty() {
                    return Err("请至少选择一个星期几".into());
                }
                if self.weekdays.iter().any(|day| *day > 6) {
                    return Err("星期取值必须在 0-6 之间（0=周日）".into());
                }
                let mut days = self.weekdays.clone();
                days.sort_unstable();
                days.dedup();
                let day_list = days
                    .iter()
                    .map(|day| day.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                // cron 标准周字段不支持 */N，interval 语义在周模式下由 day_list 表达
                Ok(format!("{minute} {hour} * * {day_list}"))
            }
            ScheduleUnit::Month => {
                let (hour, minute) = self
                    .at_time
                    .as_deref()
                    .and_then(parse_hhmm)
                    .ok_or_else(|| "请选择每月的触发时刻（HH:mm）".to_string())?;
                if self.month_days.is_empty() {
                    return Err("请至少选择一个每月第几天".into());
                }
                if self.month_days.iter().any(|day| *day < 1 || *day > 31) {
                    return Err("每月日期取值必须在 1-31 之间".into());
                }
                let mut days = self.month_days.clone();
                days.sort_unstable();
                days.dedup();
                let day_list = days
                    .iter()
                    .map(|day| day.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                Ok(format!("{minute} {hour} {day_list} * *"))
            }
        }
    }

    /// 校验配置：生成/解析 cron，检查开始时间格式
    pub fn validate(&self) -> Result<Cron, String> {
        if !is_valid_datetime(&self.start_time) {
            return Err("调度开始时间格式无效，预期 yyyy-MM-dd HH:mm:ss".into());
        }
        let expression = self.build_cron()?;
        let cron = Cron::parse(&expression)?;
        // 试算：确保 cron 在开始时间之后确实有解（如 2 月 30 日会返回 None）
        cron.next_after(&self.start_time)
            .ok_or_else(|| format!("cron 表达式无有效触发时间: {expression}"))?;
        Ok(cron)
    }

    /// 自然语言描述（界面展示用）
    pub fn describe(&self) -> String {
        let interval = self.interval.max(1);
        match self.unit.clone() {
            Some(ScheduleUnit::Minute) => format!("每 {interval} 分钟"),
            Some(ScheduleUnit::Hour) => format!("每 {interval} 小时"),
            Some(ScheduleUnit::Day) => format!(
                "每 {interval} 天 {}",
                self.at_time.clone().unwrap_or_else(|| "??:??".into())
            ),
            Some(ScheduleUnit::Week) => {
                const NAMES: [&str; 7] = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
                let days = self
                    .weekdays
                    .iter()
                    .filter(|day| **day <= 6)
                    .map(|day| NAMES[*day as usize])
                    .collect::<Vec<_>>()
                    .join("、");
                format!(
                    "每周 {} {}",
                    if days.is_empty() { "?".into() } else { days },
                    self.at_time.clone().unwrap_or_else(|| "??:??".into())
                )
            }
            Some(ScheduleUnit::Month) => {
                let days = self
                    .month_days
                    .iter()
                    .map(|day| format!("{day} 日"))
                    .collect::<Vec<_>>()
                    .join("、");
                format!(
                    "每月 {} {}",
                    if days.is_empty() { "?".into() } else { days },
                    self.at_time.clone().unwrap_or_else(|| "??:??".into())
                )
            }
            None => format!("cron: {}", self.cron),
        }
    }
}

/// 任务关联的群（含创建时间，用于首次拉取起点计算）
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleGroup {
    pub title: String,
    pub open_conversation_id: String,
    /// 群创建时间（dws 返回，"yyyy-MM-dd HH:mm:ss"；可能为空）
    #[serde(default)]
    pub create_at: Option<String>,
}

/// 单次运行记录（任务级运行日志）
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRun {
    pub run_id: String,
    pub started_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
    /// running（执行中）| success | partial | error | cancelled | skipped
    pub status: String,
    /// 本次拉取区间起点
    #[serde(default)]
    pub range_start: Option<String>,
    /// 本次拉取区间终点
    #[serde(default)]
    pub range_end: Option<String>,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default)]
    pub attachment_success: usize,
    #[serde(default)]
    pub attachment_failed: usize,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub log_lines: Vec<String>,
}

/// 定时导出任务
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub groups: Vec<ScheduleGroup>,
    /// 输出根目录（None 表示用 settings.defaultOutputDir）
    pub output_root: Option<String>,
    /// 最早聊天日志日期（首次拉取起点下限）
    #[serde(default)]
    pub earliest_chat_date: Option<String>,
    pub schedule: ScheduleConfig,
    /// 增量水位线：上次成功拉取的截止时间
    #[serde(default)]
    pub last_success_at: Option<String>,
    #[serde(default)]
    pub last_run_at: Option<String>,
    #[serde(default)]
    pub next_run_at: Option<String>,
    #[serde(default)]
    pub run_count: u32,
    /// 运行历史（最多保留 MAX_RUNS_PER_SCHEDULE 条，最新在前）
    #[serde(default)]
    pub runs: Vec<ScheduleRun>,
}

impl Schedule {
    /// 计算本次拉取的起点：
    /// - 已有水位线 → lastSuccessAt
    /// - 首次 → max(earliestChatDate, 群创建时间) 中较晚者；都缺失则为 None（全量）
    pub fn resolve_start_time(&self) -> Option<String> {
        if let Some(last) = self.last_success_at.clone() {
            return Some(last);
        }
        let mut candidates: Vec<String> = Vec::new();
        if let Some(earliest) = self
            .earliest_chat_date
            .as_deref()
            .filter(|value| is_valid_datetime(value))
        {
            candidates.push(earliest.to_string());
        }
        for group in &self.groups {
            if let Some(create_at) = group
                .create_at
                .as_deref()
                .filter(|value| is_valid_datetime(value))
            {
                candidates.push(create_at.to_string());
            }
        }
        // 字符串字典序等价于时间序（格式固定 yyyy-MM-dd HH:mm:ss）
        candidates.into_iter().max()
    }

    /// 追加运行记录并截断到上限（最新在前）
    pub fn push_run(&mut self, run: ScheduleRun) {
        self.runs.insert(0, run);
        self.runs.truncate(MAX_RUNS_PER_SCHEDULE);
        self.run_count = self.run_count.saturating_add(1);
    }

    /// 按 run_id 原地更新运行记录（用于 running → 终态）
    pub fn update_run(&mut self, run_id: &str, updater: impl FnOnce(&mut ScheduleRun)) -> bool {
        if let Some(run) = self.runs.iter_mut().find(|r| r.run_id == run_id) {
            updater(run);
            true
        } else {
            false
        }
    }

    /// 按状态查找并更新运行记录（用于终止 running 记录）
    pub fn update_run_by_status(
        &mut self,
        status: &str,
        updater: impl FnOnce(&mut ScheduleRun),
    ) -> bool {
        if let Some(run) = self.runs.iter_mut().find(|r| r.status == status) {
            updater(run);
            true
        } else {
            false
        }
    }
}

/// 调度任务集合（持久化容器）
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleStore {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub schedules: Vec<Schedule>,
}

impl Default for ScheduleStore {
    fn default() -> Self {
        ScheduleStore {
            version: default_version(),
            schedules: Vec::new(),
        }
    }
}

fn default_version() -> u32 {
    1
}

impl ScheduleStore {
    /// 按 id 查找
    pub fn find(&self, id: &str) -> Option<&Schedule> {
        self.schedules.iter().find(|item| item.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Schedule> {
        self.schedules.iter_mut().find(|item| item.id == id)
    }

    /// 同群冲突检测（决策3：保存时显式拒绝）
    /// 返回冲突描述：(冲突任务名, 冲突群名)
    pub fn find_group_conflict(&self, candidate: &Schedule) -> Option<(String, String)> {
        let candidate_ids: Vec<&str> = candidate
            .groups
            .iter()
            .map(|group| group.open_conversation_id.as_str())
            .collect();
        for existing in &self.schedules {
            if existing.id == candidate.id {
                continue; // 编辑自身不算冲突
            }
            for group in &existing.groups {
                if candidate_ids.contains(&group.open_conversation_id.as_str()) {
                    return Some((existing.name.clone(), group.title.clone()));
                }
            }
        }
        None
    }

    /// 校验任务（cron + 时间 + 同群冲突），保存前调用
    pub fn validate(&self, candidate: &Schedule) -> Result<(), String> {
        if candidate.name.trim().is_empty() {
            return Err("任务名称不能为空".into());
        }
        if candidate.groups.is_empty() {
            return Err("请至少选择一个群".into());
        }
        if candidate
            .groups
            .iter()
            .any(|group| group.open_conversation_id.trim().is_empty())
        {
            return Err("群会话 ID 不能为空".into());
        }
        // 同一任务内也不允许重复选同一群
        let mut ids: Vec<&str> = candidate
            .groups
            .iter()
            .map(|group| group.open_conversation_id.as_str())
            .collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != total {
            return Err("同一任务内不能重复选择同一个群".into());
        }
        if let Some(earliest) = candidate.earliest_chat_date.as_deref() {
            if !earliest.trim().is_empty() && !is_valid_datetime(earliest) {
                return Err("最早聊天日志日期格式无效，预期 yyyy-MM-dd HH:mm:ss".into());
            }
        }
        candidate.schedule.validate()?;
        if let Some((task_name, group_title)) = self.find_group_conflict(candidate) {
            return Err(format!(
                "群「{group_title}」已被定时任务「{task_name}」使用，同一群不能同时被多个定时任务导出"
            ));
        }
        Ok(())
    }

    /// 插入或替换任务（按 id）
    pub fn upsert(&mut self, schedule: Schedule) {
        if let Some(existing) = self
            .schedules
            .iter_mut()
            .find(|item| item.id == schedule.id)
        {
            *existing = schedule;
        } else {
            self.schedules.push(schedule);
        }
    }

    /// 删除任务，返回是否删除成功
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.schedules.len();
        self.schedules.retain(|item| item.id != id);
        self.schedules.len() != before
    }
}

const SCHEDULES_FILENAME: &str = "schedules.json";
const APP_DIR_NAME: &str = "dingtalk-chat-exporter";

/// 配置目录（与 settings.rs 同目录，逻辑独立避免循环依赖）
fn config_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var("USERPROFILE")
                    .ok()
                    .map(|path| PathBuf::from(path).join("AppData").join("Roaming"))
                    .unwrap_or_else(|| PathBuf::from("."))
            })
    } else if cfg!(target_os = "macos") {
        std::env::var("HOME")
            .ok()
            .map(|path| {
                PathBuf::from(path)
                    .join("Library")
                    .join("Application Support")
            })
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        std::env::var("HOME")
            .ok()
            .map(|path| PathBuf::from(path).join(".config"))
            .unwrap_or_else(|| PathBuf::from("."))
    }
    .join(APP_DIR_NAME)
}

/// schedules.json 路径
pub fn schedules_file_path() -> PathBuf {
    config_dir().join(SCHEDULES_FILENAME)
}

/// 加载任务表。
/// - 文件不存在 → 空表（不创建文件）
/// - 文件损坏 → 备份为 .corrupt 并返回空表（不阻断应用启动）
pub fn load_store() -> Result<ScheduleStore, String> {
    let path = schedules_file_path();
    load_store_from(&path)
}

/// 可测试版本：从指定路径加载
fn load_store_from(path: &Path) -> Result<ScheduleStore, String> {
    if !path.exists() {
        return Ok(ScheduleStore::default());
    }
    let content =
        fs::read_to_string(path).map_err(|error| format!("读取定时任务文件失败: {error}"))?;
    match serde_json::from_str::<ScheduleStore>(&content) {
        Ok(store) => Ok(store),
        Err(error) => {
            // 损坏：备份后以空表启动，避免应用无法使用
            let backup = path.with_extension("json.corrupt");
            let _ = fs::rename(path, &backup);
            eprintln!(
                "警告: schedules.json 解析失败（{error}），已备份为 {} 并以空任务表启动",
                backup.display()
            );
            Ok(ScheduleStore::default())
        }
    }
}

/// 保存任务表（原子写入：.tmp → rename）
pub fn save_store(store: &ScheduleStore) -> Result<(), String> {
    let path = schedules_file_path();
    save_store_to(store, &path)
}

fn save_store_to(store: &ScheduleStore, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|error| format!("创建配置目录失败: {error}"))?;
        }
    }
    let json = serde_json::to_string_pretty(store)
        .map_err(|error| format!("序列化定时任务失败: {error}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|error| format!("写入定时任务临时文件失败: {error}"))?;
    fs::rename(&tmp, path).map_err(|error| format!("重命名定时任务文件失败: {error}"))
}

/// 生成任务 ID（基于北京时间时间戳）
pub fn generate_schedule_id(now: &str) -> String {
    let compact: String = now.chars().filter(|c| c.is_ascii_digit()).collect();
    let stamp = if compact.len() >= 14 {
        compact[..14].to_string()
    } else {
        crate::export_log::generate_timestamp_id()
    };
    format!("sch_{stamp}")
}

/// 计算下次运行时间：从 max(调度开始时间, 基准时间) 之后的第一个触发点
pub fn compute_next_run(config: &ScheduleConfig, after: &str) -> Option<String> {
    let cron = config.validate().ok()?;
    // 首次运行时 next_run 不能早于调度开始时间
    let base = if config.start_time.as_str() > after {
        // 从开始时间前一分钟起算，使开始时间本身可成为触发点
        previous_minute(&config.start_time)?
    } else {
        after.to_string()
    };
    cron.next_after(&base)
}

/// 求前一分钟（用于让 startTime 本身可被命中）
fn previous_minute(datetime: &str) -> Option<String> {
    let (year, month, day, hour, minute, _second) = parse_dws_datetime(datetime)?;
    // 转成纪元分钟再减 1，避免手动处理借位
    let epoch_day = crate::date::ymd_to_epoch_days(year, month, day);
    let total_minutes = epoch_day * 1440 + i64::from(hour) * 60 + i64::from(minute) - 1;
    let days = total_minutes.div_euclid(1440);
    let rem = total_minutes.rem_euclid(1440);
    let (new_year, new_month, new_day) = crate::date::epoch_days_to_ymd(days);
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:00",
        new_year,
        new_month,
        new_day,
        rem / 60,
        rem % 60
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_config(unit: ScheduleUnit, interval: u32) -> ScheduleConfig {
        ScheduleConfig {
            mode: "simple".into(),
            unit: Some(unit),
            interval,
            weekdays: Vec::new(),
            month_days: Vec::new(),
            at_time: Some("02:30".into()),
            start_time: "2026-09-10 00:00:00".into(),
            cron: String::new(),
        }
    }

    #[test]
    fn ui_config_translates_to_cron() {
        let mut config = simple_config(ScheduleUnit::Minute, 5);
        assert_eq!(config.build_cron().unwrap(), "*/5 * * * *");

        config = simple_config(ScheduleUnit::Hour, 6);
        assert_eq!(config.build_cron().unwrap(), "0 */6 * * *");

        config = simple_config(ScheduleUnit::Day, 1);
        assert_eq!(config.build_cron().unwrap(), "30 2 */1 * *");

        config = simple_config(ScheduleUnit::Week, 1);
        config.weekdays = vec![1, 3, 5];
        assert_eq!(config.build_cron().unwrap(), "30 2 * * 1,3,5");

        config = simple_config(ScheduleUnit::Month, 1);
        config.month_days = vec![1, 15];
        assert_eq!(config.build_cron().unwrap(), "30 2 1,15 * *");
    }

    #[test]
    fn week_and_month_dedupe_and_sort() {
        let mut config = simple_config(ScheduleUnit::Week, 1);
        config.weekdays = vec![5, 1, 1, 3];
        assert_eq!(config.build_cron().unwrap(), "30 2 * * 1,3,5");

        let mut config = simple_config(ScheduleUnit::Month, 1);
        config.month_days = vec![15, 1, 15];
        assert_eq!(config.build_cron().unwrap(), "30 2 1,15 * *");
    }

    #[test]
    fn invalid_ui_config_is_rejected() {
        let mut config = simple_config(ScheduleUnit::Minute, 0);
        assert!(config.build_cron().is_err()); // 间隔 0

        config = simple_config(ScheduleUnit::Minute, 60);
        assert!(config.build_cron().is_err()); // 分钟 > 59

        config = simple_config(ScheduleUnit::Day, 1);
        config.at_time = None;
        assert!(config.build_cron().is_err()); // 缺时刻

        config = simple_config(ScheduleUnit::Week, 1);
        config.weekdays = vec![7];
        assert!(config.build_cron().is_err()); // 周越界

        config = simple_config(ScheduleUnit::Month, 1);
        config.month_days = vec![32];
        assert!(config.build_cron().is_err()); // 日越界

        config = simple_config(ScheduleUnit::Month, 1);
        config.month_days = Vec::new();
        assert!(config.build_cron().is_err()); // 空集合
    }

    #[test]
    fn cron_mode_uses_user_expression() {
        let config = ScheduleConfig {
            mode: "cron".into(),
            unit: None,
            interval: 1,
            weekdays: Vec::new(),
            month_days: Vec::new(),
            at_time: None,
            start_time: "2026-09-10 00:00:00".into(),
            cron: "15 3 * * 1-5".into(),
        };
        assert_eq!(config.build_cron().unwrap(), "15 3 * * 1-5");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn impossible_cron_is_rejected_on_validate() {
        let config = ScheduleConfig {
            mode: "cron".into(),
            unit: None,
            interval: 1,
            weekdays: Vec::new(),
            month_days: Vec::new(),
            at_time: None,
            start_time: "2026-09-10 00:00:00".into(),
            cron: "0 0 30 2 *".into(), // 2 月 30 日
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_start_time_is_rejected() {
        let mut config = simple_config(ScheduleUnit::Day, 1);
        config.start_time = "bad".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn describe_is_human_readable() {
        let mut config = simple_config(ScheduleUnit::Day, 2);
        assert_eq!(config.describe(), "每 2 天 02:30");

        config = simple_config(ScheduleUnit::Week, 1);
        config.weekdays = vec![1, 3];
        assert_eq!(config.describe(), "每周 周一、周三 02:30");

        config = simple_config(ScheduleUnit::Month, 1);
        config.month_days = vec![1];
        assert_eq!(config.describe(), "每月 1 日 02:30");

        config = simple_config(ScheduleUnit::Minute, 10);
        assert_eq!(config.describe(), "每 10 分钟");
    }

    fn test_schedule(id: &str, name: &str, group_id: &str) -> Schedule {
        Schedule {
            id: id.into(),
            name: name.into(),
            enabled: true,
            groups: vec![ScheduleGroup {
                title: format!("群-{group_id}"),
                open_conversation_id: group_id.into(),
                create_at: None,
            }],
            output_root: None,
            earliest_chat_date: None,
            schedule: simple_config(ScheduleUnit::Day, 1),
            last_success_at: None,
            last_run_at: None,
            next_run_at: None,
            run_count: 0,
            runs: Vec::new(),
        }
    }

    #[test]
    fn conflict_detection_rejects_same_group() {
        let mut store = ScheduleStore::default();
        store
            .schedules
            .push(test_schedule("sch_1", "任务A", "cid_x"));

        // 新任务引用同一群 → 冲突
        let candidate = test_schedule("sch_2", "任务B", "cid_x");
        let conflict = store.find_group_conflict(&candidate);
        assert_eq!(conflict, Some(("任务A".into(), "群-cid_x".into())));
        assert!(store.validate(&candidate).is_err());

        // 不同群 → 无冲突
        let other = test_schedule("sch_3", "任务C", "cid_y");
        assert!(store.find_group_conflict(&other).is_none());
        assert!(store.validate(&other).is_ok());

        // 编辑自身 → 不算冲突
        let mut self_edit = test_schedule("sch_1", "任务A改名", "cid_x");
        self_edit.name = "任务A改名".into();
        assert!(store.find_group_conflict(&self_edit).is_none());
        assert!(store.validate(&self_edit).is_ok());
    }

    #[test]
    fn duplicate_group_in_same_task_is_rejected() {
        let mut candidate = test_schedule("sch_1", "任务A", "cid_x");
        candidate.groups.push(ScheduleGroup {
            title: "重复群".into(),
            open_conversation_id: "cid_x".into(),
            create_at: None,
        });
        assert!(ScheduleStore::default().validate(&candidate).is_err());
    }

    #[test]
    fn empty_name_or_groups_rejected() {
        let candidate = test_schedule("sch_1", "", "cid_x");
        assert!(ScheduleStore::default().validate(&candidate).is_err());

        let mut candidate = test_schedule("sch_1", "任务A", "cid_x");
        candidate.groups.clear();
        assert!(ScheduleStore::default().validate(&candidate).is_err());
    }

    #[test]
    fn resolve_start_time_prefers_watermark() {
        let mut schedule = test_schedule("sch_1", "任务A", "cid_x");
        schedule.last_success_at = Some("2026-09-09 02:30:00".into());
        schedule.earliest_chat_date = Some("2026-01-01 00:00:00".into());
        // 已有水位线 → 直接用水位线
        assert_eq!(
            schedule.resolve_start_time(),
            Some("2026-09-09 02:30:00".into())
        );
    }

    #[test]
    fn first_run_takes_later_of_earliest_and_group_create() {
        let mut schedule = test_schedule("sch_1", "任务A", "cid_x");
        // 仅 earliestChatDate
        schedule.earliest_chat_date = Some("2026-01-01 00:00:00".into());
        assert_eq!(
            schedule.resolve_start_time(),
            Some("2026-01-01 00:00:00".into())
        );

        // 群创建时间更晚 → 取群创建时间
        schedule.groups[0].create_at = Some("2026-03-01 09:00:00".into());
        assert_eq!(
            schedule.resolve_start_time(),
            Some("2026-03-01 09:00:00".into())
        );

        // earliestChatDate 更晚 → 取 earliestChatDate
        schedule.earliest_chat_date = Some("2026-05-01 00:00:00".into());
        assert_eq!(
            schedule.resolve_start_time(),
            Some("2026-05-01 00:00:00".into())
        );

        // 都缺失 → None（全量）
        schedule.earliest_chat_date = None;
        schedule.groups[0].create_at = None;
        assert_eq!(schedule.resolve_start_time(), None);

        // 非法格式被忽略
        schedule.earliest_chat_date = Some("bad".into());
        assert_eq!(schedule.resolve_start_time(), None);
    }

    #[test]
    fn runs_are_capped_and_newest_first() {
        let mut schedule = test_schedule("sch_1", "任务A", "cid_x");
        for index in 0..60 {
            schedule.push_run(ScheduleRun {
                run_id: format!("run_{index}"),
                started_at: format!("2026-09-09 {:02}:00:00", index % 24),
                finished_at: None,
                status: "success".into(),
                range_start: None,
                range_end: None,
                message_count: index as usize,
                attachment_success: 0,
                attachment_failed: 0,
                error: None,
                log_lines: Vec::new(),
            });
        }
        assert_eq!(schedule.runs.len(), MAX_RUNS_PER_SCHEDULE);
        assert_eq!(schedule.run_count, 60);
        // 最新在前
        assert_eq!(schedule.runs[0].run_id, "run_59");
        assert_eq!(schedule.runs[0].message_count, 59);
    }

    #[test]
    fn store_upsert_and_remove() {
        let mut store = ScheduleStore::default();
        store.upsert(test_schedule("sch_1", "任务A", "cid_x"));
        assert_eq!(store.schedules.len(), 1);

        let mut updated = test_schedule("sch_1", "任务A改名", "cid_x");
        updated.enabled = false;
        store.upsert(updated);
        assert_eq!(store.schedules.len(), 1); // 替换而非新增
        assert_eq!(store.find("sch_1").unwrap().name, "任务A改名");
        assert!(!store.find("sch_1").unwrap().enabled);

        assert!(store.remove("sch_1"));
        assert!(!store.remove("sch_1")); // 已删除
        assert!(store.schedules.is_empty());
    }

    #[test]
    fn store_round_trip_with_temp_file() {
        let temp_dir =
            std::env::temp_dir().join(format!("dingtalk-schedule-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("schedules.json");

        let mut store = ScheduleStore::default();
        let mut schedule = test_schedule("sch_1", "任务A", "cid_x");
        schedule.last_success_at = Some("2026-09-09 02:30:00".into());
        schedule.push_run(ScheduleRun {
            run_id: "run_1".into(),
            started_at: "2026-09-09 02:30:00".into(),
            finished_at: Some("2026-09-09 02:31:00".into()),
            status: "success".into(),
            range_start: Some("2026-09-08 02:30:00".into()),
            range_end: Some("2026-09-09 02:30:00".into()),
            message_count: 42,
            attachment_success: 3,
            attachment_failed: 1,
            error: None,
            log_lines: vec!["拉取消息: 42 条".into()],
        });
        store.upsert(schedule);

        save_store_to(&store, &path).unwrap();
        let loaded = load_store_from(&path).unwrap();
        assert_eq!(loaded.schedules.len(), 1);
        assert_eq!(loaded.schedules[0].name, "任务A");
        assert_eq!(
            loaded.schedules[0].last_success_at,
            Some("2026-09-09 02:30:00".into())
        );
        assert_eq!(loaded.schedules[0].runs.len(), 1);
        assert_eq!(loaded.schedules[0].runs[0].message_count, 42);
        assert_eq!(loaded.version, 1);

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn corrupted_store_backs_up_and_returns_empty() {
        let temp_dir =
            std::env::temp_dir().join(format!("dingtalk-schedule-corrupt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("schedules.json");
        fs::write(&path, "not valid json {{{").unwrap();

        let loaded = load_store_from(&path).unwrap();
        assert!(loaded.schedules.is_empty());
        // 原文件已被备份为 .corrupt
        assert!(!path.exists());
        assert!(temp_dir.join("schedules.json.corrupt").exists());

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn missing_store_file_returns_default() {
        let temp_dir =
            std::env::temp_dir().join(format!("dingtalk-schedule-missing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let path = temp_dir.join("schedules.json");
        let loaded = load_store_from(&path).unwrap();
        assert!(loaded.schedules.is_empty());
        assert_eq!(loaded.version, 1);
    }

    #[test]
    fn legacy_store_without_new_fields_loads() {
        // 旧版本文件缺少 runs / nextRunAt 等字段，serde default 应兜底
        let json = r#"{
            "version": 1,
            "schedules": [{
                "id": "sch_old",
                "name": "旧任务",
                "enabled": true,
                "groups": [],
                "schedule": {
                    "mode": "simple",
                    "unit": "day",
                    "interval": 1,
                    "atTime": "02:30",
                    "startTime": "2026-09-10 00:00:00",
                    "cron": "30 2 * * *"
                }
            }]
        }"#;
        let store: ScheduleStore = serde_json::from_str(json).unwrap();
        assert_eq!(store.schedules.len(), 1);
        let schedule = &store.schedules[0];
        assert!(schedule.runs.is_empty());
        assert!(schedule.next_run_at.is_none());
        assert!(schedule.last_success_at.is_none());
        assert_eq!(schedule.run_count, 0);
        assert!(schedule.earliest_chat_date.is_none());
        assert!(schedule.output_root.is_none());
    }

    #[test]
    fn compute_next_run_respects_start_time() {
        let config = simple_config(ScheduleUnit::Day, 1); // 每天 02:30
                                                          // 基准时间早于 startTime → 首次触发为 startTime 之后的第一个 02:30
        let next = compute_next_run(&config, "2026-09-01 00:00:00").unwrap();
        assert_eq!(next, "2026-09-10 02:30:00");

        // 基准时间晚于 startTime → 正常的下一个 02:30
        let next = compute_next_run(&config, "2026-09-15 10:00:00").unwrap();
        assert_eq!(next, "2026-09-16 02:30:00");
    }

    #[test]
    fn previous_minute_handles_borrow() {
        assert_eq!(
            previous_minute("2026-09-10 00:00:00"),
            Some("2026-09-09 23:59:00".into())
        );
        assert_eq!(
            previous_minute("2026-01-01 00:00:00"),
            Some("2025-12-31 23:59:00".into())
        );
        assert_eq!(
            previous_minute("2026-03-01 00:00:00"),
            Some("2026-02-28 23:59:00".into())
        );
        assert_eq!(previous_minute("bad"), None);
    }

    #[test]
    fn generate_schedule_id_from_datetime() {
        let id = generate_schedule_id("2026-09-09 15:47:00");
        assert_eq!(id, "sch_20260909154700");
    }
}
