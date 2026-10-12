//! 仓库类型：列表里的每个目录要么是 SVN 工作副本（有 .svn），要么是 Git 仓库（有 .git）。
//! 后端判定、版本号的显示写法都收在这里，界面层不直接写死 r 前缀。

use std::path::Path;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RepoKind {
    #[default]
    Svn,
    Git,
}

impl RepoKind {
    /// 状态栏 / 提示里的后端名。
    pub fn label(self) -> &'static str {
        match self {
            Self::Svn => "SVN",
            Self::Git => "Git",
        }
    }
}

/// 目录里躺着 .git（目录，或 worktree 场景下指向别处的一个文件）就是 Git；
/// 有 .svn 目录就是 SVN；都没有就不是受版本控制的目录。
pub fn detect(path: &Path) -> Option<RepoKind> {
    if path.join(".git").exists() {
        return Some(RepoKind::Git);
    }
    if path.join(".svn").is_dir() {
        return Some(RepoKind::Svn);
    }
    None
}

/// 版本号的界面写法：svn 是 r1234；git 存的是 40 位 SHA，命令行都用全量，
/// 界面上截前 8 位。空版本（Git 仓库还没打过第一个提交）返回空串。
pub fn fmt_rev(kind: RepoKind, revision: &str) -> String {
    match kind {
        RepoKind::Svn => format!("r{revision}"),
        RepoKind::Git => short_sha(revision),
    }
}

/// git 版本号统一显示前 8 位；svn 版本原样返回。
pub fn short_sha(revision: &str) -> String {
    if revision.chars().count() > 8 {
        revision.chars().take(8).collect()
    } else {
        revision.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_rev_uses_r_prefix_for_svn_and_short_sha_for_git() {
        assert_eq!(fmt_rev(RepoKind::Svn, "120"), "r120");
        assert_eq!(
            fmt_rev(RepoKind::Git, "0123456789abcdef0123456789abcdef01234567"),
            "01234567"
        );
        // 已经是短 SHA 或空版本就原样过
        assert_eq!(fmt_rev(RepoKind::Git, "abc123"), "abc123");
        assert_eq!(fmt_rev(RepoKind::Git, ""), "");
    }
}
