//! 目录更新订阅的系统通知：检测到服务器有新版本时弹一次 Windows 原生 toast。
//!
//! * 同一版本只通知一次：每个目录记录「上次已通知的远端版本号」
//!   （`DirConfig::notified_rev`，持久化，跨重启有效），与本次检测结果相同就跳过；
//! * 多个目录合并：同一帧内到齐的多条检测结果先收集进 `RepoApp::notify_queue`，
//!   帧末统一发一条 toast，不逐目录连环弹。
//!
//! toast 走 WinRT（`tauri-winrt-notification`），免打包借用 PowerShell 的应用身份；
//! 非 Windows 平台整套为空操作（本项目只在 Windows 上跑，但不让编译塌掉）。

/// 一条待通知的目录更新：label 进正文，rev / author / behind 是通知内容三要素
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateNotice {
    /// 目录显示名（别名，空则用路径）
    pub label: String,
    /// 远端最新版本号（SVN revision / Git 提交 SHA）
    pub rev: String,
    /// 远端最新提交人
    pub author: String,
    /// 未更新项数（svn status -u 的 out-of-date 计数 / git 的落后提交文件数）
    pub behind: usize,
}

/// Git 的 40 位 SHA 通知里太占地方：长版本号缩到前 8 位，SVN 的 revision 原样显示
pub fn short_rev(rev: &str) -> String {
    if rev.len() >= 40 {
        format!("{}…", &rev[..8])
    } else {
        rev.to_owned()
    }
}

/// toast 标题：单个目录点名，多个目录报数量
pub fn toast_title(items: &[UpdateNotice]) -> String {
    match items.len() {
        1 => format!("「{}」有新版本", items[0].label),
        n => format!("检测到 {n} 个目录有新版本"),
    }
}

/// toast 正文：每个目录一行（最新版本、最新提交人、未更新项数）
pub fn toast_lines(items: &[UpdateNotice]) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            format!(
                "{}：最新版本 {}，提交人 {}，未更新 {} 项",
                item.label,
                short_rev(&item.rev),
                if item.author.is_empty() { "未知" } else { &item.author },
                item.behind,
            )
        })
        .collect()
}

/// 自己的应用身份（AUMID）：启动时往 `HKCU\Software\Classes\AppUserModelId` 注册
/// 显示名与图标（见 [`ensure_identity`]），通知中心就显示「仓库管理器」和兔子图标，
/// 而不是未打包程序借用 PowerShell 身份时的「Windows PowerShell」。
#[cfg(windows)]
pub const APP_ID: &str = "RepoManager";

/// exe 里嵌的那枚兔子图标（build.rs 用它给 exe 上图标，这里同源内嵌一份给通知用）
#[cfg(windows)]
const ICON: &[u8] = include_bytes!("../icon/repo-manager.ico");

/// 启动时调一次：把内嵌图标释放到配置目录，再往注册表写 AUMID 的显示名与图标。
/// 写失败不打断启动——顶多通知弹出来没有图标（发送错误会进输出记录）
#[cfg(windows)]
pub fn ensure_identity() {
    let Some(icon_path) = write_icon_file() else { return };
    imp::register_app_id(APP_ID, crate::APP_TITLE, &icon_path.to_string_lossy());
}

/// 图标落盘到 `%APPDATA%\RepoManager\toast.ico`：IconUri 只认文件路径，吃不到 exe 资源。
/// 内容没变不重写（省一次磁盘写，也避免杀软盯着配置目录的频繁写入）
#[cfg(windows)]
fn write_icon_file() -> Option<std::path::PathBuf> {
    let dir = crate::config::config_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("toast.ico");
    let unchanged = std::fs::read(&path)
        .map(|old| old == ICON)
        .unwrap_or(false);
    if !unchanged {
        std::fs::write(&path, ICON).ok()?;
    }
    Some(path)
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
        REG_SZ,
    };

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    fn set_value(hkey: HKEY, name: &str, data: &str) {
        let value = wide(data);
        unsafe {
            RegSetValueExW(
                hkey,
                wide(name).as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            )
        };
    }

    /// 往 `HKCU\Software\Classes\AppUserModelId\<app_id>` 写 DisplayName 与 IconUri。
    /// 只动 HKCU，不需要管理员权限；每次启动都重写一遍（幂等，还能跟随图标更新）
    pub fn register_app_id(app_id: &str, display_name: &str, icon_uri: &str) {
        let subkey = wide(&format!(r"Software\Classes\AppUserModelId\{app_id}"));
        let mut hkey: HKEY = std::ptr::null_mut();
        let mut created: u32 = 0;
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_SET_VALUE,
                std::ptr::null(),
                &mut hkey,
                &mut created,
            )
        };
        if rc != ERROR_SUCCESS {
            return;
        }
        set_value(hkey, "DisplayName", display_name);
        set_value(hkey, "IconUri", icon_uri);
        unsafe { RegCloseKey(hkey) };
    }
}

/// 发送一条合并后的更新通知。返回 Err 时调用方写输出记录（弹不出来不该无声失败）。
#[cfg(windows)]
pub fn send_update_toast(items: &[UpdateNotice]) -> Result<(), String> {
    use tauri_winrt_notification::Toast;
    let lines = toast_lines(items);
    let mut toast = Toast::new(APP_ID)
        .title(&toast_title(items))
        .text1(&lines[0]);
    if lines.len() > 1 {
        toast = toast.text2(&lines[1..].join("；"));
    }
    toast.show().map_err(|e| e.to_string())
}

#[cfg(not(windows))]
pub fn send_update_toast(_items: &[UpdateNotice]) -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
pub fn ensure_identity() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(label: &str, rev: &str, author: &str, behind: usize) -> UpdateNotice {
        UpdateNotice {
            label: label.to_owned(),
            rev: rev.to_owned(),
            author: author.to_owned(),
            behind,
        }
    }

    /// 单目录：标题点名目录，正文带齐版本 / 提交人 / 未更新数三要素
    #[test]
    fn single_dir_names_the_dir_and_lists_all_three_facts() {
        let items = [sample("HRP", "12345", "张三", 7)];
        assert_eq!(toast_title(&items), "「HRP」有新版本");
        assert_eq!(
            toast_lines(&items),
            vec!["HRP：最新版本 12345，提交人 张三，未更新 7 项".to_owned()]
        );
    }

    /// 多目录：合并成一条，标题报数量，每个目录各占一行
    #[test]
    fn multiple_dirs_merge_into_one_notice() {
        let items = [
            sample("HRP", "12345", "张三", 7),
            sample("vue_ss", "abcdef1234567890abcdef1234567890abcdef12", "李四", 2),
        ];
        assert_eq!(toast_title(&items), "检测到 2 个目录有新版本");
        let lines = toast_lines(&items);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("HRP"), "第一行该是 HRP：{lines:?}");
        assert!(
            lines[1].contains("abcdef12…"),
            "Git 长 SHA 该缩写：{lines:?}"
        );
    }

    /// 提交人拿不到时不写空串，显示「未知」
    #[test]
    fn missing_author_shows_unknown() {
        let items = [sample("HRP", "9", "", 1)];
        assert!(toast_lines(&items)[0].contains("提交人 未知"));
    }
}
