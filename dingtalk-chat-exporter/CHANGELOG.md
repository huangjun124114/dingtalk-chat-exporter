# 更新日志

本文件记录钉钉群聊导出器的所有重要变更。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

---

## [1.0.4] - 2026-08-31

### 新增
- **导出日志模块**：新增 `export_log.rs`，记录每次导出的详细信息到 `export_logs.json`
- **设置管理模块**：新增 `settings.rs`，支持跨平台持久化用户配置
- **时区常量**：在 `date.rs` 中定义 `CHINA_STANDARD_TIME_OFFSET_SECS` 常量，统一 UTC+8 偏移

### 改进
- **目录命名**：导出目录从 `{群名}_{id片段}_{hash}` 改为 `{群名}_{MMDD}_{HHMMSS}`，便于记忆和排序
- **时间范围支持**：支持开始时间 + 结束时间双向过滤，不再只支持截止时间
- **原子写入**：`write_json` 和 `append_export_log` 均采用 `.tmp + rename` 模式，防止写入中断导致数据损坏
- **颜色哈希算法**：`color_for` 从字符求和改为 FNV-1a 哈希，不同发送者的颜色分布更均匀
- **附件路径**：`viewer.rs` 使用完整相对路径（含年月子目录）查找附件，修复路径不匹配问题

### 优化
- **AppSettings 精简**：移除未消费的 `dws_timeout_secs`、`download_timeout_mins`、`page_limit_initial`、`page_limit_max` 字段，只保留实际生效的配置
- **日志 ID 时区修复**：`generate_timestamp_id()` 添加 UTC+8 偏移，与目录名时间保持一致
- **测试安全性**：`load_settings_returns_default_when_file_missing` 不再修改全局环境变量，避免并行测试竞态
- **废弃代码清理**：删除 `normalize_datetime`、`effective_cutoff_time`、`is_within_cutoff`、`crossed_cutoff` 等废弃函数及相关测试

### 重构
- **时间过滤函数重命名**：
  - `is_within_cutoff` → `is_within_time_range`
  - `crossed_cutoff` → `crossed_start_boundary`
- **消息拉取参数**：`fetch_all_messages` 从 `cutoff_time: Option<&str>` 改为 `start_time` + `end_time` 双参数

### 修复
- **日志 ID 时区不一致**：修复 `generate_timestamp_id()` 使用 UTC 时间导致日志 ID 与目录名时间矛盾的问题
- **附件路径查找**：修复 `viewer.rs` 只用文件名查找附件，但附件按年月分目录存储导致的路径不匹配问题

### 测试
- 新增 50 个单元测试，覆盖导出日志、设置管理、时间解析、文件名生成等核心功能
- 所有测试通过，覆盖率提升

---

## [1.0.3] - 2026-08-30

### 新增
- 支持导出指定时间范围的群聊天记录（截止时间过滤）
- 按月分文件生成 HTML，文件名格式 `{群名}-YYYYMM.html`
- 附件按年月分目录存储 `attachments/YYYYMM/`

### 改进
- 消息分页支持同秒边界检测 + 自适应分页扩大
- 流式输出截断，保留尾部 8MB 避免内存爆炸
- 诊断日志凭据脱敏，覆盖 16 种敏感标记

---

## [1.0.2] - 2026-08-29

### 新增
- Windows Job Object 进程树管理，确保超时或取消时彻底杀死所有子进程
- HTML 生成的取消传播，通过 `CancellableReader` 响应取消信号
- XSS 防护，`escape_html` 处理 `&<>"` 四种字符

### 改进
- `sanitize_filename` 覆盖 Windows 保留名（包括上标字符 ¹²³）
- 消息分页安全防护：`MAX_MESSAGES`（100 万）和 `MAX_MESSAGE_BYTES`（512 MB）防止内存耗尽

---

## [1.0.1] - 2026-08-28

### 新增
- 初始版本，支持导出钉钉群聊记录为 HTML
- 支持 Windows + macOS 跨平台
- 四菜单架构：导出/日志/设置/关于

---

## 版本说明

- **新增**：新功能
- **改进**：现有功能的增强和优化
- **优化**：性能提升和代码质量改进
- **重构**：代码结构调整，不影响功能
- **修复**：问题修复
- **测试**：测试覆盖和测试质量提升

---

[1.0.4]: https://github.com/huangjun124114/dingtalk-chat-exporter/compare/v1.0.3...v1.0.4
[1.0.3]: https://github.com/huangjun124114/dingtalk-chat-exporter/compare/v1.0.2...v1.0.3
[1.0.2]: https://github.com/huangjun124114/dingtalk-chat-exporter/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/huangjun124114/dingtalk-chat-exporter/releases/tag/v1.0.1
