//! 开机自启（仅 Windows）：在 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! 写一个指向当前 exe 的 REG_SZ 值，登录后 Windows 自动拉起本程序；删掉即关闭。
//! 只动 HKCU，不需要管理员权限；显示状态以注册表为准（用户手动删了值也能如实反映），
//! 配置文件里的 `auto_start` 字段只做记录。

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegQueryValueExW, RegSetValueExW,
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
    };

    /// 写入的注册表值名（任务管理器 → 启动应用里显示的名字）
    const VALUE_NAME: &str = "RepoManager";
    /// 改名前用的值名，只用于启动时接管一次（见 `adopt_legacy`）
    const LEGACY_VALUE_NAME: &str = "SVNManager";
    const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// 打开（必要时创建）Run 键，执行 `f` 后负责关闭
    fn with_run_key<T>(sam: u32, f: impl FnOnce(HKEY) -> Result<T, String>) -> Result<T, String> {
        let subkey = wide(RUN_SUBKEY);
        let mut hkey: HKEY = std::ptr::null_mut();
        let mut created: u32 = 0;
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                std::ptr::null(),
                0,
                sam,
                std::ptr::null(),
                &mut hkey,
                &mut created,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("打开注册表 Run 键失败（错误码 {rc}）"));
        }
        let result = f(hkey);
        unsafe { RegCloseKey(hkey) };
        result
    }

    /// Run 键下这个值名存在与否；存在就等于开机自启开着
    fn value_exists(hkey: HKEY, name: &str) -> Result<bool, String> {
        let rc = unsafe {
            RegQueryValueExW(
                hkey,
                wide(name).as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
            Ok(rc == ERROR_SUCCESS)
        } else {
            Err(format!("读取注册表失败（错误码 {rc}）"))
        }
    }

    /// 把自启命令写成「指向当前 exe」：路径整体加引号，放在带空格的目录（如 Program Files）里也能正确拉起
    fn write_value(hkey: HKEY, name: &str, exe: &std::path::Path) -> Result<(), String> {
        let data = wide(&format!("\"{}\"", exe.display()));
        let rc = unsafe {
            RegSetValueExW(
                hkey,
                wide(name).as_ptr(),
                0,
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("写入自启注册表值失败（错误码 {rc}）"))
        }
    }

    pub fn is_enabled() -> bool {
        with_run_key(KEY_QUERY_VALUE, |hkey| value_exists(hkey, VALUE_NAME)).unwrap_or(false)
    }

    /// 机器名从 SVNManager 换成 RepoManager 后第一次启动：把旧值名上的开机自启接到新值名上。
    /// 旧值存在就说明他原本开着自启，删掉旧值再按当前 exe 写新值；旧值不存在就什么都不做，
    /// 所以每次启动都调一次也没有副作用。不接的话任务管理器启动项里会一直挂着一条指向
    /// 已经不存在的 svn_manager.exe 的死值，而设置页会显示成「没开自启」。
    pub fn adopt_legacy() {
        let had_legacy = with_run_key(KEY_QUERY_VALUE, |hkey| value_exists(hkey, LEGACY_VALUE_NAME))
            .unwrap_or(false);
        if !had_legacy {
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let _ = with_run_key(KEY_SET_VALUE, |hkey| {
            let _ = unsafe { RegDeleteValueW(hkey, wide(LEGACY_VALUE_NAME).as_ptr()) };
            write_value(hkey, VALUE_NAME, &exe)
        });
    }

    pub fn set_enabled(enable: bool) -> Result<(), String> {
        if enable {
            let exe = std::env::current_exe().map_err(|e| format!("取程序路径失败：{e}"))?;
            with_run_key(KEY_SET_VALUE, |hkey| write_value(hkey, VALUE_NAME, &exe))
        } else {
            with_run_key(KEY_SET_VALUE, |hkey| {
                let rc = unsafe { RegDeleteValueW(hkey, wide(VALUE_NAME).as_ptr()) };
                match rc {
                    // 值本来就不存在也当成功：目标状态就是「没有」
                    ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
                    _ => Err(format!("删除自启注册表值失败（错误码 {rc}）")),
                }
            })
        }
    }
}

#[cfg(windows)]
pub use imp::{adopt_legacy, is_enabled, set_enabled};

#[cfg(not(windows))]
pub fn adopt_legacy() {}

#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    false
}

#[cfg(not(windows))]
pub fn set_enabled(_enable: bool) -> Result<(), String> {
    Err("开机自启目前只支持 Windows".to_owned())
}

#[cfg(all(test, windows))]
mod tests {
    use super::imp::{is_enabled, set_enabled};

    /// 真机演练注册表开关往返：开 → 查得到，关 → 查不到，最后恢复原状态。
    /// 会真的写/删 HKCU Run 键（不需要管理员权限），默认跳过，
    /// 手动跑：`cargo test -- --ignored autostart_roundtrip`
    #[test]
    #[ignore]
    fn autostart_roundtrip() {
        let original = is_enabled();
        set_enabled(true).unwrap();
        assert!(is_enabled(), "写入 Run 键后 is_enabled 应为真");
        set_enabled(false).unwrap();
        assert!(!is_enabled(), "删除 Run 键后 is_enabled 应为假");
        // 别动用户已有的设置：原来开着就恢复开着
        set_enabled(original).unwrap();
        assert_eq!(is_enabled(), original);
    }
}
