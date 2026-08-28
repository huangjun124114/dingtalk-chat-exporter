# 钉钉群聊导出器 v1.0.4

本版本集中提升长群聊、超大附件和异常中断场景下的导出完整性，并完善诊断、跨平台兼容和界面状态。

## 界面预览

### 导出工具

![钉钉群聊导出器界面](https://github.com/ainuoyan/dingtalk-chat-exporter/raw/main/docs/images/app-interface.png)

### 导出的钉钉样式聊天记录

![钉钉样式聊天记录](https://github.com/ainuoyan/dingtalk-chat-exporter/raw/main/docs/images/chat-viewer.png)

预览图使用匿名演示数据，不包含真实组织、账号或聊天内容。

## 本版改进

- macOS App Translocation 场景默认导出到“下载”目录，避免落入系统临时挂载路径；普通运行仍优先使用应用所在目录。
- 导出的 HTML 使用清洗后的群聊名称作为文件名，并兼容 Windows 保留名称及带括号的附件文件名。
- “取消导出”仅在后台任务启动成功后显示；新增自动脱敏的诊断日志导出入口。
- dws 环境检测、登录和群搜索改为异步执行，避免长命令阻塞桌面窗口。
- JSON 命令保持 45 秒超时，附件下载独立使用 30 分钟超时；超时、网络错误和指定消息权限误报支持有限重试。
- macOS/Linux 使用进程组、Windows 使用 Job Object 终止完整 dws 子进程树，取消和超时不会遗留后台进程。
- 严格校验 `hasMore`、消息时间和分页推进；同秒消息边界自适应扩页至 10000 条，无法保证完整时明确失败。
- 系统消息缺少发送人或正文时使用安全默认值，关键消息 ID 和时间仍保持严格校验。
- 子进程输出限制为 8 MiB 尾部，消息限制为 100 万条或约 512 MiB，避免异常数据耗尽内存。
- 导出先写入 `.partial` 断点目录，全部成功后事务替换正式目录；取消后可复用已下载附件。
- 发布中断后，下次导出会自动恢复最近的 `.backup-*` 旧导出；正式目录存在时自动清理残留备份。
- 新导出已经就位但旧备份清理失败时降级为警告，不再误报整个群发布失败。
- 修复音频 HTML 闭合标签、Emoji 群头像截断、日志强制滚底和日志全量轮询问题。
- 应用版本由 Cargo 提供给页面，并在构建期校验 Cargo、Tauri 和 macOS Info.plist 三处版本一致。

## 验证

- `cargo fmt --all -- --check` 通过。
- `cargo test --all-targets --locked --offline`：41 项测试全部通过。
- `cargo clippy --all-targets --locked --offline -- -D warnings` 通过。
- Windows x64 Release 交叉编译通过，产物为 GUI PE32+ x86-64，未发现动态 `VCRUNTIME*.dll` 依赖。
- macOS arm64 Release 应用打包成功；压缩包完整性、应用版本 1.0.4 和严格 codesign 校验通过。
- 前端 JavaScript 语法、macOS 打包脚本语法和 Git 差异检查通过。

## 下载

- `dingtalk-chat-exporter-macOS-arm64.zip`：macOS Apple Silicon 应用包。
- `dingtalk-chat-exporter-Windows-x64.exe`：Windows 10/11 x64 免安装程序。
- 同名 `.sha256.txt` 文件可用于校验下载完整性。

## SHA-256

- macOS Apple Silicon：`9861d64a50f74682e2b02bc3817e5b2c311adfc30a2a0b53474eb701eaf4b9b4`
- Windows x64：`19ed3aa79fc466d43bf383c008ef78989329e6567fb5402476111a3d011775f4`

## 升级说明

- macOS：退出旧版本后，解压并用新的 `.app` 替换旧版本。
- Windows：退出旧版本后，直接使用新的 `.exe`。
- 本版本不涉及配置迁移；未完成导出的 `.partial` 目录可由新版本继续复用。

## 签名说明

macOS 应用使用 ad-hoc 签名，未进行 Apple 公证；Windows 程序未进行 Authenticode 签名。
