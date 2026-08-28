use std::ops::Range;

const MEDIA_TAGS: [&str; 5] = [
    "[图片消息]",
    "[视频消息]",
    "[文件消息]",
    "[语音消息]",
    "[音频消息]",
];

fn markup_ranges(content: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    while offset < content.len() {
        let Some((relative_start, tag)) = MEDIA_TAGS
            .iter()
            .filter_map(|tag| content[offset..].find(tag).map(|position| (position, *tag)))
            .min_by_key(|(position, _)| *position)
        else {
            break;
        };
        let start = offset + relative_start;
        let metadata_start = start + tag.len();
        if content.as_bytes().get(metadata_start) != Some(&b'(') {
            offset = metadata_start;
            continue;
        }

        let metadata = &content[metadata_start + 1..];
        let anchor = metadata
            .find("mediaId=")
            .or_else(|| metadata.find("fileName="));
        let Some(anchor) = anchor else {
            offset = metadata_start + 1;
            continue;
        };
        let Some(close) = metadata[anchor..].find(')') else {
            break;
        };
        let end = metadata_start + 1 + anchor + close + 1;
        ranges.push(start..end);
        offset = end;
    }
    ranges
}

pub(crate) fn extract_media_ids(content: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for range in markup_ranges(content) {
        let mut search = &content[range];
        while let Some(position) = search.find("mediaId=") {
            let after = &search[position + "mediaId=".len()..];
            let end = after
                .find(|character: char| {
                    character == ')'
                        || character == ','
                        || character == ';'
                        || character == ']'
                        || character.is_whitespace()
                })
                .unwrap_or(after.len());
            let id = after[..end].trim_matches(['"', '\'']);
            if !id.is_empty() && !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
            search = &after[end..];
        }
    }
    ids
}

pub(crate) fn extract_original_file_name(content: &str) -> Option<String> {
    for range in markup_ranges(content) {
        let metadata = &content[range];
        let Some(position) = metadata.find("fileName=") else {
            continue;
        };
        let after = &metadata[position + "fileName=".len()..];
        let end = [",mediaId=", ", mediaId=", ";mediaId=", "; mediaId="]
            .iter()
            .filter_map(|delimiter| after.find(delimiter))
            .min()
            .unwrap_or_else(|| after.rfind(')').unwrap_or(after.len()));
        let name = after[..end].trim().trim_matches(['"', '\'']);
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

pub(crate) fn strip_media_markup(content: &str) -> String {
    let mut text = content.to_string();
    for range in markup_ranges(content).into_iter().rev() {
        text.replace_range(range, "");
    }
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_parentheses_do_not_end_metadata_early() {
        let content = "[文件消息](fileName=报告(终版).pdf, mediaId=abc==) 请查收";
        assert_eq!(
            extract_original_file_name(content).as_deref(),
            Some("报告(终版).pdf")
        );
        assert_eq!(extract_media_ids(content), vec!["abc=="]);
        assert_eq!(strip_media_markup(content), "请查收");
    }

    #[test]
    fn ordinary_text_cannot_trigger_a_media_download() {
        assert!(extract_media_ids("用户输入 mediaId=not-an-attachment").is_empty());
    }
}
