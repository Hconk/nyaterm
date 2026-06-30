//! Shared helpers for remote file system backends: path quoting, permission
//! formatting, and common type definitions.

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};

pub(crate) const SFTP_FILE_TYPE_MASK: u32 = 0o170_000;
pub(crate) const POSIX_MODE_MASK: u32 = 0o7777;

/// Parsed entry from a remote directory listing for the file explorer.
#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub permissions: String,
    pub owner: String,
    pub group: String,
    pub mtime: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileProperties {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub permissions: String,
    pub owner: String,
    pub group: String,
    pub uid: String,
    pub gid: String,
    pub mtime: u64,
    pub atime: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RemoteFileAttributeUpdate {
    pub mode: Option<String>,
    pub owner: Option<String>,
    pub group: Option<String>,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteTextFile {
    pub path: String,
    pub content: String,
    pub size: u64,
    pub mtime: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteRemoteTextResult {
    pub status: String,
    pub mtime: Option<u64>,
    pub size: Option<u64>,
}

impl WriteRemoteTextResult {
    pub fn saved(mtime: u64, size: u64) -> Self {
        Self {
            status: "saved".to_string(),
            mtime: Some(mtime),
            size: Some(size),
        }
    }

    pub fn conflict(mtime: u64, size: u64) -> Self {
        Self {
            status: "conflict".to_string(),
            mtime: Some(mtime),
            size: Some(size),
        }
    }
}

pub(crate) fn ensure_text_bytes(bytes: &[u8], max_bytes: u64) -> AppResult<()> {
    if bytes.len() as u64 > max_bytes {
        return Err(AppError::Config(format!(
            "File is too large to open as text ({} bytes > {} bytes)",
            bytes.len(),
            max_bytes
        )));
    }
    if bytes.contains(&0) {
        return Err(AppError::Config(
            "Binary files are not supported by the built-in editor".to_string(),
        ));
    }
    Ok(())
}

/// POSIX shell-safe quoting: wraps `input` in single quotes and escapes any
/// embedded single-quote characters.  An empty string returns `''`.
pub(crate) fn sh_quote(input: &str) -> String {
    if input.is_empty() {
        return "''".to_string();
    }
    let escaped = input.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

pub(crate) fn remote_dir_listing_path(path: &str) -> String {
    if path == "/" || path.ends_with('/') {
        path.to_string()
    } else {
        format!("{path}/")
    }
}

/// Convert a POSIX permission bitmask to the classic `ls -l` string like `-rwxr-xr-x`.
pub(crate) fn permissions_to_string(mode: u32, type_char: char) -> String {
    let mut s = String::with_capacity(10);

    s.push(type_char);

    s.push(if mode & 0o400 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o200 != 0 { 'w' } else { '-' });
    s.push(match (mode & 0o100 != 0, mode & 0o4000 != 0) {
        (true, true) => 's',
        (false, true) => 'S',
        (true, false) => 'x',
        (false, false) => '-',
    });

    s.push(if mode & 0o040 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o020 != 0 { 'w' } else { '-' });
    s.push(match (mode & 0o010 != 0, mode & 0o2000 != 0) {
        (true, true) => 's',
        (false, true) => 'S',
        (true, false) => 'x',
        (false, false) => '-',
    });

    s.push(if mode & 0o004 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o002 != 0 { 'w' } else { '-' });
    s.push(match (mode & 0o001 != 0, mode & 0o1000 != 0) {
        (true, true) => 't',
        (false, true) => 'T',
        (true, false) => 'x',
        (false, false) => '-',
    });

    s
}

pub(crate) fn type_char_from_mode(mode: u32) -> char {
    match mode & SFTP_FILE_TYPE_MASK {
        0o040_000 => 'd',
        0o120_000 => 'l',
        _ => '-',
    }
}

pub(crate) fn describe_permissions(mode: Option<u32>) -> String {
    match mode {
        Some(mode) => format!(
            "{mode:#06o} ({})",
            permissions_to_string(mode, type_char_from_mode(mode))
        ),
        None => "none".to_string(),
    }
}

pub(crate) fn owner_or_id(owner: &Option<String>, uid: Option<u32>) -> String {
    owner
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| uid.map(|value| value.to_string()))
        .unwrap_or_default()
}

pub(crate) fn group_or_id(group: &Option<String>, gid: Option<u32>) -> String {
    group
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| gid.map(|value| value.to_string()))
        .unwrap_or_default()
}

pub(crate) fn parse_octal_mode(mode: &str) -> crate::error::AppResult<u32> {
    u32::from_str_radix(mode, 8)
        .map_err(|_| crate::error::AppError::Channel(format!("Invalid octal mode: {}", mode)))
}

pub(crate) fn sanitize_download_file_name(name: &str) -> String {
    sanitize_download_file_name_for_platform(name, cfg!(windows))
}

pub(crate) fn append_safe_local_child_path(parent: &str, child_name: &str) -> String {
    std::path::Path::new(parent)
        .join(sanitize_download_file_name(child_name))
        .to_string_lossy()
        .to_string()
}

fn sanitize_download_file_name_for_platform(name: &str, windows: bool) -> String {
    let base = name;
    if base.is_empty() {
        return "download".to_string();
    }

    let mut result = String::new();
    let mut chars = base.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        let is_last = chars.peek().is_none();
        if should_percent_encode_download_char(ch, is_last, windows) {
            percent_encode_char(ch, &mut result);
        } else {
            result.push_str(&base[index..index + ch.len_utf8()]);
        }
    }

    if result.is_empty() {
        result.push_str("download");
    }

    if windows && is_windows_reserved_device_name(&result) {
        let mut chars = result.chars();
        if let Some(first) = chars.next() {
            let mut escaped = String::new();
            percent_encode_char(first, &mut escaped);
            escaped.push_str(chars.as_str());
            result = escaped;
        }
    }

    result
}

fn should_percent_encode_download_char(ch: char, is_last: bool, windows: bool) -> bool {
    ch == '%'
        || ch == '/'
        || ch == '\0'
        || ch.is_control()
        || (windows
            && (matches!(ch, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*')
                || (is_last && matches!(ch, ' ' | '.'))))
}

fn percent_encode_char(ch: char, output: &mut String) {
    let mut bytes = [0u8; 4];
    for byte in ch.encode_utf8(&mut bytes).as_bytes() {
        output.push('%');
        output.push_str(&format!("{byte:02X}"));
    }
}

fn is_windows_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0')
}

#[cfg(test)]
mod tests {
    use super::sanitize_download_file_name_for_platform;

    #[test]
    fn percent_encodes_windows_invalid_characters() {
        assert_eq!(
            sanitize_download_file_name_for_platform("a<b>:c\"d|e?f*.txt", true),
            "a%3Cb%3E%3Ac%22d%7Ce%3Ff%2A.txt"
        );
        assert_eq!(
            sanitize_download_file_name_for_platform("foo\\bar.txt", true),
            "foo%5Cbar.txt"
        );
    }

    #[test]
    fn encodes_windows_reserved_device_names() {
        assert_eq!(
            sanitize_download_file_name_for_platform("CON.txt", true),
            "%43ON.txt"
        );
        assert_eq!(
            sanitize_download_file_name_for_platform("nul", true),
            "%6Eul"
        );
        assert_eq!(
            sanitize_download_file_name_for_platform("LPT1.log", true),
            "%4CPT1.log"
        );
    }

    #[test]
    fn encodes_windows_trailing_dot_and_space() {
        assert_eq!(
            sanitize_download_file_name_for_platform("file. ", true),
            "file.%20"
        );
        assert_eq!(
            sanitize_download_file_name_for_platform("file.", true),
            "file%2E"
        );
    }

    #[test]
    fn preserves_readable_safe_characters() {
        assert_eq!(
            sanitize_download_file_name_for_platform("中文 name-_.txt", true),
            "中文 name-_.txt"
        );
    }

    #[test]
    fn encodes_percent_to_keep_generated_sequences_unambiguous() {
        assert_eq!(
            sanitize_download_file_name_for_platform("100%.txt", true),
            "100%25.txt"
        );
    }

    #[test]
    fn unix_rules_are_minimal() {
        assert_eq!(
            sanitize_download_file_name_for_platform("a:b?c*.txt", false),
            "a:b?c*.txt"
        );
        assert_eq!(
            sanitize_download_file_name_for_platform("100%.txt", false),
            "100%25.txt"
        );
    }
}
