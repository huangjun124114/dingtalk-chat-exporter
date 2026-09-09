// 后台调度引擎 —— 定时导出任务的触发与运行记录
//
// 设计（对应《定时导出功能设计方案 v1.1》）：
// - 单一后台线程，每 30 秒 tick 一次；桌面应用运行期间才会触发（界面需提示）
// - 每次 tick 重新加载 schedules.json（界面改动即时生效），STORE_LOCK 串行化读写避免与 commands 竞争
// - 三阶段执行：
//   1. 短锁扫描：找 next_run_at <= now 的启用任务；全局任务忙 → 记 skipped 并重算下次时间
//   2. 长任务导出：调用 exporter::run_job（ScheduledGroupArchive），不持有 store 锁
//   3. 短锁回写：重新加载 store（尊重运行期间的用户编辑），写运行记录/水位线/下次触发时间
// - 运行区间 = [水位线, 触发时刻)；仅完全成功才推进水位线（失败/部分失败下次重拉，
//   由消息 ID 去重与附件复用兜底，不会重复渲染或重复下载）
// - 错过不补跑：运行后从"当前时间"重算下次触发，应用关闭期间错过的点位跳过（增量水位线兜底数据不丢）

use crate::export_log;
use crate::exporter::{self, ExportJob, ExportProgress, GroupExportRequest, Trigger};
use crate::schedule::{self, Schedule, ScheduleRun};
use crate::{dws, AppInner, TaskState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// tick 间隔（秒）
const TICK_SECONDS: u64 = 30;

/// schedules.json 读写串行锁（scheduler 与 UI commands 共用，防止并发写坏文件）
pub(crate) static STORE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn store_guard() -> std::sync::MutexGuard<'static, ()> {
    STORE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 启动后台调度线程（应用 setup 时调用一次）
pub fn start_scheduler(inner: Arc<Mutex<AppInner>>, cancel_requested: Arc<AtomicBool>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(TICK_SECONDS));
        tick(&inner, &cancel_requested);
    });
}

/// 一次调度检查（独立函数便于测试与手动触发复用）
pub fn tick(inner: &Arc<Mutex<AppInner>>, cancel_requested: &Arc<AtomicBool>) {
    // 忙闲在进入 store 锁之前取快照：scan_due 持 store 锁期间不再锁 inner，
    // 避免 store→inner 锁嵌套（与 Task#6 命令的 inner→store 顺序冲突会死锁）。
    // 快照到执行之间的状态变化由 execute 的双重检查兜底。
    let busy_snapshot = task_slot_busy(inner);
    let busy = || busy_snapshot;

    // 阶段 1：扫描到期任务（短锁）
    let due = {
        let _guard = store_guard();
        match scan_due(&dws::current_time_str(), &busy) {
            ScanResult::None => return,
            ScanResult::Skipped { schedule_id } => {
                eprintln!("[scheduler] 任务 {schedule_id} 到期但已有导出在运行，标记 skipped");
                return;
            }
            ScanResult::Due {
                schedule,
                trigger_time,
            } => DueTrigger {
                schedule,
                trigger_time,
            },
        }
    };

    // 阶段 2：执行导出（长任务，不持有 store 锁）
    let now = dws::current_time_str();
    let run = execute(
        inner,
        cancel_requested,
        &due.schedule,
        &due.trigger_time,
        &now,
    );

    // 阶段 3：回写运行记录与水位线（短锁）
    let _guard = store_guard();
    finalize(&due.schedule.id, &run);
}

struct DueTrigger {
    schedule: Schedule,
    trigger_time: String,
}

enum ScanResult {
    None,
    Skipped {
        schedule_id: String,
    },
    Due {
        schedule: Schedule,
        trigger_time: String,
    },
}

/// 扫描到期任务；顺带为缺 next_run_at 的启用任务补算并持久化。
/// 忙时直接记录 skipped 并保存（返回 Skipped）。
fn scan_due(now: &str, busy: &dyn Fn() -> bool) -> ScanResult {
    scan_due_with(
        now,
        busy,
        &mut schedule::load_store,
        &mut |store: &schedule::ScheduleStore| schedule::save_store(store),
    )
}

/// 可测试版本：注入加载/保存函数
fn scan_due_with<L, S>(now: &str, busy: &dyn Fn() -> bool, load: &mut L, save: &mut S) -> ScanResult
where
    L: FnMut() -> Result<schedule::ScheduleStore, String>,
    S: FnMut(&schedule::ScheduleStore) -> Result<(), String>,
{
    let mut store = match load() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("[scheduler] 加载定时任务失败: {error}");
            return ScanResult::None;
        }
    };
    let mut dirty = false;

    // 补算缺失的 next_run_at（新建任务/上次计算失败的任务）
    for schedule in store.schedules.iter_mut() {
        if schedule.enabled && schedule.next_run_at.is_none() {
            if let Some(next) = schedule::compute_next_run(&schedule.schedule, now) {
                schedule.next_run_at = Some(next);
                dirty = true;
            }
        }
    }

    // 找第一个到期任务（格式固定 yyyy-MM-dd HH:mm:ss，字典序 = 时间序）
    let due_index = store.schedules.iter().position(|schedule| {
        schedule.enabled
            && schedule
                .next_run_at
                .as_deref()
                .is_some_and(|next| is_due(next, now))
    });
    let Some(index) = due_index else {
        if dirty {
            if let Err(error) = save(&store) {
                eprintln!("[scheduler] 保存定时任务失败: {error}");
            }
        }
        return ScanResult::None;
    };

    let schedule = store.schedules[index].clone();
    let trigger_time = schedule
        .next_run_at
        .clone()
        .unwrap_or_else(|| now.to_string());

    // 全局单任务互斥：忙则记 skipped，不补跑
    if busy() {
        let run = skipped_run(now, &trigger_time, "到期时已有导出任务正在运行".into());
        if let Some(target) = store.schedules.get_mut(index) {
            target.push_run(run);
            target.last_run_at = Some(now.to_string());
            target.next_run_at = schedule::compute_next_run(&target.schedule, now);
        }
        if let Err(error) = save(&store) {
            eprintln!("[scheduler] 保存 skipped 记录失败: {error}");
        }
        return ScanResult::Skipped {
            schedule_id: schedule.id,
        };
    }

    ScanResult::Due {
        schedule,
        trigger_time,
    }
}

/// 任务槽是否被占用（手动导出或另一次定时导出正在运行）
fn task_slot_busy(inner: &Arc<Mutex<AppInner>>) -> bool {
    inner.lock().is_ok_and(|guard| {
        guard
            .task
            .as_ref()
            .is_some_and(|task| task.status == "running")
    })
}

fn is_due(next_run_at: &str, now: &str) -> bool {
    next_run_at <= now
}

/// 执行一次定时导出：占用任务槽 → run_job → 释放任务槽，返回运行记录
fn execute(
    inner: &Arc<Mutex<AppInner>>,
    cancel_requested: &Arc<AtomicBool>,
    schedule: &Schedule,
    trigger_time: &str,
    now: &str,
) -> ScheduleRun {
    let started_at = dws::current_time_str();

    // 占用任务槽（与手动导出互斥）；抢占失败按 skipped 处理（双重检查，防扫描与执行间的竞态）
    {
        let Ok(mut guard) = inner.lock() else {
            return skipped_run(now, trigger_time, "应用状态锁异常".into());
        };
        if guard
            .task
            .as_ref()
            .is_some_and(|task| task.status == "running")
        {
            drop(guard);
            return skipped_run(now, trigger_time, "已有导出任务正在运行".into());
        }
        guard.task = Some(TaskState {
            kind: "schedule".into(),
            status: "running".into(),
            progress_text: format!("【定时】任务「{}」开始导出", schedule.name),
            log: Vec::new(),
            log_start: 0,
            output_path: None,
            error: None,
        });
    }
    // 定时任务不支持界面取消（cancel 标志与手动共用，需重置）
    cancel_requested.store(false, Ordering::Relaxed);

    let range_start = schedule.resolve_start_time();
    let range_end = trigger_time.to_string();
    let output_root = resolve_output_root(schedule);
    let self_name = resolve_self_name(inner);

    let groups: Vec<GroupExportRequest> = schedule
        .groups
        .iter()
        .map(|group| GroupExportRequest {
            title: group.title.clone(),
            open_conversation_id: group.open_conversation_id.clone(),
            create_at: group.create_at.clone(),
        })
        .collect();

    let job = ExportJob {
        groups,
        output_root: output_root.clone(),
        self_name,
        start_time: range_start.clone(),
        end_time: Some(range_end.clone()),
        trigger: Trigger::Scheduled {
            schedule_id: schedule.id.clone(),
        },
        archive: Box::new(exporter::ScheduledGroupArchive),
        cancel: cancel_requested.clone(),
    };
    let progress = crate::TaskProgress {
        state: inner.clone(),
    };
    let outcome = exporter::run_job(&job, &progress);
    let log_lines = progress.log_snapshot();
    let finished_at = dws::current_time_str();

    let status = map_outcome_status(&outcome);
    let message_count: usize = outcome.groups.iter().map(|group| group.message_count).sum();
    let attachment_success: usize = outcome
        .groups
        .iter()
        .map(|group| group.attachment_success)
        .sum();
    let attachment_failed: usize = outcome
        .groups
        .iter()
        .map(|group| group.attachment_failed)
        .sum();
    let error = if outcome.all_errors.is_empty() {
        None
    } else {
        Some(outcome.all_errors.join("; "))
    };

    // 释放任务槽（界面显示最终状态；日志保留在任务槽中供界面查看）
    let (task_status, task_text) = match status.as_str() {
        "success" => (
            "done",
            format!(
                "【定时】任务「{}」导出完成（{} 条消息）",
                schedule.name, message_count
            ),
        ),
        "partial" => (
            "error",
            format!("【定时】任务「{}」部分完成，有错误", schedule.name),
        ),
        _ => (
            "error",
            format!("【定时】任务「{}」导出失败", schedule.name),
        ),
    };
    crate::finish_task(
        inner,
        task_status,
        task_text,
        error.clone(),
        Some(output_root),
    );

    ScheduleRun {
        run_id: export_log::generate_timestamp_id(),
        started_at,
        finished_at: Some(finished_at),
        status,
        range_start,
        range_end: Some(range_end),
        message_count,
        attachment_success,
        attachment_failed,
        error,
        log_lines,
    }
}

fn skipped_run(now: &str, trigger_time: &str, reason: String) -> ScheduleRun {
    ScheduleRun {
        run_id: export_log::generate_timestamp_id(),
        started_at: now.to_string(),
        finished_at: Some(now.to_string()),
        status: "skipped".into(),
        range_start: None,
        range_end: Some(trigger_time.to_string()),
        message_count: 0,
        attachment_success: 0,
        attachment_failed: 0,
        error: Some(format!("{reason}，本次跳过（错过不补跑，增量水位线兜底）")),
        log_lines: Vec::new(),
    }
}

/// outcome → 运行状态：done=success；有群已产出=partial；其余=error
fn map_outcome_status(outcome: &exporter::JobOutcome) -> String {
    match outcome.status.as_str() {
        "done" => "success".into(),
        "cancelled" => "error".into(),
        _ => {
            let any_published = outcome
                .groups
                .iter()
                .any(|group| group.status == "success" || group.status == "partial");
            if any_published {
                "partial".into()
            } else {
                "error".into()
            }
        }
    }
}

/// 仅完全成功才推进水位线
fn should_advance_watermark(run_status: &str) -> bool {
    run_status == "success"
}

/// 回写：重新加载 store（尊重运行期间用户编辑），按 id 更新并保存
fn finalize(schedule_id: &str, run: &ScheduleRun) {
    let mut store = match schedule::load_store() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("[scheduler] 回写时加载定时任务失败: {error}");
            return;
        }
    };
    let Some(schedule) = store.find_mut(schedule_id) else {
        return; // 运行期间被删除：丢弃记录
    };
    let finished_now = dws::current_time_str();

    if should_advance_watermark(&run.status) {
        // 水位线 = 本次运行区间终点（触发时刻），与下次区间无缝衔接
        schedule.last_success_at = run.range_end.clone();
    }
    schedule.last_run_at = Some(finished_now.clone());
    // 错过不补跑：从当前时间重算下次触发
    schedule.next_run_at = schedule::compute_next_run(&schedule.schedule, &finished_now);
    schedule.push_run(run.clone());

    if let Err(error) = schedule::save_store(&store) {
        eprintln!("[scheduler] 保存运行记录失败: {error}");
    }
}

/// 输出目录：任务未配置时用应用默认目录
fn resolve_output_root(schedule: &Schedule) -> String {
    schedule
        .output_root
        .clone()
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(crate::get_default_output_dir)
}

/// HTML 抬头用户名：优先内存中的登录信息，其次现场探测，最后回退
fn resolve_self_name(inner: &Arc<Mutex<AppInner>>) -> String {
    if let Ok(guard) = inner.lock() {
        if let Some(auth) = &guard.auth {
            return auth.user_name.clone();
        }
    }
    dws::check_auth()
        .map(|auth| auth.user_name)
        .unwrap_or_else(|_| "定时任务".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exporter::{GroupOutcome, JobOutcome};
    use crate::schedule::{ScheduleConfig, ScheduleGroup, ScheduleStore, ScheduleUnit};

    fn test_store_with_next(next_run_at: Option<&str>, enabled: bool) -> ScheduleStore {
        let mut store = ScheduleStore::default();
        store.schedules.push(Schedule {
            id: "sch_1".into(),
            name: "任务A".into(),
            enabled,
            groups: vec![ScheduleGroup {
                title: "测试群".into(),
                open_conversation_id: "cid_x".into(),
                create_at: None,
            }],
            output_root: None,
            earliest_chat_date: None,
            schedule: ScheduleConfig {
                mode: "simple".into(),
                unit: Some(ScheduleUnit::Day),
                interval: 1,
                weekdays: Vec::new(),
                month_days: Vec::new(),
                at_time: Some("02:30".into()),
                start_time: "2026-09-01 00:00:00".into(),
                cron: "30 2 * * *".into(),
            },
            last_success_at: None,
            last_run_at: None,
            next_run_at: next_run_at.map(String::from),
            run_count: 0,
            runs: Vec::new(),
        });
        store
    }

    #[test]
    fn due_check_is_inclusive_string_compare() {
        assert!(is_due("2026-09-09 02:30:00", "2026-09-09 02:30:00"));
        assert!(is_due("2026-09-09 02:30:00", "2026-09-09 02:30:59"));
        assert!(!is_due("2026-09-09 02:31:00", "2026-09-09 02:30:59"));
    }

    #[test]
    fn scan_fills_missing_next_run_and_persists() {
        let store = test_store_with_next(None, true);
        let mut saved: Option<ScheduleStore> = None;
        let result = scan_due_with(
            "2026-09-09 10:00:00",
            &|| false,
            &mut || Ok(store.clone()),
            &mut |candidate: &ScheduleStore| {
                saved = Some(candidate.clone());
                Ok(())
            },
        );
        assert!(matches!(result, ScanResult::None));
        let saved = saved.expect("应触发保存");
        assert_eq!(
            saved.schedules[0].next_run_at.as_deref(),
            Some("2026-09-10 02:30:00")
        );
    }

    #[test]
    fn scan_ignores_disabled_and_not_due() {
        let store = test_store_with_next(Some("2026-09-10 02:30:00"), true);
        let mut saved = false;
        let result = scan_due_with(
            "2026-09-09 10:00:00",
            &|| false,
            &mut || Ok(store.clone()),
            &mut |_| {
                saved = true;
                Ok(())
            },
        );
        assert!(matches!(result, ScanResult::None));
        assert!(!saved, "未到期不应写盘");

        let disabled = test_store_with_next(Some("2026-09-09 02:30:00"), false);
        let result = scan_due_with(
            "2026-09-09 10:00:00",
            &|| false,
            &mut || Ok(disabled.clone()),
            &mut |_| Ok(()),
        );
        assert!(matches!(result, ScanResult::None), "禁用任务不应触发");
    }

    #[test]
    fn scan_returns_due_when_time_reached() {
        let store = test_store_with_next(Some("2026-09-09 02:30:00"), true);
        let result = scan_due_with(
            "2026-09-09 02:30:10",
            &|| false,
            &mut || Ok(store.clone()),
            &mut |_| Ok(()),
        );
        match result {
            ScanResult::Due {
                schedule,
                trigger_time,
            } => {
                assert_eq!(schedule.id, "sch_1");
                assert_eq!(trigger_time, "2026-09-09 02:30:00");
            }
            other => panic!(
                "应返回 Due，实际 {:?}",
                match other {
                    ScanResult::None => "None",
                    ScanResult::Skipped { .. } => "Skipped",
                    ScanResult::Due { .. } => "Due",
                }
            ),
        }
    }

    #[test]
    fn scan_marks_skipped_when_task_slot_busy() {
        let store = test_store_with_next(Some("2026-09-09 02:30:00"), true);
        let mut saved: Option<ScheduleStore> = None;
        let result = scan_due_with(
            "2026-09-09 02:30:10",
            &|| true, // 忙
            &mut || Ok(store.clone()),
            &mut |candidate: &ScheduleStore| {
                saved = Some(candidate.clone());
                Ok(())
            },
        );
        assert!(matches!(result, ScanResult::Skipped { .. }));
        let saved = saved.expect("skipped 应写盘");
        let schedule = &saved.schedules[0];
        assert_eq!(schedule.runs.len(), 1);
        assert_eq!(schedule.runs[0].status, "skipped");
        assert_eq!(schedule.run_count, 1);
        // 下次触发从当前时间重算（错过不补跑）
        assert_eq!(schedule.next_run_at.as_deref(), Some("2026-09-10 02:30:00"));
        assert_eq!(schedule.last_run_at.as_deref(), Some("2026-09-09 02:30:10"));
    }

    #[test]
    fn outcome_status_mapping() {
        let done = JobOutcome {
            status: "done".into(),
            groups: Vec::new(),
            all_errors: Vec::new(),
        };
        assert_eq!(map_outcome_status(&done), "success");

        let cancelled = JobOutcome {
            status: "cancelled".into(),
            groups: Vec::new(),
            all_errors: Vec::new(),
        };
        assert_eq!(map_outcome_status(&cancelled), "error");

        let partial = JobOutcome {
            status: "error".into(),
            groups: vec![GroupOutcome {
                group_title: "A".into(),
                status: "success".into(),
                ..Default::default()
            }],
            all_errors: vec!["附件失败".into()],
        };
        assert_eq!(map_outcome_status(&partial), "partial");

        let failed = JobOutcome {
            status: "error".into(),
            groups: Vec::new(),
            all_errors: vec!["拉取失败".into()],
        };
        assert_eq!(map_outcome_status(&failed), "error");
    }

    #[test]
    fn watermark_advances_only_on_success() {
        assert!(should_advance_watermark("success"));
        assert!(!should_advance_watermark("partial"));
        assert!(!should_advance_watermark("error"));
        assert!(!should_advance_watermark("skipped"));
    }

    #[test]
    fn output_root_falls_back_to_default() {
        let store = test_store_with_next(None, true);
        let schedule = &store.schedules[0];
        assert!(!resolve_output_root(schedule).is_empty());
    }

    #[test]
    fn task_slot_busy_detects_running_task() {
        let inner = Arc::new(Mutex::new(AppInner {
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
        assert!(task_slot_busy(&inner));

        if let Ok(mut guard) = inner.lock() {
            if let Some(task) = guard.task.as_mut() {
                task.status = "done".into();
            }
        }
        assert!(!task_slot_busy(&inner));
    }
}
