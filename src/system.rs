//! 系统账号查询：把 uid 转成用户名、把用户名展开成组列表。
//!
//! 只在两个地方使用：解析主体身份（配置里没写组时查系统）与把会话文件的属主 uid
//! 转成名字。这两处都发生在启动或首次见到会话时，调用频率很低。

use std::ffi::{CStr, CString};

/// `getgrouplist` 的 gid 参数在 macOS 上是 c_int，在 Linux 上是 gid_t。
#[cfg(target_os = "macos")]
type GidArg = libc::c_int;
#[cfg(not(target_os = "macos"))]
type GidArg = libc::gid_t;

/// 当前进程的所属用户。
#[must_use]
pub fn current_uid() -> u32 {
    // SAFETY: getuid 没有参数也不会失败。
    unsafe { libc::getuid() }
}

/// 把 uid 转成用户名。
#[must_use]
pub fn user_name(uid: u32) -> Option<String> {
    let mut buffer = vec![0 as libc::c_char; 4096];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: 传入的都是本函数持有的缓冲区，getpwuid_r 会写入它们。
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if code != 0 || result.is_null() || entry.pw_name.is_null() {
        return None;
    }
    // SAFETY: pw_name 是 getpwuid_r 写入缓冲区里的以 NUL 结尾的字符串。
    Some(unsafe { CStr::from_ptr(entry.pw_name) }.to_string_lossy().into_owned())
}

/// 展开用户名所属的全部组（含主组）。
#[must_use]
pub fn user_groups(name: &str) -> Option<Vec<String>> {
    let cname = CString::new(name).ok()?;
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: 同上，缓冲区由本函数持有。
    let code = unsafe {
        libc::getpwnam_r(
            cname.as_ptr(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if code != 0 || result.is_null() {
        return None;
    }
    let primary_gid = entry.pw_gid as GidArg;

    // macOS 的 getgrouplist 在缓冲不足时不保证回填所需数量，因此这里从较大的缓冲起步、
    // 失败就把容量翻倍重试。
    let mut capacity: libc::c_int = 64;
    let mut gids: Vec<GidArg> = Vec::new();
    let mut filled: libc::c_int = 0;
    let mut succeeded = false;
    for _ in 0..8 {
        let mut buffer = vec![0 as GidArg; capacity as usize];
        let mut needed = capacity;
        // SAFETY: buffer 由本函数持有，容量通过 needed 传入。
        let code = unsafe { libc::getgrouplist(cname.as_ptr(), primary_gid, buffer.as_mut_ptr(), &mut needed) };
        if code == 0 {
            buffer.truncate(needed.max(0) as usize);
            gids = buffer;
            filled = needed;
            succeeded = true;
            break;
        }
        capacity = if needed > capacity { needed + 8 } else { capacity * 2 };
        if capacity > 8192 {
            break;
        }
    }
    if !succeeded {
        return None;
    }
    let _ = filled;

    let mut names = Vec::new();
    for gid in gids {
        // SAFETY: getgrgid 返回指向静态数据的指针，只在本进程内读取，之后立刻拷贝。
        let group = unsafe { libc::getgrgid(gid as libc::gid_t) };
        if group.is_null() {
            continue;
        }
        // SAFETY: gr_name 是以 NUL 结尾的字符串。
        let name = unsafe { CStr::from_ptr((*group).gr_name) }.to_string_lossy().into_owned();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.sort();
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 当前_uid_能解析出用户名() {
        let uid = current_uid();
        let name = user_name(uid).expect("当前用户应当能解析出名字");
        assert!(!name.is_empty());
        // 与环境变量里的用户名应当一致（容器里可能不同，因此只在两者都有时比较）。
        if let Ok(expected) = std::env::var("USER") {
            assert_eq!(name, expected);
        }
    }

    #[test]
    fn 未知_uid_返回_none() {
        assert!(user_name(u32::MAX).is_none());
    }

    #[test]
    fn 当前用户的组可以展开() {
        let name = user_name(current_uid()).unwrap();
        let groups = user_groups(&name).expect("当前用户应当能查出组");
        assert!(!groups.is_empty(), "至少包含主组");
    }

    #[test]
    fn 不存在的用户返回_none() {
        assert!(user_groups("这个用户不存在·acp-view").is_none());
    }
}
