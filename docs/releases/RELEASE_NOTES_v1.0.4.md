# 钉钉群聊导出器 v1.0.4

本版本完成导出日志、设置管理模块，优化目录命名和时间范围支持，修复时区不一致问题，并完成代码质量优化。

## 界面预览

### 导出工具

![钉钉群聊导出器界面](https://github.com/huangjun124114/dingtalk-chat-exporter/raw/main/docs/images/app-interface.png)

### 导出的钉钉样式聊天记录

![钉钉样式聊天记录](https://github.com/huangjun124114/dingtalk-chat-exporter/raw/main/docs/images/chat-viewer.png)

预览图使用匿名演示数据，不包含真实组织、账号或聊天内容。

## 本版改进

### 新增功能
- **导出日志模块**：新增 `export_log.rs`，记录每次导出的详细信息到 `export_logs.json`，便于追溯和审计
- **设置管理模块**：新增 `settings.rs`，支持跨平台持久化用户配置（默认输出目录、文件名安全模式）
- **时区常量**：在 `date.rs` 中定义 `CHINA_STANDARD_TIME_OFFSET_SECS` 常量，统一 UTC+8 偏移处理

### 改进优化
- **目录命名优化**：导出目录从 `{群名}_{id片段}_{hash}` 改为 `{群名}_{MMDD}_{HHMMSS}`，便于记忆和排序
- **时间范围支持**：支持开始时间 + 结束时间双向过滤，不再只支持截止时间
- **原子写入**：`write_json` 和 `append_export_log` 均采用 `.tmp + rename` 模式，防止写入中断导致数据损坏
- **颜色哈希算法**：`color_for` 从字符求和改为 FNV-1a 哈希，不同发送者的颜色分布更均匀
- **附件路径修复**：`viewer.rs` 使用完整相对路径（含年月子目录）查找附件，修复路径不匹配问题

### 代码质量
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

## 验证

- `cargo check` 通过，0 errors / 0 warnings
- `cargo test --lib`：50 项测试全部通过
- `cargo clippy`：3 warnings（均可自动修复，非阻塞）
- 代码质量评分：B+ (87/100)

## 下载

- `dingtalk-chat-exporter.exe`：Windows 10/11 x64 免安装程序（6.5 MB）

## 升级说明

- Windows：退出旧版本后，直接使用新的 `.exe`
- 本版本不涉及配置迁移
- 历史导出目录不受影响

## 技术细节

- 编译环境：Rust 1.98.0 + Tauri v2
- 目标平台：Windows x64
- CRT 静态链接：已启用
- 依赖：无动态 VCRUNTIME*.dll 依赖
