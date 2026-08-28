# 钉钉群聊导出器 v1.0.3

本版本提升长群聊导出的稳定性，并增加可脱敏分享的诊断日志。

## 本版改进

- 修复长分页过程中 `chat/list_conversation_message_v2` 偶发误报 `AUTH_PERMISSION_DENIED` 后直接放弃整个群的问题。
- 仅对该消息接口的特定权限误报执行最多 4 次有限退避重试；真实权限不足和其他权限错误仍会明确失败。
- “开始导出”区域新增“导出诊断日志”按钮，可选择位置保存 `dingtalk-chat-exporter-diagnostic.txt`。
- 诊断日志包含应用与 dws 版本、系统架构、任务状态、分页进度、重试记录、错误和 `trace_id`。
- 诊断日志不包含聊天正文、登录令牌或附件内容；用户主目录显示为 `<HOME>`，凭据字段整行脱敏。
- Release 下载文件继续使用不带版本号的稳定名称。

## 验证

- Rust 单元测试 21 项全部通过。
- `cargo fmt` 和 `cargo clippy -D warnings` 通过。
- Windows x64 目标编译检查通过。
- Playwright 验证诊断日志保存成功、取消和失败三种界面状态。
- 使用长历史测试群执行完整分页回归，服务端临时权限错误在有限退避后恢复，最终返回 `hasMore=false`。

## 下载

- `dingtalk-chat-exporter-macOS-arm64.zip`：macOS Apple Silicon 应用包。
- `dingtalk-chat-exporter-Windows-x64.exe`：Windows 10/11 x64 免安装程序。
- 同名 `.sha256.txt` 文件可用于校验下载完整性。

## SHA-256

- macOS Apple Silicon：`301222731b665f0f47a723ef4bb7e8deea3df472a550a9ed710e7a41d93f765b`
- Windows x64：`9d62f23da26c046d2b91856205c3c98ee922ba3bc48a26bb1b066971d4500e68`

## 升级说明

- macOS：退出旧版本后，用新 `.app` 替换旧版本。
- Windows：退出旧版本后，直接使用新的 `.exe`。
- 本版本不涉及配置迁移。

## 签名说明

macOS 应用使用 ad-hoc 签名，未进行 Apple 公证；Windows 程序未进行 Authenticode 签名。
