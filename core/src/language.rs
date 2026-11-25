//! The language the model replies in when the person has not asked for
//! one: the `language` of `settings.json` beside the database, else the
//! operating system's display language. It is never guessed from what the
//! person writes (docs/evidence/reply-language.md).

use std::path::Path;

/// The default reply language, as written in the settings or as the
/// system's language tag (such as `ja-JP`); `None` when neither names one.
pub(crate) async fn default(settings: &Path) -> Option<String> {
    match configured(settings).await {
        Ok(Some(language)) => Some(language),
        Ok(None) => system(),
        Err(error) => {
            eprintln!("inocode: {}: {error}", settings.display());
            system()
        }
    }
}

/// The instruction that names `language` and lets the person change it.
pub(crate) fn instruction(language: &str) -> String {
    format!(
        "Write your replies to the person in {language}. If the person asks for \
         another language, use the language they asked for most recently instead."
    )
}

async fn configured(settings: &Path) -> Result<Option<String>, String> {
    let text = match tokio::fs::read_to_string(settings).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| error.to_string())?;
    match value.get("language") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(language)) => {
            let language = language.trim();
            Ok((!language.is_empty()).then(|| language.to_string()))
        }
        Some(_) => Err("\"language\" is not a string".to_string()),
    }
}

/// The first of the person's display languages.
#[cfg(windows)]
pub(crate) fn system() -> Option<String> {
    use windows_sys::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
    let mut count = 0u32;
    let mut length = 0u32;
    // SAFETY: a null buffer asks only for the length it needs.
    let sized = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            std::ptr::null_mut(),
            &mut length,
        )
    };
    if sized == 0 || length == 0 {
        return None;
    }
    let mut buffer = vec![0u16; length as usize];
    // SAFETY: the buffer holds the `length` characters asked for above.
    let filled = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            buffer.as_mut_ptr(),
            &mut length,
        )
    };
    if filled == 0 {
        return None;
    }
    // The buffer is a list of names, each ended by a NUL.
    let first = buffer.split(|&unit| unit == 0).next()?;
    let name = String::from_utf16(first).ok()?;
    (!name.is_empty()).then_some(name)
}

/// The locale the system's messages use: `LC_ALL`, then `LC_MESSAGES`,
/// then `LANG`, the order POSIX gives them.
#[cfg(not(windows))]
pub(crate) fn system() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
        .and_then(|value| tag(&value))
}

/// The language tag of a POSIX locale name such as `ja_JP.UTF-8`; `None`
/// for the `C` and `POSIX` locales, which name no language.
#[cfg_attr(windows, allow(dead_code))]
fn tag(locale: &str) -> Option<String> {
    let name = locale.split(['.', '@']).next()?;
    if name.is_empty() || name == "C" || name == "POSIX" {
        return None;
    }
    Some(name.replace('_', "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_names_become_language_tags() {
        assert_eq!(tag("ja_JP.UTF-8").as_deref(), Some("ja-JP"));
        assert_eq!(tag("en_US").as_deref(), Some("en-US"));
        assert_eq!(tag("de_DE@euro").as_deref(), Some("de-DE"));
        assert_eq!(tag("ko").as_deref(), Some("ko"));
        assert_eq!(tag("C.UTF-8"), None);
        assert_eq!(tag("POSIX"), None);
        assert_eq!(tag(""), None);
    }

    #[tokio::test]
    async fn the_settings_language_comes_before_the_system_one() {
        let folder = std::env::temp_dir().join(format!("inocode-language-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let settings = folder.join("settings.json");

        std::fs::write(&settings, r#"{"language": " Japanese "}"#).unwrap();
        assert_eq!(default(&settings).await.as_deref(), Some("Japanese"));

        std::fs::write(&settings, r#"{"language": ""}"#).unwrap();
        assert_eq!(configured(&settings).await, Ok(None));
        std::fs::write(&settings, "{}").unwrap();
        assert_eq!(configured(&settings).await, Ok(None));
        std::fs::write(&settings, r#"{"language": 3}"#).unwrap();
        assert!(configured(&settings).await.is_err());
        std::fs::write(&settings, "not json").unwrap();
        assert!(configured(&settings).await.is_err());
        assert_eq!(default(&settings).await, system());

        std::fs::remove_file(&settings).unwrap();
        assert_eq!(configured(&settings).await, Ok(None));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn the_instruction_names_the_language_and_yields_to_the_person() {
        let text = instruction("ja-JP");
        assert!(text.contains("in ja-JP"));
        assert!(text.contains("asked for most recently"));
    }
}
