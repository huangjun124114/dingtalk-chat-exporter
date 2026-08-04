# 钉钉群聊导出器 v1.0.1

本版本优化导出文件命名，并统一发布制品名称。

## 本版改进

- 导出的 HTML 使用清理后的群聊名称命名，不再固定为 `chat_viewer.html`。
- 自动移除控制字符及 `\ / : * ? " < > |` 等跨平台不合规字符。
- 处理 Windows 保留名称、尾部点与空格；清理后为空时回退为 `群聊.html`。
- Release 下载文件使用不带版本号的稳定名称，便于自动化下载和覆盖更新。

## 下载

- `dingtalk-chat-exporter-macOS-arm64.zip`：macOS Apple Silicon 应用包。
- `dingtalk-chat-exporter-Windows-x64.exe`：Windows 10/11 x64 免安装程序。
- 同名 `.sha256.txt` 文件可用于校验下载完整性。

## 使用前提

使用前请先安装 `dws` 并完成 `dws auth login`。

## 签名说明

macOS 应用使用 ad-hoc 签名，未进行 Apple 公证；Windows 程序未进行 Authenticode 签名。
