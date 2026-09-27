//! Linux 风格的会话权限：主体 × 属主/属组/模式位。
//!
//! 每个会话对象带三个属性：
//!
//! * `owner`：属主名字；
//! * `group`：属组名字（可为空，表示没有属组）；
//! * `mode`：三位八进制权限，依次是属主位、属组位、其他人位。
//!
//! 三个位在本工具里的语义（只读工具没有「写内容」这回事）：
//!
//! | 位 | 值 | 含义 |
//! | --- | --- | --- |
//! | `r` | 4 | 读取会话内容（重放事件） |
//! | `w` | 2 | 修改该会话的权限（chmod / chown） |
//! | `x` | 1 | 在会话列表里可见 |
//!
//! 判定时先看请求者是不是属主，是就用属主位；否则看请求者的组与属组是否有交集，
//! 有交集就用属组位；都不是就用其他人位。

use std::collections::BTreeSet;

/// 请求者身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
    pub groups: BTreeSet<String>,
}

impl Principal {
    #[must_use]
    pub fn new(name: &str, groups: impl IntoIterator<Item = String>) -> Self {
        Self {
            name: name.to_string(),
            groups: groups.into_iter().collect(),
        }
    }
}

/// 会话对象的权限属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectAcl {
    pub owner: String,
    pub group: Option<String>,
    pub mode: u16,
}

impl ObjectAcl {
    #[must_use]
    pub fn new(owner: &str, group: Option<&str>, mode: u16) -> Self {
        Self {
            owner: owner.to_string(),
            group: group.map(str::to_string),
            mode,
        }
    }

    /// 渲染成 `rwxr-x---` 这种形式。
    #[must_use]
    pub fn mode_text(&self) -> String {
        render_mode(self.mode)
    }
}

/// 需要的访问类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// 读取会话内容。
    Read,
    /// 修改会话权限。
    WriteAcl,
    /// 在列表里可见。
    List,
}

impl Access {
    #[must_use]
    fn bit(self) -> u16 {
        match self {
            Access::Read => 0o4,
            Access::WriteAcl => 0o2,
            Access::List => 0o1,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::WriteAcl => "write-acl",
            Access::List => "list",
        }
    }
}

/// 判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclDecision {
    pub allowed: bool,
    /// 请求者落在哪一类：owner / group / other。
    pub class: &'static str,
    pub reason: String,
}

/// 判定一次访问。
#[must_use]
pub fn check(principal: &Principal, acl: &ObjectAcl, access: Access) -> AclDecision {
    let (class, bits) = if principal.name == acl.owner {
        ("owner", (acl.mode >> 6) & 0o7)
    } else if acl
        .group
        .as_ref()
        .is_some_and(|group| principal.groups.contains(group))
    {
        ("group", (acl.mode >> 3) & 0o7)
    } else {
        ("other", acl.mode & 0o7)
    };
    let needed = access.bit();
    let allowed = bits & needed != 0;
    let reason = if allowed {
        format!(
            "{} 以 {class} 身份获得 {} 权限（模式 {}）",
            principal.name,
            access.as_str(),
            render_mode(acl.mode)
        )
    } else {
        format!(
            "{} 以 {class} 身份缺少 {} 权限（模式 {}）",
            principal.name,
            access.as_str(),
            render_mode(acl.mode)
        )
    };
    AclDecision {
        allowed,
        class,
        reason,
    }
}

/// 把模式渲染成 `rwxr-x---`。
#[must_use]
pub fn render_mode(mode: u16) -> String {
    let mut text = String::with_capacity(9);
    for shift in [6u16, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        text.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        text.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        text.push(if bits & 0o1 != 0 { 'x' } else { '-' });
    }
    text
}

#[derive(Debug, thiserror::Error)]
pub enum AclError {
    #[error("权限写法非法：{0:?}（可用 750、0750 或 rwxr-x---）")]
    BadMode(String),
    #[error("权限里的 x 位没有意义：{0:?}（本工具的位含义是 r=读内容、w=改权限、x=可见）")]
    MisplacedBit(String),
}

/// 解析权限：支持 `750`、`0750`、`rwxr-x---`。
pub fn parse_mode(text: &str) -> Result<u16, AclError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(AclError::BadMode(text.to_string()));
    }
    if trimmed.chars().all(|character| character.is_ascii_digit() || character == '0') {
        let digits = trimmed.strip_prefix("0o").unwrap_or(trimmed);
        let value = u16::from_str_radix(digits, 8).map_err(|_| AclError::BadMode(text.to_string()))?;
        if value > 0o777 {
            return Err(AclError::BadMode(text.to_string()));
        }
        return Ok(value);
    }
    if trimmed.len() != 9 {
        return Err(AclError::BadMode(text.to_string()));
    }
    let mut mode = 0u16;
    for (index, character) in trimmed.chars().enumerate() {
        let group = index / 3;
        let bit = match character {
            'r' => 0o4,
            'w' => 0o2,
            'x' => 0o1,
            '-' => 0,
            _ => return Err(AclError::BadMode(text.to_string())),
        };
        mode |= bit << (6 - 3 * group as u16);
    }
    Ok(mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acl(owner: &str, group: Option<&str>, mode: u16) -> ObjectAcl {
        ObjectAcl::new(owner, group, mode)
    }

    #[test]
    fn 属主按属主位判定() {
        let principal = Principal::new("fun10165", []);
        // 属主位 6 = rw：可读、可改权限，但 x 位为 0，列表里看不到
        let object = acl("fun10165", None, 0o640);
        assert!(check(&principal, &object, Access::Read).allowed);
        assert!(check(&principal, &object, Access::WriteAcl).allowed);
        assert!(!check(&principal, &object, Access::List).allowed, "x 位为 0 时不应放行");
        assert_eq!(check(&principal, &object, Access::Read).class, "owner");
    }

    #[test]
    fn 组内成员按属组位判定() {
        let principal = Principal::new("teammate", ["dev".to_string()]);
        let object = acl("fun10165", Some("dev"), 0o750);
        assert!(check(&principal, &object, Access::Read).allowed);
        assert!(check(&principal, &object, Access::List).allowed);
        assert!(!check(&principal, &object, Access::WriteAcl).allowed, "组位只有 r-x");
        assert_eq!(check(&principal, &object, Access::Read).class, "group");
    }

    #[test]
    fn 其他人按其他人位判定() {
        let principal = Principal::new("stranger", ["其他组".to_string()]);
        let object = acl("fun10165", Some("dev"), 0o754);
        assert!(check(&principal, &object, Access::Read).allowed);
        assert!(!check(&principal, &object, Access::List).allowed);
        assert_eq!(check(&principal, &object, Access::Read).class, "other");
    }

    #[test]
    fn 没有属组时组位不适用() {
        let principal = Principal::new("teammate", ["dev".to_string()]);
        let object = acl("fun10165", None, 0o700);
        let decision = check(&principal, &object, Access::Read);
        assert!(!decision.allowed);
        assert_eq!(decision.class, "other");
    }

    #[test]
    fn 属主身份优先于组身份() {
        let principal = Principal::new("fun10165", ["dev".to_string()]);
        let object = acl("fun10165", Some("dev"), 0o077);
        assert_eq!(check(&principal, &object, Access::List).class, "owner");
        assert!(!check(&principal, &object, Access::List).allowed);
    }

    #[test]
    fn 模式解析与渲染() {
        assert_eq!(parse_mode("750").unwrap(), 0o750);
        assert_eq!(parse_mode("0750").unwrap(), 0o750);
        assert_eq!(parse_mode("rwxr-x---").unwrap(), 0o750);
        assert_eq!(parse_mode("---------").unwrap(), 0);
        assert_eq!(render_mode(0o750), "rwxr-x---");
        assert_eq!(render_mode(0o644), "rw-r--r--");
        assert!(parse_mode("999").is_err());
        assert!(parse_mode("rwxr-x--").is_err());
        assert!(parse_mode("").is_err());
    }

    #[test]
    fn 模式文本可以来回转换() {
        for mode in [0o000u16, 0o400, 0o640, 0o750, 0o755, 0o777] {
            assert_eq!(parse_mode(&render_mode(mode)).unwrap(), mode);
        }
    }
}
