//! 会话文件定位：按配置的模式找到会话对应的文件，为文件条件提供事实。
//!
//! 只有配置了定位规则的 agent 才有文件事实；定位不到时相关规则判为不匹配，
//! 不会因为「拿不到信息」而意外放行。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::policy::FileFacts;

/// 会话 ID 里出现这些字符时不做 glob 匹配，避免把 ID 当成模式。
const GLOB_META: [char; 5] = ['*', '?', '[', ']', '{'];

/// 按模式列表查找会话文件：`{id}` 替换成会话 ID，`~` 展开成家目录。
#[must_use]
pub fn find_file(patterns: &[String], session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains(GLOB_META) {
        return None;
    }
    for pattern in patterns {
        let expanded = expand(pattern, session_id);
        let Ok(expanded) = expanded else {
            continue;
        };
        let Ok(entries) = glob::glob(&expanded) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.is_file() {
                return Some(entry);
            }
        }
    }
    None
}

fn expand(pattern: &str, session_id: &str) -> Result<String, ()> {
    let with_id = pattern.replace("{id}", session_id);
    let home = crate::config::home_dir().ok_or(())?;
    if let Some(rest) = with_id.strip_prefix('~') {
        let rest = rest.trim_start_matches('/');
        if rest.is_empty() {
            return Ok(home.to_string_lossy().into_owned());
        }
        return Ok(home.join(rest).to_string_lossy().into_owned());
    }
    Ok(with_id)
}

/// 读取文件事实；读不到时返回 None。
#[must_use]
pub fn file_facts(path: &Path) -> Option<FileFacts> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some(FileFacts {
        path: path.to_string_lossy().into_owned(),
        size: metadata.len(),
        btime: metadata.created().ok().map(to_utc),
        mtime: metadata.modified().ok().map(to_utc),
        mode: metadata.mode() & 0o7777,
        owner_uid: metadata.uid(),
        group_gid: metadata.gid(),
    })
}

fn to_utc(value: std::time::SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 按会话_id_定位文件() {
        let dir = std::env::temp_dir().join(format!("acp-view-locate-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        let file = dir.join("nested").join("2026-09-27T00-00-00_abc123.jsonl");
        std::fs::write(&file, "{}").unwrap();

        let patterns = vec![format!("{}/**/*{{id}}*.jsonl", dir.to_string_lossy())];
        assert_eq!(find_file(&patterns, "abc123"), Some(file.clone()));
        assert_eq!(find_file(&patterns, "不存在的会话"), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 带_glob_元字符的_id_不做匹配() {
        let patterns = vec!["/tmp/*{id}*".to_string()];
        assert_eq!(find_file(&patterns, "a*b"), None);
    }

    #[test]
    fn 读取文件事实() {
        let path = std::env::temp_dir().join(format!("acp-view-facts-{}.txt", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        let facts = file_facts(&path).expect("应当能读到文件事实");
        assert_eq!(facts.size, 5);
        assert!(facts.mtime.is_some());
        assert_eq!(facts.owner_uid, current_uid());
        assert_eq!(facts.mode & !0o777, 0, "普通文件的权限位不应包含特殊位");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn 不存在的文件返回_none() {
        assert!(file_facts(Path::new("/不存在的文件")).is_none());
    }

    fn current_uid() -> u32 {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(".").unwrap().uid()
    }
}
