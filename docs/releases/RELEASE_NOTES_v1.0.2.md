# 钉钉群聊导出器 v1.0.2

本版本修复应用页面版本号未随发布版本更新的问题。

## 本版改进

- 页面版本号改为从 Cargo 包版本动态读取，不再使用硬编码值。
- 应用页面、Cargo、Tauri 配置及 macOS 包版本统一为 `1.0.2`。
- “取消导出”按钮默认隐藏，仅在导出任务成功启动后显示。
- 保留 v1.0.1 的群聊名称 HTML 文件名清理逻辑。
- Release 下载文件继续使用不带版本号的稳定名称。

## 下载

- `dingtalk-chat-exporter-macOS-arm64.zip`：macOS Apple Silicon 应用包。
- `dingtalk-chat-exporter-Windows-x64.exe`：Windows 10/11 x64 免安装程序。
- 同名 `.sha256.txt` 文件可用于校验下载完整性。

## 使用前提

使用前请先安装 `dws` 并完成 `dws auth login`。

## 签名说明

macOS 应用使用 ad-hoc 签名，未进行 Apple 公证；Windows 程序未进行 Authenticode 签名。
