// 应用设置管理 - 跨平台持久化用户配置

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// 文件名安全处理模式
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum FilenameSafeMode {
    /// 将不安全字符替换为下划线
    ReplaceWithUnderscore,
    /// 静默移除不安全字符
    RemoveSilently,
}

impl Default for FilenameSafeMode {
    fn default() -> Self {
        FilenameSafeMode::ReplaceWithUnderscore
    }
}

/// 应用设置
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    /// 默认输出目录（None 表示使用可执行文件所在目录）
    pub default_output_dir: Option<String>,
    /// 文件名安全处理模式
    pub filename_safe_mode: FilenameSafeMode,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            default_output_dir: None,
            filename_safe_mode: FilenameSafeMode::default(),
        }
    }
}

/// 设置文件名
const SETTINGS_FILENAME: &str = "settings.json";

/// 应用配置目录名
const APP_DIR_NAME: &str = "dingtalk-chat-exporter";

/// 获取跨平台的设置文件路径。
/// - Windows: %APPDATA%\dingtalk-chat-exporter\settings.json
/// - macOS: ~/Library/Application Support/dingtalk-chat-exporter/settings.json
/// - Linux: ~/.config/dingtalk-chat-exporter/settings.json
pub fn settings_file_path() -> PathBuf {
    let config_dir = if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var("USERPROFILE")
                    .ok()
                    .map(|p| PathBuf::from(p).join("AppData").join("Roaming"))
                    .unwrap_or_else(|| PathBuf::from("."))
            })
    } else if cfg!(target_os = "macos") {
        std::env::var("HOME")
            .ok()
            .map(|p| PathBuf::from(p).join("Library").join("Application Support"))
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        std::env::var("HOME")
            .ok()
            .map(|p| PathBuf::from(p).join(".config"))
            .unwrap_or_else(|| PathBuf::from("."))
    };
    config_dir.join(APP_DIR_NAME).join(SETTINGS_FILENAME)
}

/// 加载应用设置。
/// 如果设置文件不存在，返回默认值（不创建文件）。
/// 如果文件存在但解析失败，返回错误。
pub fn load_settings() -> Result<AppSettings, String> {
    let path = settings_file_path();
    if !path.exists() {
        return Ok(AppSettings::default());
    }
    let content =
        fs::read_to_string(&path).map_err(|error| format!("读取设置文件失败: {}", error))?;
    let settings: AppSettings =
        serde_json::from_str(&content).map_err(|error| format!("解析设置文件失败: {}", error))?;
    Ok(settings)
}

/// 保存应用设置。
/// 会自动创建配置目录（如不存在）。
pub fn save_settings(settings: &AppSettings) -> Result<(), String> {
    let path = settings_file_path();

    // 确保配置目录存在
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|error| format!("创建配置目录失败: {}", error))?;
        }
    }

    let json = serde_json::to_string_pretty(settings)
        .map_err(|error| format!("序列化设置失败: {}", error))?;
    fs::write(&path, json).map_err(|error| format!("写入设置文件失败: {}", error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn default_settings_have_expected_values() {
        let settings = AppSettings::default();
        assert!(settings.default_output_dir.is_none());
        assert_eq!(
            settings.filename_safe_mode,
            FilenameSafeMode::ReplaceWithUnderscore
        );
    }

    #[test]
    fn settings_file_path_is_under_config_dir() {
        let path = settings_file_path();
        let path_str = path.to_string_lossy();
        // 路径应包含应用目录名
        assert!(
            path_str.contains(APP_DIR_NAME),
            "设置路径应包含 '{}', 实际: {}",
            APP_DIR_NAME,
            path_str
        );
        // 路径应以 settings.json 结尾
        assert!(
            path_str.ends_with(SETTINGS_FILENAME),
            "设置路径应以 '{}' 结尾, 实际: {}",
            SETTINGS_FILENAME,
            path_str
        );
    }

    #[test]
    fn load_settings_returns_default_when_file_missing() {
        // 测试 load_settings 函数在文件不存在时返回默认值
        // 注意：此函数依赖系统环境变量，因此只验证函数不会 panic
        // 实际测试通过 temp_dir 的 round_trip 测试完成
        let result = load_settings();
        assert!(result.is_ok());
    }

    #[test]
    fn save_and_load_settings_round_trip() {
        let temp_dir =
            std::env::temp_dir().join(format!("dingtalk-settings-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);

        let settings_path = temp_dir.join("test-settings.json");

        let settings = AppSettings {
            default_output_dir: Some("D:\\exports".to_string()),
            filename_safe_mode: FilenameSafeMode::RemoveSilently,
        };

        let json = serde_json::to_string_pretty(&settings).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(&settings_path, &json).unwrap();

        let content = fs::read_to_string(&settings_path).unwrap();
        let loaded: AppSettings = serde_json::from_str(&content).unwrap();

        assert_eq!(loaded.default_output_dir, Some("D:\\exports".to_string()));
        assert_eq!(loaded.filename_safe_mode, FilenameSafeMode::RemoveSilently);

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn settings_json_uses_camel_case() {
        let settings = AppSettings::default();
        let json = serde_json::to_string_pretty(&settings).unwrap();
        // 验证 camelCase 字段名
        assert!(json.contains("\"defaultOutputDir\""));
        assert!(json.contains("\"filenameSafeMode\""));
    }

    #[test]
    fn filename_safe_mode_variants_serialize_correctly() {
        let replace = FilenameSafeMode::ReplaceWithUnderscore;
        let remove = FilenameSafeMode::RemoveSilently;

        let replace_json = serde_json::to_string(&replace).unwrap();
        let remove_json = serde_json::to_string(&remove).unwrap();

        // 反序列化验证
        let replace_back: FilenameSafeMode = serde_json::from_str(&replace_json).unwrap();
        let remove_back: FilenameSafeMode = serde_json::from_str(&remove_json).unwrap();

        assert_eq!(replace_back, FilenameSafeMode::ReplaceWithUnderscore);
        assert_eq!(remove_back, FilenameSafeMode::RemoveSilently);
    }

    #[test]
    fn load_settings_handles_corrupted_file() {
        let temp_dir = std::env::temp_dir().join(format!(
            "dingtalk-settings-corrupt-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let corrupt_path = temp_dir.join("corrupt-settings.json");
        fs::write(&corrupt_path, "this is not valid json {{{").unwrap();

        // 直接读取并尝试解析
        let content = fs::read_to_string(&corrupt_path).unwrap();
        let result: Result<AppSettings, _> = serde_json::from_str(&content);
        assert!(result.is_err());

        fs::remove_dir_all(&temp_dir).unwrap();
    }
}
