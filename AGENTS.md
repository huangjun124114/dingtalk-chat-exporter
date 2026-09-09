# 钉钉群聊导出器 (dingtalk-chat-exporter)

一个桌面 GUI 应用，让用户选择钉钉群聊，自动导出主消息、话题回复和附件，生成钉钉风格的聊天记录网页。

## 架构

Tauri v2 桌面应用,Rust 后端 + 原生 HTML/JS 前端(无 React/Vite/pnpm 构建链)。

```
dingtalk-chat-exporter/      源码项目
├── Cargo.toml               Rust 依赖
├── .cargo/config.toml       交叉编译 crt-static 配置(关键)
├── build.rs                 tauri-build
├── tauri.conf.json          Tauri 配置(窗口/CSP/bundle)
├── capabilities/default.json 权限(core + dialog)
├── icons/                   应用图标(png + ico)
├── packaging/macos/         macOS Info.plist 模板
├── scripts/package-macos.sh 可重复的 macOS 构建/打包/签名校验
├── frontend/
│   ├── index.html           应用界面(搜索群/选目录/导出/进度)
│   ├── _head.html           聊天查看器的 CSS 头模板
│   └── _foot.html           聊天查看器的 JS 尾模板(搜索/侧边栏)
└── src/
    ├── main.rs              入口(windows_subsystem=windows,不弹控制台)
    ├── lib.rs               Tauri commands + 后台导出任务引擎 + 轮询
    ├── dws.rs               dws CLI 调用封装(找 dws/登录检测/搜群/拉消息/下载附件)
    ├── date.rs              dws 时间格式校验与日期换算
    ├── media.rs             消息附件标记解析与文件名提取
    ├── viewer.rs            流式生成聊天 HTML(附件 base64 内嵌)
    ├── exporter.rs          导出核心(手动/定时共用):ExportJob + run_job + ArchiveStrategy
    ├── cron.rs              自研最小 cron 引擎(5 字段,*/N/范围/列表/星期名,北京时间)
    ├── schedule.rs          调度模型与持久化(Schedule/ScheduleConfig/ScheduleRun/ScheduleStore)
    └── scheduler.rs         后台调度引擎(30s tick 三阶段 + 水位线增量 + skipped 互斥)
```

## 核心设计

- **Rust 后端调 dws 二进制**:用 `std::process::Command` 调系统 PATH 上的 dws,不是 FFI。
- **完整分页**：群搜索、主消息与话题回复均拉取到分页结束；分页无法推进时明确失败。
- **前端轮询**:前端每 500ms 调 `snapshot` command 拉状态(进度/日志),不用事件推送。
- **后台任务**:导出在独立线程跑,通过 `Arc<Mutex<AppInner>>` 共享状态。
- **任务边界**：支持取消、JSON 命令 45 秒超时、附件下载 30 分钟超时、日志绝对偏移；附件或 HTML 失败不会误报成功。
- **事务发布**：先写同目录 `.partial` 断点目录，完整成功后再替换正式目录；中断发布遗留的 `.backup-*` 会在下次导出时恢复或清理。
- **自包含 HTML**:导出的 `chat_viewer.html` 流式内嵌图片、视频、音频和文件。

## 构建

### 前置依赖
- Rust(stable)+ cargo
- `cargo-xwin`(交叉编译 Windows 用):`cargo install cargo-xwin`
- dws 已安装并登录(`dws auth login`)

### 本地校验
```bash
cd dingtalk-chat-exporter
cargo fmt --all -- --check
cargo test --all-targets --locked --offline
cargo clippy --all-targets --locked --offline -- -D warnings
```

### 交叉编译 Windows exe
```bash
cargo xwin build --locked --release --target x86_64-pc-windows-msvc --offline
# 产物:target/x86_64-pc-windows-msvc/release/dingtalk-chat-exporter.exe
```

### 打包 Mac .app
```bash
./scripts/package-macos.sh
```

脚本会执行离线 Release 构建、生成完整包结构与图标、ad-hoc 签名及严格签名校验。正式外部分发仍需 Developer ID 签名和公证。

## 踩坑记录(务必遵守)

### 1. CSP 必须允许 inline script
`tauri.conf.json` 的 CSP 必须包含 `script-src 'self' 'unsafe-inline'`,否则前端 `<script>` 被静默拦截,**所有按钮点击无反应**(页面看起来正常但 JS 不执行)。

### 2. 必须静态链接 CRT
`.cargo/config.toml` 配置:
```toml
[target.x86_64-pc-windows-msvc]
rustflags = ["-C", "target-feature=+crt-static"]
```
否则 exe 依赖 `VCRUNTIME140.dll`,同事没装 VC++ Redistributable 会报错。
验证:`strings xxx.exe | grep VCRUNTIME` 应无输出。

### 3. 不要用 inline onclick,用 addEventListener
前端按钮事件必须用 `addEventListener` 绑定,不要用 HTML 的 `onclick="..."` 属性。WKWebView 在某些 CSP 配置下对 inline event handler 处理有差异,会导致点击无效。

### 4. GUI 程序的 PATH 不全
GUI 程序的 PATH 不含 `~/.local/bin` 等用户自定义路径,而 dws 常装在那里。`find_dws()` 必须额外检查常见安装路径(`~/.local/bin`、`/opt/homebrew/bin`、Windows 的 `%APPDATA%\npm`),不能只靠 `which`/`where`。

### 5. 调 dws 命令必须静默(不弹终端)
所有 `Command::new(dws)` 必须用 `silent_command()` 封装:
- Windows: `CREATE_NO_WINDOW`(0x08000000)
- 所有平台: stdin=null, stdout/stderr=piped
否则 GUI 程序调子进程会弹出终端/控制台窗口。

### 6. dws 输出可能有日志前缀
dws 的 stdout 可能在 JSON 前后混有 INFO 日志，解析器必须按 UTF-8 字符边界查找可反序列化的 JSON 对象或数组，不能按字节截取错误摘要。

### 7. Windows icon.ico 必须存在
Tauri 在 Windows target 编译时必须有 `icons/icon.ico`,否则 build.rs 报错。用 PNG 数据包一层 ICO header 即可。

### 8. Tauri v2 的 invoke API
纯 HTML 前端(无 @tauri-apps/api npm 包)用 `window.__TAURI_INTERNALS__.invoke(cmd, args)` 调后端。这个对象由 Tauri 在 webview 初始化时自动注入。

### 9. 目录选择用后端 dialog,不要用前端 window.__TAURI__.dialog
`window.__TAURI__.dialog` 是 Tauri v1 API,v2 不存在。目录选择必须在 Rust 后端用 `tauri-plugin-dialog` 的 `app.dialog().file().blocking_pick_folder()`。

### 10. 升版本必须三处同步(build.rs 有断言)
`build.rs` 会断言三处版本一致,任何一处漏改 release 构建直接 panic:
- `Cargo.toml` 的 `version`
- `tauri.conf.json` 的 `version`
- `packaging/macos/Info.plist` 的 `CFBundleShortVersionString`(顺带递增 `CFBundleVersion`)

## Tauri Commands

| Command | 作用 |
|---|---|
| `get_app_version` | 获取 Cargo 包版本，供前端动态显示 |
| `check_dws_installed` | 检测 dws 是否已安装 |
| `check_env` | 检查 dws 登录状态,返回 AppSnapshot |
| `login_dws` | 打开终端执行 `dws auth login` |
| `open_url` | 打开浏览器(URL) |
| `search_groups` | 搜索群聊 |
| `get_default_output_dir` | 获取程序所在目录；macOS App Translocation 下回退到下载目录 |
| `choose_output_dir` | 打开系统目录选择器 |
| `export_diagnostic_log` | 保存脱敏后的应用、dws 环境及导出任务诊断日志 |
| `export_groups` | 后台导出选中的群 |
| `cancel_export` | 取消当前导出并终止正在执行的 dws 子进程 |
| `snapshot` | 轮询获取当前状态(进度/日志) |
| `open_output` | 在文件管理器打开导出目录 |
| `list_schedules` | 列出全部定时任务(含自然语言描述) |
| `get_schedule_runs` | 查看某任务的运行历史(最多 50 条,含日志) |
| `preview_schedule` | 按配置预览未来 3 次触发时间 |
| `validate_schedule` | 预校验任务(cron/时间格式/同群冲突) |
| `save_schedule` | 新建/编辑任务(编辑时保留水位线与运行历史) |
| `delete_schedule` | 删除任务(连同运行历史) |
| `toggle_schedule` | 启用/停用任务(启用时重算下次触发) |
| `run_schedule_now` | 立即运行一次(与手动导出共享任务槽,忙则拒绝) |

## 定时导出核心设计(v1.1.0)

- **增量水位线**:每个任务记录 `lastSuccessAt`,运行区间 = [水位线, 触发时刻);仅完整成功才推进水位线,失败/取消下次自动重拉,不丢数据。
- **首次拉取起点**:max(最早聊天日志日期参数, 群创建时间);都缺失则全量。
- **不补跑**:错过触发点(关机/忙)后,`nextRunAt` 从当前时刻重算;漏掉的区间由水位线兜底在下次运行补齐。
- **全局单任务互斥**:手动与定时共享 `AppInner.task` 槽;定时到期遇忙 → 运行记录 `skipped`。
- **三阶段 tick(30s)**:短锁扫描(补算 nextRunAt/找到期/记 skipped) → 长导出(不持 store 锁) → 短锁回写(水位线/运行记录/重算下次)。
- **锁顺序纪律**:busy 快照必须在拿 store 锁之前取(避免 store→inner 与命令的 inner→store 嵌套死锁);`execute` 内双重检查兜底。
- **归档策略**:`ScheduledGroupArchive` 固定 `输出目录/群名/` 归档,按月合并 HTML(消息 ID 去重),附件已存在即复用;手动导出仍走 `PerRunArchive` 每次独立目录。
- **同群冲突**:同一群只允许被一个定时任务引用;前端勾选时预警,后端 `save_schedule` 硬拒绝。

## 参考项目

架构参考 `dingtalk-wiki-backup/wiki-backup-desktop`(Tauri v2 + React):
- 后台任务引擎 + 进程组取消(`task.rs`/`process.rs`)
- 前端轮询模型(500ms pull snapshot)
- `cargo xwin` 交叉编译 + crt-static
