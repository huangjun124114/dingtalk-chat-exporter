# 钉钉群聊导出器 v1.0.0

首个正式版本，用于通过本机已登录的 `dws` CLI 导出钉钉群聊记录、话题回复和附件，并生成自包含的聊天记录网页。

## 本版内容

- 统一项目、可执行文件和应用标识为 `dingtalk-chat-exporter`。
- 默认导出目录改为应用所在文件夹；macOS 会使用 `.app` 外层目录，Windows 会使用 `.exe` 所在目录。
- 支持群搜索、主消息和话题回复完整分页。
- 支持导出任务取消、子进程超时和失败状态区分。
- 附件与聊天记录会生成自包含 HTML，便于离线查看。

## 下载

- `dingtalk-chat-exporter-v1.0.0-macOS-arm64.zip`：macOS Apple Silicon 应用包。
- `dingtalk-chat-exporter-v1.0.0-Windows-x64.exe`：Windows 10/11 x64 免安装程序。
- 同名 `.sha256.txt` 文件可用于校验下载完整性。

## 使用前提

使用前请先安装 `dws` 并完成 `dws auth login`。

## 签名说明

macOS 应用使用 ad-hoc 签名，未进行 Apple 公证；Windows 程序未进行 Authenticode 签名。
