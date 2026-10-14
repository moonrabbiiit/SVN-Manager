//! 自绘标题栏的底层：把系统标题栏从窗口上抹掉，绘制与命中测试都归界面层。
//!
//! Windows 的经典配方（Windows Terminal 同款思路）：
//! * `WM_NCCALCSIZE`：wparam 为真时直接返回 0，客户区扩到整个窗口——系统标题栏
//!   就没了，边框与阴影还是 DWM 的；最大化时窗口四周会伸出屏幕一个边框宽，
//!   这里按边框宽把客户区收一格，内容才不会被裁到屏幕外。
//! * `WM_NCHITTEST`：四周照常给 HT\* 缩放命中（拖边缩放、四角斜向缩放都是原生的），
//!   顶栏「天区」给 `HTCAPTION`——原生拖动、双击最大化、Win+方向键贴边分屏全白拿；
//!   按钮区给 `HTCLIENT`，egui 照常收到点击。
//! * 天区范围（顶栏高度、按钮区左缘）由界面每帧调 [`set_drag_zone`] 写进来，
//!   缩放比（pixels_per_point）一并给出，原生侧换算成物理像素后读同一份原子数据。
//! * `WM_NCACTIVATE`：激活态切换时系统默认会重画标准标题栏（偶现双标题栏的根源），
//!   传 lParam = -1 让它跳过；未公开的 WM_NCUAHDRAWCAPTION/FRAME 一并吞掉。
//!
//! 非 Windows 平台整套是空操作（本项目只在 Windows 上跑，但别让编译塌掉）。

#[cfg(windows)]
static BAR_H: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(48);
#[cfg(windows)]
static BTN_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(i32::MAX);

/// 界面每帧写入：顶栏高度与按钮区左缘（egui 逻辑点）。
/// 画过第一帧之前用默认值——整条顶栏（48 物理 px）都算天区，按钮一出现就被划出去。
#[cfg(windows)]
pub fn set_drag_zone(bar_height: f32, buttons_left: f32, pixels_per_point: f32) {
    use std::sync::atomic::Ordering;
    if !(pixels_per_point > 0.0) {
        return;
    }
    BAR_H.store((bar_height.max(0.0) * pixels_per_point).round() as i32, Ordering::Relaxed);
    BTN_X.store((buttons_left.max(0.0) * pixels_per_point).round() as i32, Ordering::Relaxed);
}

/// 装上窗口子类。必须在窗口创建之后调一次；这条消息链在窗口整个生命周期都有效。
#[cfg(windows)]
pub fn install(hwnd: isize) {
    unsafe {
        imp::install(hwnd);
    }
}

/// 圆角随最大化状态切换：界面层在状态变化时调用一次（逐帧发 DWM 属性没必要）
#[cfg(windows)]
pub fn refresh_shape(hwnd: isize, maximized: bool) {
    unsafe {
        imp::apply_shape(hwnd as _, maximized);
    }
}

#[cfg(not(windows))]
pub fn refresh_shape(_hwnd: isize, _maximized: bool) {}

/// 无头测试用：读当前写给原生侧的天区（顶栏高度物理 px，按钮区左缘物理 px）
#[cfg(test)]
pub(crate) fn drag_zone() -> (i32, i32) {
    #[cfg(windows)]
    {
        use std::sync::atomic::Ordering;
        (BAR_H.load(Ordering::Relaxed), BTN_X.load(Ordering::Relaxed))
    }
    #[cfg(not(windows))]
    (0, 0)
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::Ordering;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND,
    };
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::Controls::MARGINS;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, GetWindowRect, IsIconic, IsZoomed, HTBOTTOM, HTBOTTOMLEFT,
        HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT,
        SetWindowPos, SIZE_MAXIMIZED, SIZE_MINIMIZED, SIZE_RESTORED, SM_CXPADDEDBORDER,
        SM_CXSIZEFRAME, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        WM_NCACTIVATE, WM_NCCALCSIZE, WM_NCHITTEST, WM_SIZE,
    };

    /// 还原后要不要清一次帧：最小化期间失活路径会按系统默认把标准标题栏画上
    /// （窗口被 DWM 遮蔽看不见），还原后的激活又因传 -1 而不重画，残留便一直挂着。
    /// 置位于 WM_SIZE(SIZE_MINIMIZED)，还原出来时消费掉，只发一次 FRAMECHANGED
    static RESTORE_PENDING: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// 随手挑的一个 id：只要进程内唯一就行
    const SUBCLASS_ID: usize = 0x524D_4247;

    /// 视觉样式下的两条未公开消息：DefWindowProc 会借它们重画经典标题栏/边框，
    /// 是偶现双标题栏的另一条路（Chromium 同款处理：直接吞掉）
    const WM_NCUAHDRAWCAPTION: u32 = 0x00AE;
    const WM_NCUAHDRAWFRAME: u32 = 0x00AF;

    pub unsafe fn install(hwnd: isize) {
        // 阴影：客户区扩到整窗后 DWM 不再认为这扇窗有框，投影跟着没了；
        // 往客户区延 1px 框（四边，画完就被界面盖住）把 DWM 的投影请回来
        let margins = MARGINS {
            cxLeftWidth: 1,
            cyTopHeight: 1,
            cxRightWidth: 1,
            cyBottomHeight: 1,
        };
        windows_sys::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea(
            hwnd as HWND,
            &margins,
        );
        SetWindowSubclass(hwnd as HWND, Some(wnd_proc), SUBCLASS_ID, 0);
    }

    /// 非最大化时给四角圆角，最大化切回直角（满屏圆角会把四角内容裁掉）。
    /// Win11 的 DWM 圆角自带阴影；Win10 没有这个属性，调用失败即忽略（保持方角）
    pub unsafe fn apply_shape(hwnd: HWND, maximized: bool) {
        let preference = if maximized { DWMWCP_DONOTROUND } else { DWMWCP_ROUND };
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &preference as *const i32 as *const core::ffi::c_void,
            4,
        );
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        match msg {
            // 客户区扩到整窗 = 系统标题栏消失。最大化时窗口每边伸出屏幕一个边框宽，
            // 把客户区夹回显示器工作区，内容不多不少正好铺满可视区域
            WM_NCCALCSIZE if wparam != 0 => {
                if IsZoomed(hwnd) != 0 {
                    let rects = lparam as *mut RECT;
                    clamp_to_work_area(hwnd, &mut *rects);
                }
                0
            }
            WM_NCHITTEST => hit_test(hwnd, lparam),
            // 激活/失活切换（点别的窗口、Alt+Tab、弹系统通知……）：DefWindowProc 默认
            // 会把标准非客户区重画一遍 —— 系统标题栏盖回来，双标题栏的偶现根源。
            // lParam 传 -1 让它跳过这次重绘；但「失活且已最小化」必须原样放行：
            // 图标化窗口的激活变更被拦会让还原后激活态错乱（双标题栏 + 界面假死），
            // 而此时窗口被 DWM 遮蔽，标准框画了也看不见。返回值恒 TRUE 放行激活变更
            WM_NCACTIVATE => {
                let lp: LPARAM = if wparam == 0 && IsIconic(hwnd) != 0 {
                    lparam
                } else {
                    -1
                };
                DefSubclassProc(hwnd, msg, wparam, lp);
                1
            }
            // 视觉样式下的两条未公开消息：DefWindowProc 会借它们重画经典标题栏/边框，
            // 直接吞掉（Chromium 同款处理）
            WM_NCUAHDRAWCAPTION | WM_NCUAHDRAWFRAME => 0,
            // 还原/最大化出来时强制重算一次非客户区：重跑 WM_NCCALCSIZE（客户区=整窗），
            // DWM 随之把最小化期间挂上的标准标题栏残留清掉。只在刚从最小化出来时发一次
            WM_SIZE => {
                if wparam == SIZE_MINIMIZED as usize {
                    RESTORE_PENDING.store(true, Ordering::Relaxed);
                } else if (wparam == SIZE_RESTORED as usize || wparam == SIZE_MAXIMIZED as usize)
                    && RESTORE_PENDING.swap(false, Ordering::Relaxed)
                {
                    SetWindowPos(
                        hwnd,
                        std::ptr::null_mut(),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                    );
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }

    /// 窗口自己 DPI 下的边框总宽（可缩放边框 + 系统垫进去的几像素）。
    /// GetSystemMetrics 报的是系统 DPI 的数，跨显示器会差几像素，按窗口 DPI 换算一遍。
    /// 仅作获取工作区失败时的兜底，正常路径一律用显示器工作区。
    unsafe fn frame_pad(hwnd: HWND) -> i32 {
        let frame = GetSystemMetrics(SM_CXSIZEFRAME) as i64;
        let added = GetSystemMetrics(SM_CXPADDEDBORDER) as i64;
        let dpi = GetDpiForWindow(hwnd) as i64;
        if dpi > 0 {
            ((frame + added) * dpi / 96) as i32
        } else {
            (frame + added) as i32
        }
    }

    /// 窗口所在显示器的工作区（不含任务栏）。
    unsafe fn work_area(hwnd: HWND) -> Option<RECT> {
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            Some(info.rcWork)
        } else {
            None
        }
    }

    /// 最大化时窗口矩形每边伸出屏幕一个边框宽。以前按 GetSystemMetrics 估算这个宽度，
    /// 多显示器缩放不一致时会估多几像素 → 客户区比可视区域小 → 四周漏出黑线；
    /// 现在直接把矩形夹回显示器工作区，估算误差不再参与布局。
    unsafe fn clamp_to_work_area(hwnd: HWND, rects: &mut RECT) {
        match work_area(hwnd) {
            Some(work) => {
                rects.left = rects.left.max(work.left);
                rects.top = rects.top.max(work.top);
                rects.right = rects.right.min(work.right);
                rects.bottom = rects.bottom.min(work.bottom);
            }
            // 兜底：拿不到工作区就退回老算法
            None => {
                let pad = frame_pad(hwnd);
                rects.left += pad;
                rects.top += pad;
                rects.right -= pad;
                rects.bottom -= pad;
            }
        }
    }

    /// 顶栏天区 + 四周边框的命中测试。lparam 是屏幕坐标（低 16 位 x、高 16 位 y，有符号）
    unsafe fn hit_test(hwnd: HWND, lparam: LPARAM) -> LRESULT {
        let x_screen = (lparam & 0xFFFF) as u16 as i16 as i32;
        let y_screen = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;
        let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetWindowRect(hwnd, &mut rc);
        let x = x_screen - rc.left;
        let y = y_screen - rc.top;
        let width = rc.right - rc.left;
        let height = rc.bottom - rc.top;
        let zoomed = IsZoomed(hwnd) != 0;
        if !zoomed {
            let pad = frame_pad(hwnd);
            let left = x < pad;
            let right = x >= width - pad;
            let top = y < pad;
            let bottom = y >= height - pad;
            if left || right || top || bottom {
                return match (left, right, top, bottom) {
                    (true, _, true, _) => HTTOPLEFT as LRESULT,
                    (_, true, true, _) => HTTOPRIGHT as LRESULT,
                    (true, _, _, true) => HTBOTTOMLEFT as LRESULT,
                    (_, true, _, true) => HTBOTTOMRIGHT as LRESULT,
                    (true, _, _, _) => HTLEFT as LRESULT,
                    (_, true, _, _) => HTRIGHT as LRESULT,
                    (_, _, true, _) => HTTOP as LRESULT,
                    (_, _, _, true) => HTBOTTOM as LRESULT,
                    _ => HTCLIENT as LRESULT,
                };
            }
        }
        // 顶栏天区：最大化时客户区缩进了边框宽，坐标先换算回客户区再比对。
        // 缩进量与 WM_NCCALCSIZE 同源（工作区差值），拿不到就退回边框估算
        let bar_h = super::BAR_H.load(Ordering::Relaxed);
        let btn_x = super::BTN_X.load(Ordering::Relaxed);
        let (inset_x, inset_y) = if zoomed {
            match work_area(hwnd) {
                Some(work) => ((work.left - rc.left).max(0), (work.top - rc.top).max(0)),
                None => (frame_pad(hwnd), frame_pad(hwnd)),
            }
        } else {
            (0, 0)
        };
        let lx = x - inset_x;
        let ly = y - inset_y;
        if bar_h > 0 && ly >= 0 && ly < bar_h && lx < btn_x {
            return HTCAPTION as LRESULT;
        }
        HTCLIENT as LRESULT
    }
}

#[cfg(not(windows))]
pub fn set_drag_zone(_bar_height: f32, _buttons_left: f32, _pixels_per_point: f32) {}

#[cfg(not(windows))]
pub fn install(_hwnd: isize) {}
