// 生成钉钉风格的自包含聊天记录 HTML。

use crate::dws::Message;
use base64::engine::general_purpose::STANDARD as B64;
use base64::write::EncoderWriter;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const AVATAR_COLORS: [&str; 10] = [
    "#5B8FF9", "#5AD8A6", "#F6BD16", "#E86452", "#6DC8EC", "#945FB9", "#FF9845", "#1E9493",
    "#FF99C3", "#7F8B9C",
];

const WEEKDAYS: [&str; 7] = [
    "星期一",
    "星期二",
    "星期三",
    "星期四",
    "星期五",
    "星期六",
    "星期日",
];

pub const HTML_HEAD: &str = include_str!("../frontend/_head.html");
pub const HTML_FOOT: &str = include_str!("../frontend/_foot.html");

#[derive(Debug)]
struct MediaFile {
    path: PathBuf,
    download_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentRecord {
    open_message_id: String,
    file: String,
    status: String,
    #[serde(default)]
    original_file_name: Option<String>,
}

#[derive(Clone, Copy)]
enum MediaKind {
    Image,
    Video,
    Audio,
    File,
}

/// 生成某个月的聊天记录 HTML。
///
/// `index_path` 显式指定附件索引文件（手动导出 = 群目录根索引；
/// 定时导出 = attachments_index/{YYYYMM}.json 月度索引）。
pub fn generate_html(
    messages: &[Message],
    group_title: &str,
    attachments_dir: &Path,
    index_path: &Path,
    self_name: &str,
    output_path: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err(crate::dws::CANCELLED_ERROR.into());
    }
    let media_map = build_media_map(attachments_dir, index_path)?;
    let file = File::create(output_path)
        .map_err(|error| format!("创建 HTML {} 失败: {}", output_path.display(), error))?;
    let mut writer = BufWriter::new(file);
    write_document(
        &mut writer,
        messages,
        group_title,
        self_name,
        &media_map,
        cancel,
    )
    .map_err(|error| {
        if error.kind() == std::io::ErrorKind::Interrupted {
            crate::dws::CANCELLED_ERROR.into()
        } else {
            format!("写入 HTML {} 失败: {}", output_path.display(), error)
        }
    })?;
    writer
        .flush()
        .map_err(|error| format!("刷新 HTML {} 失败: {}", output_path.display(), error))
}

fn write_document<W: Write>(
    writer: &mut W,
    messages: &[Message],
    group_title: &str,
    self_name: &str,
    media_map: &HashMap<String, Vec<MediaFile>>,
    cancel: &AtomicBool,
) -> std::io::Result<()> {
    writer.write_all(HTML_HEAD.as_bytes())?;

    let mut sender_counts: BTreeMap<String, usize> = BTreeMap::new();
    for message in messages {
        *sender_counts.entry(message.sender.clone()).or_insert(0) += 1;
    }
    let mut senders_sorted: Vec<_> = sender_counts.iter().collect();
    senders_sorted.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
    let media_count: usize = media_map.values().map(Vec::len).sum();
    let date_range = match (messages.first(), messages.last()) {
        (Some(first), Some(last)) => {
            match (
                crate::date::parse_dws_datetime(&first.create_time),
                crate::date::parse_dws_datetime(&last.create_time),
            ) {
                (Some((y1, m1, d1, _, _, _)), Some((y2, m2, d2, _, _, _))) => format!(
                    "{:04}-{:02}-{:02} ~ {:04}-{:02}-{:02}",
                    y1, m1, d1, y2, m2, d2
                ),
                _ => "未知".into(),
            }
        }
        _ => "无消息".into(),
    };

    let icon_text = group_title.chars().take(2).collect::<String>();
    write!(
        writer,
        r#"<div class="header"><div class="group-icon">{}</div><div><div class="title">{}</div><div class="meta">{} · {} 位成员</div></div><div class="stats"><span><b>{}</b> 条消息</span><span><b>{}</b> 个附件</span></div></div>"#,
        escape_html(&icon_text),
        escape_html(group_title),
        date_range,
        senders_sorted.len(),
        messages.len(),
        media_count
    )?;

    write!(
        writer,
        r#"<div class="sidebar"><h3>群成员 · {}</h3>"#,
        senders_sorted.len()
    )?;
    for (name, count) in &senders_sorted {
        write!(
            writer,
            r#"<div class="member"><div class="avatar" style="background:{}">{}</div><div class="info"><div class="name">{}</div><div class="count">{} 条消息</div></div></div>"#,
            color_for(name),
            escape_html(&initial(name)),
            escape_html(name),
            count
        )?;
    }
    writer.write_all(b"</div><div class=\"chat-container\">")?;

    let mut previous_date = None;
    for message in messages {
        ensure_not_cancelled(cancel)?;
        if let Some((year, month, day, _, _, _)) =
            crate::date::parse_dws_datetime(&message.create_time)
        {
            if previous_date != Some((year, month, day)) {
                write!(
                    writer,
                    r#"<div class="date-sep"><span>{}</span></div>"#,
                    date_label(year, month, day)
                )?;
                previous_date = Some((year, month, day));
            }
        }

        let is_self = message.sender == self_name;
        let row_class = if is_self { "msg-row self" } else { "msg-row" };
        let time_text = message.create_time.get(11..16).unwrap_or("");
        write!(
            writer,
            r#"<div class="{}"><div class="avatar" style="background:{}">{}</div><div class="msg-body">"#,
            row_class,
            color_for(&message.sender),
            escape_html(&initial(&message.sender))
        )?;
        if !is_self {
            write!(
                writer,
                r#"<div class="sender-name">{}</div>"#,
                escape_html(&message.sender)
            )?;
        }
        writer.write_all(b"<div class=\"bubble\">")?;
        write_message_content(writer, message, media_map, cancel)?;
        write!(
            writer,
            r#"</div><div class="msg-time" title="{}">{}</div></div></div>"#,
            escape_html(&message.create_time),
            escape_html(time_text)
        )?;
    }

    writer.write_all(
        " <div class=\"date-sep\"><span>— 已是最新消息 —</span></div></div>"
            .trim_start()
            .as_bytes(),
    )?;
    writer.write_all(HTML_FOOT.as_bytes())
}

fn write_message_content<W: Write>(
    writer: &mut W,
    message: &Message,
    media_map: &HashMap<String, Vec<MediaFile>>,
    cancel: &AtomicBool,
) -> std::io::Result<()> {
    let has_media_marker = !crate::media::extract_media_ids(&message.content).is_empty();
    let text = if has_media_marker {
        crate::media::strip_media_markup(&message.content)
    } else {
        message.content.clone()
    };
    let text_html = (!text.is_empty()).then(|| linkify(&text));
    let files = media_map.get(&message.open_message_id);

    if let Some(text_html) = &text_html {
        write!(writer, r#"<div class="text-part">{}</div>"#, text_html)?;
    }

    if let Some(files) = files.filter(|files| !files.is_empty()) {
        write_media_files(writer, files, cancel)?;
    } else if has_media_marker {
        writer.write_all("<div class=\"media-placeholder\">附件（未下载）</div>".as_bytes())?;
    } else if text_html.is_none() {
        writer.write_all("<span class=\"media-placeholder\">（空消息）</span>".as_bytes())?;
    }
    Ok(())
}

fn write_media_files<W: Write>(
    writer: &mut W,
    files: &[MediaFile],
    cancel: &AtomicBool,
) -> std::io::Result<()> {
    if files.len() > 1 {
        writer.write_all(b"<div class=\"image-grid\">")?;
    }
    for media in files {
        ensure_not_cancelled(cancel)?;
        write_media_file(writer, media, cancel)?;
    }
    if files.len() > 1 {
        writer.write_all(b"</div>")?;
    }
    Ok(())
}

fn write_media_file<W: Write>(
    writer: &mut W,
    media: &MediaFile,
    cancel: &AtomicBool,
) -> std::io::Result<()> {
    let mut file = File::open(&media.path)?;
    let mut header = [0_u8; 8192];
    let header_length = file.read(&mut header)?;
    file.seek(SeekFrom::Start(0))?;
    let (mime, kind) = detect_media_type(&media.path, &header[..header_length]);
    let download_name = escape_html(&media.download_name);

    match kind {
        MediaKind::Image => write!(
            writer,
            r#"<img class="chat-image" loading="lazy" alt="{}" src="data:{};base64,"#,
            download_name, mime
        )?,
        MediaKind::Video => write!(
            writer,
            r#"<video class="chat-video" controls preload="metadata"><source type="{}" src="data:{};base64,"#,
            mime, mime
        )?,
        MediaKind::Audio => write!(
            writer,
            r#"<audio class="chat-audio" controls preload="metadata" src="data:{};base64,"#,
            mime
        )?,
        MediaKind::File => write!(
            writer,
            r#"<a class="chat-file" download="{}" href="data:{};base64,"#,
            download_name, mime
        )?,
    }

    {
        let mut encoder = EncoderWriter::new(&mut *writer, &B64);
        let mut reader = CancellableReader {
            inner: &mut file,
            cancel,
        };
        std::io::copy(&mut reader, &mut encoder)?;
        let _ = encoder.finish()?;
    }

    match kind {
        MediaKind::Image => writer.write_all(b"\">")?,
        MediaKind::Video => writer.write_all(b"\"></video>")?,
        MediaKind::Audio => writer.write_all(b"\"></audio>")?,
        MediaKind::File => write!(writer, "\">📎 {}</a>", download_name)?,
    }
    Ok(())
}

fn ensure_not_cancelled(cancel: &AtomicBool) -> std::io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            crate::dws::CANCELLED_ERROR,
        ))
    } else {
        Ok(())
    }
}

struct CancellableReader<'a, R> {
    inner: &'a mut R,
    cancel: &'a AtomicBool,
}

impl<R: Read> Read for CancellableReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        ensure_not_cancelled(self.cancel)?;
        self.inner.read(buffer)
    }
}

fn detect_media_type(path: &Path, header: &[u8]) -> (String, MediaKind) {
    if let Some(kind) = infer::get(header) {
        let mime = kind.mime_type().to_string();
        return (mime.clone(), kind_from_mime(&mime));
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string();
    (mime.clone(), kind_from_mime(&mime))
}

fn kind_from_mime(mime: &str) -> MediaKind {
    if mime.starts_with("image/") {
        MediaKind::Image
    } else if mime.starts_with("video/") {
        MediaKind::Video
    } else if mime.starts_with("audio/") {
        MediaKind::Audio
    } else {
        MediaKind::File
    }
}

fn build_media_map(
    attachments_dir: &Path,
    index_path: &Path,
) -> Result<HashMap<String, Vec<MediaFile>>, String> {
    let mut map: HashMap<String, Vec<MediaFile>> = HashMap::new();
    if !index_path.exists() {
        return Ok(map);
    }
    let index_text = fs::read_to_string(index_path)
        .map_err(|error| format!("读取 {} 失败: {}", index_path.display(), error))?;
    let records: Vec<AttachmentRecord> = serde_json::from_str(&index_text)
        .map_err(|error| format!("解析 {} 失败: {}", index_path.display(), error))?;

    for record in records {
        if record.status != "ok" {
            continue;
        }
        let source_path = Path::new(&record.file);
        let file_name = source_path
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| format!("附件索引包含无效文件名: {}", record.file))?;
        // 使用相对路径（可能包含年月子目录）
        let path = attachments_dir.join(&record.file);
        if !path.is_file() {
            return Err(format!(
                "附件索引标记成功，但文件不存在: {}",
                path.display()
            ));
        }
        let download_name = record
            .original_file_name
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| file_name.to_string_lossy().into_owned());
        map.entry(record.open_message_id)
            .or_default()
            .push(MediaFile {
                path,
                download_name,
            });
    }
    for files in map.values_mut() {
        files.sort_by(|left, right| left.path.cmp(&right.path));
    }
    Ok(map)
}

fn color_for(name: &str) -> &'static str {
    let hash: usize = crate::stable_hash(name) as usize;
    AVATAR_COLORS[hash % AVATAR_COLORS.len()]
}

fn initial(name: &str) -> String {
    name.chars()
        .next()
        .map(|character| character.to_string())
        .unwrap_or_else(|| "?".into())
}

fn escape_html(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            _ => output.push(character),
        }
    }
    output
}

/// 对原始文本一次性完成 URL 识别和 HTML 转义。
fn linkify(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        let rest = &text[index..];
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let url: String = rest
                .chars()
                .take_while(|character| {
                    !character.is_whitespace()
                        && !matches!(character, '<' | '>' | '"' | '\'')
                        && !('\u{4e00}'..='\u{9fff}').contains(character)
                })
                .collect();
            if url.len() > 8 {
                let escaped = escape_html(&url);
                write!(
                    output,
                    r#"<a href="{}" target="_blank" rel="noopener noreferrer">{}</a>"#,
                    escaped, escaped
                )
                .expect("writing to String cannot fail");
                for _ in url.chars().skip(1) {
                    let _ = characters.next();
                }
                continue;
            }
        }
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            _ => output.push(character),
        }
    }
    output
}

fn weekday(year: i32, month: u32, day: u32) -> usize {
    let (mut year, mut month) = (year, month);
    if month < 3 {
        year -= 1;
        month += 12;
    }
    let century = year / 100;
    let year_of_century = year % 100;
    let weekday = (day as i32
        + 13 * (month as i32 + 1) / 5
        + year_of_century
        + year_of_century / 4
        + century / 4
        + 5 * century)
        % 7;
    ((weekday + 5) % 7) as usize
}

fn current_date() -> (i32, u32, u32) {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 8 * 3600;
    crate::date::epoch_days_to_ymd((seconds / 86_400) as i64)
}

fn ymd_to_epoch_days(year: i32, month: u32, day: u32) -> i64 {
    let (year, month, day) = (year as i64, month as i64, day as i64);
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 {
        year / 400
    } else {
        (year - 399) / 400
    };
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn shift_day(date: (i32, u32, u32), delta: i32) -> (i32, u32, u32) {
    crate::date::epoch_days_to_ymd(ymd_to_epoch_days(date.0, date.1, date.2) + delta as i64)
}

fn date_label(year: i32, month: u32, day: u32) -> String {
    date_label_for(current_date(), (year, month, day))
}

fn date_label_for(today: (i32, u32, u32), date: (i32, u32, u32)) -> String {
    if date == today {
        return "今天".into();
    }
    if date == shift_day(today, -1) {
        return "昨天".into();
    }
    format!(
        "{}年{}月{}日 {}",
        date.0,
        date.1,
        date.2,
        WEEKDAYS[weekday(date.0, date.1, date.2)]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linkify_escapes_text_and_query_once() {
        let output = linkify("a < b https://example.test/?a=1&b=2");
        assert!(output.starts_with("a &lt; b "));
        assert!(output.contains(r#"href="https://example.test/?a=1&amp;b=2""#));
        assert!(!output.contains("&amp;amp;"));
    }

    #[test]
    fn media_markup_removal_keeps_caption_and_notice() {
        let output = crate::media::strip_media_markup(
            "[图片消息](mediaId=abc) 请查看\n注意：这是用户写的说明",
        );
        assert_eq!(output, "请查看\n注意：这是用户写的说明");
    }

    #[test]
    fn yesterday_is_calculated_from_today() {
        assert_eq!(date_label_for((2026, 7, 25), (2026, 7, 24)), "昨天");
        assert_ne!(date_label_for((2026, 7, 25), (2026, 7, 26)), "昨天");
        assert!(date_label_for((2000, 1, 1), (2026, 7, 26)).ends_with("星期日"));
    }

    #[test]
    fn invalid_date_is_rejected() {
        assert!(crate::date::parse_dws_datetime("2026-99-01 10:00:00").is_none());
        assert!(crate::date::parse_dws_datetime("2026-02-29 10:00:00").is_none());
        assert!(crate::date::parse_dws_datetime("2024-02-29 10:00:00").is_some());
        assert!(crate::date::parse_dws_datetime("not-a-date").is_none());
    }

    #[test]
    fn empty_export_produces_a_valid_document() {
        let safe_thread_name = std::thread::current()
            .name()
            .unwrap_or("test")
            .replace("::", "-")
            .replace(":", "-");
        let output = std::env::temp_dir().join(format!(
            "dingtalk-chat-exporter-empty-{}-{}.html",
            std::process::id(),
            safe_thread_name
        ));
        let attachments = output.with_extension("attachments");
        let cancel = AtomicBool::new(false);
        generate_html(
            &[],
            "空群",
            &attachments,
            &attachments.with_extension("attachments_index.json"),
            "",
            &output,
            &cancel,
        )
        .unwrap();
        let html = fs::read_to_string(&output).unwrap();
        assert!(html.contains("0</b> 条消息"));
        assert!(html.trim_end().ends_with("</html>"));
        fs::remove_file(output).unwrap();
    }

    #[test]
    fn audio_element_has_a_required_end_tag() {
        let path = std::env::temp_dir().join(format!(
            "dingtalk-chat-exporter-audio-test-{}.mp3",
            std::process::id()
        ));
        fs::write(&path, b"not-real-audio").unwrap();
        let media = MediaFile {
            path: path.clone(),
            download_name: "recording.mp3".into(),
        };
        let mut output = Vec::new();
        let cancel = AtomicBool::new(false);
        write_media_file(&mut output, &media, &cancel).unwrap();
        let html = String::from_utf8(output).unwrap();
        assert!(html.ends_with("</audio>"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn media_map_uses_only_successful_existing_index_entries() {
        let root = std::env::temp_dir().join(format!(
            "dingtalk-chat-exporter-media-map-test-{}",
            std::process::id()
        ));
        let attachments = root.join("attachments");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&attachments).unwrap();
        fs::write(attachments.join("ok.bin"), b"attachment").unwrap();
        fs::write(
            root.join("attachments_index.json"),
            r#"[
              {"openMessageId":"msg-1","file":"ok.bin","status":"ok","originalFileName":"报告(终版).pdf"},
              {"openMessageId":"msg-1","file":"failed.bin","status":"fail"}
            ]"#,
        )
        .unwrap();

        let map = build_media_map(&attachments, &root.join("attachments_index.json")).unwrap();
        let files = map.get("msg-1").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].download_name, "报告(终版).pdf");
        assert_eq!(files[0].path, attachments.join("ok.bin"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn html_generation_stops_before_writing_when_cancelled() {
        let output = std::env::temp_dir().join(format!(
            "dingtalk-chat-exporter-cancel-test-{}.html",
            std::process::id()
        ));
        let cancel = AtomicBool::new(true);
        let error = generate_html(
            &[],
            "取消群",
            Path::new("missing"),
            Path::new("missing/attachments_index.json"),
            "",
            &output,
            &cancel,
        )
        .err()
        .unwrap();
        assert_eq!(error, crate::dws::CANCELLED_ERROR);
        assert!(!output.exists());
    }
}
