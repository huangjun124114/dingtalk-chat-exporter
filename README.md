# 钉钉群聊导出器

基于 Tauri v2 的桌面应用，通过本机已登录的 `dws` CLI 搜索群聊、导出全部主消息与话题回复、下载附件，并生成以群聊名称命名的自包含钉钉风格 HTML。

## 使用前提

- 已安装 `dws`，并完成 `dws auth login`。
- macOS 10.15+，或 Windows 10/11。
- 导出目录需要有足够空间；附件会同时保留为独立文件并内嵌到 HTML。

启动应用后先检查登录状态，再搜索并选择群聊和导出目录。任务运行时可以取消；成功、失败、部分附件失败和取消会显示为不同状态，不会把不完整导出误报为成功。

默认导出目录通常为应用所在文件夹：macOS 使用 `.app` 外层目录，Windows 使用 `.exe` 所在目录。macOS 被 Gatekeeper 以 App Translocation 隔离运行时无法可靠反查原始位置，因此自动改用当前用户的“下载”目录。也可以在界面中选择其他目录。

每个群会先写入同级隐藏的 `.partial` 断点目录；只有消息、附件索引和 HTML 全部成功后才原子替换旧导出。失败或取消不会破坏上一次完整结果，下次导出会复用断点目录和旧结果中已经下载成功的附件。

## 导出内容

每个群聊生成一个独立目录：

```text
群名称_群ID/
├── messages.json
├── attachments_index.json
├── attachments/
└── 清理后的群聊名称.html
```

为避免异常 dws 输出或极端群历史耗尽内存，单个子进程最多保留最近 8 MiB 输出，单次群导出最多处理 100 万条或约 512 MiB 消息文本；达到上限会明确失败，不会把截断结果标记为成功。

- 群搜索、主消息和话题回复都会完整分页。
- HTML 文件名来自群聊名称，并会移除跨平台不合规字符。
- 图片、视频、音频和普通文件按实际 MIME 类型呈现。
- HTML 使用流式写入和 Base64 编码，避免生成阶段把全部附件加载进内存。
- `attachments_index.json` 记录每个附件的成功或失败状态；任一关键步骤失败时任务会返回错误。

## 开发验证

```bash
cd dingtalk-chat-exporter
cargo fmt --all -- --check
cargo test --all-targets --locked --offline
cargo clippy --all-targets --locked --offline -- -D warnings
```

## macOS 构建与打包

```bash
cd dingtalk-chat-exporter
./scripts/package-macos.sh
```

脚本会离线构建 Release 二进制、将本机主目录统一映射为 `/home/builder`、生成完整的 `.app` 目录和 `AppIcon.icns`，执行 ad-hoc 签名，并用 `codesign --deep --strict` 校验。默认产物为工作区 `artifacts` 目录下的 `钉钉群聊导出器.app`；也可以传入一个绝对 `.app` 路径。

ad-hoc 签名只保证包结构和代码签名自洽，不代表 Apple Developer ID 签名或公证。面向外部分发时仍需使用正式证书完成签名、公证和 Gatekeeper 验证。

## Windows 交叉编译

```bash
cd dingtalk-chat-exporter
RUSTFLAGS="-C target-feature=+crt-static --remap-path-prefix=${HOME}=/home/builder" \
  cargo xwin build --locked --release \
  --target x86_64-pc-windows-msvc --offline
```

产物位于 `target/x86_64-pc-windows-msvc/release/dingtalk-chat-exporter.exe`。构建命令会隐藏本机主目录，项目通过 `.cargo/config.toml` 静态链接 MSVC CRT，降低目标机器缺少 VC++ Runtime 的风险。

## 运行边界

- 单条 `dws` 调用最长等待 45 秒，超时会终止子进程并报错。
- 取消会终止当前 `dws` 子进程；已经写入磁盘的部分目录会保留，便于排查。
- 自包含 HTML 的体积大致等于消息内容加附件 Base64 后的总量；超大群聊可能受到浏览器单文件加载能力限制。
- 应用主动打开的外部链接只允许 `https://`；聊天正文中的原始 `http://` / `https://` 链接仍会保留。打开目录和附件文件名均经过边界校验。
