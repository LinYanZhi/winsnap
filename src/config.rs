//! 硬编码配置 — 开发者直接编辑此文件修改规则，打包后无需外部配置文件

use std::path::Path;
use windows::Win32::Foundation::HWND;

// ── 交互设置 ──

pub const SCALE_STEP: f64 = 0.1;

// ── Win32 辅助 ──

/// 获取窗口进程名（不含 .exe）
pub fn get_process_name(hwnd: HWND) -> String {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    unsafe {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 { return String::new(); }

        let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid);
        if let Ok(handle) = handle {
            let mut buf = vec![0u16; 260];
            let mut len = buf.len() as u32;
            let result = QueryFullProcessImageNameW(
                handle, PROCESS_NAME_FORMAT(0),
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut len,
            );
            let _ = CloseHandle(handle);
            if result.is_ok() {
                let name = String::from_utf16_lossy(&buf[..len as usize]);
                let name = Path::new(&name)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                return name;
            }
        }
    }
    String::new()
}

/// 探测目标窗口线程是否响应（SendMessageTimeout + SMTO_ABORTIFHUNG）
///
/// 对挂起进程的窗口做同步调用（SetWindowPos / GetWindowTextW）会被无限期阻塞，
/// 导致 winctl 整体卡死（Windows 报 AppHang，用户只能关掉进程）。
/// 调用前先用 WM_NULL 探测：目标线程挂起时该调用会立即失败返回，
/// 从而跳过操作而不是卡住本进程。
fn window_responding(hwnd: HWND, timeout_ms: u32) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_NULL,
    };
    unsafe {
        let mut result: usize = 0;
        SendMessageTimeoutW(
            hwnd,
            WM_NULL,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
            SMTO_ABORTIFHUNG,
            timeout_ms,
            Some(&mut result),
        ).0 != 0
    }
}

/// 获取窗口标题
pub fn get_window_title(hwnd: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_GETTEXT,
    };
    unsafe {
        let mut buf = vec![0u16; 512];
        let mut result: usize = 0;
        let ret = SendMessageTimeoutW(
            hwnd,
            WM_GETTEXT,
            windows::Win32::Foundation::WPARAM(buf.len()),
            windows::Win32::Foundation::LPARAM(buf.as_mut_ptr() as isize),
            SMTO_ABORTIFHUNG,
            200,
            Some(&mut result),
        );
        // 目标窗口挂起/超时 → 返回空标题，避免阻塞
        if ret.0 == 0 {
            return String::new();
        }
        let len = result.min(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }
}

/// 获取窗口矩形 (left, top, right, bottom)
pub fn get_window_rect(hwnd: HWND) -> (i32, i32, i32, i32) {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    unsafe {
        let mut rect = std::mem::zeroed();
        GetWindowRect(hwnd, &mut rect).ok();
        (rect.left, rect.top, rect.right, rect.bottom)
    }
}

/// 获取窗口可视矩形 (left, top, right, bottom)
///
/// 通过 DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS) 获取窗口的
/// 实际可视区域，排除 DWM 的不可见调整边框和阴影。
/// 贴边吸附/缩放均以可视区域为准（与 `get_dwm_frame_offsets` 同源，
/// 但直接返回矩形，避免右侧/底部偏移兜底值带来的误差）。
/// 获取失败或矩形无效时回退 `get_window_rect`。
pub fn get_visual_rect(hwnd: HWND) -> (i32, i32, i32, i32) {
    unsafe {
        use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
        let mut dr: windows::Win32::Foundation::RECT = std::mem::zeroed();
        let size = std::mem::size_of::<windows::Win32::Foundation::RECT>() as u32;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut dr as *mut _ as *mut std::ffi::c_void,
            size,
        )
        .is_ok()
        {
            // 最小化等异常状态 DWM 可能返回无意义矩形，校验后才采用
            if dr.right > dr.left && dr.bottom > dr.top {
                return (dr.left, dr.top, dr.right, dr.bottom);
            }
        }
        get_window_rect(hwnd)
    }
}

/// 判断窗口是否被系统判定为"不可见"（不参与用户交互/吸附）
///
/// 通用机制而非进程黑名单：通过 DWM 的 Cloak 状态判断。
/// UWP 宿主框架残留、Win+D 显示桌面、后台挂起等被系统隐藏的窗口
/// 都会进入 cloak 状态（DWM_CLOAKED_APP / DWM_CLOAKED_SHELL 等），
/// 这类窗口用户实际看不到，不能作为吸附对象。
/// 获取失败时保守地视为可见（不误杀）。
pub fn is_system_hidden(hwnd: HWND) -> bool {
    unsafe {
        use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
        let mut cloaked: u32 = 0;
        let size = std::mem::size_of::<u32>() as u32;
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut std::ffi::c_void,
            size,
        )
        .is_ok() && cloaked != 0
    }
}

/// 屏幕尺寸（多显示器：虚拟屏幕范围）
pub fn screen_size() -> (i32, i32) {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
        };
        (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN))
    }
}

/// 获取窗口所在显示器的工作区（排除任务栏等系统占用区域）
///
/// 返回 (left, top, right, bottom)，即 rcWork 的四个边界。
/// 工作区是任务栏、停靠栏等系统 UI 未遮挡的有效可视区域，
/// 用于等比例缩放时计算最大窗口尺寸，避免窗口延伸到任务栏下方。
pub fn get_monitor_work_area(hwnd: HWND) -> (i32, i32, i32, i32) {
    unsafe {
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        };
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: std::mem::zeroed(),
            rcWork: std::mem::zeroed(),
            dwFlags: 0,
        };
        if GetMonitorInfoW(monitor, &mut mi).as_bool() {
            (mi.rcWork.left, mi.rcWork.top, mi.rcWork.right, mi.rcWork.bottom)
        } else {
            // 失败时回退到全屏尺寸
            let (sw, sh) = screen_size();
            (0, 0, sw, sh)
        }
    }
}

/// 移动窗口
pub fn move_window_to(hwnd: HWND, x: i32, y: i32) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::SetWindowPos;
    // 目标窗口线程挂起时 SetWindowPos 会同步阻塞，先探测，不响应则跳过
    if !window_responding(hwnd, 100) {
        return false;
    }
    unsafe {
        SetWindowPos(hwnd, None, x, y, 0, 0,
            windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
        ).is_ok()
    }
}

/// 设置窗口位置和大小
pub fn set_window_pos(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::SetWindowPos;
    // 同上：目标挂起时跳过，避免阻塞
    if !window_responding(hwnd, 100) {
        return false;
    }
    unsafe {
        SetWindowPos(hwnd, None, x, y, w, h,
            windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
        ).is_ok()
    }
}

/// 获取 DWM 不可见调整边框的偏移量
///
/// 使用 DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS) 获取窗口的
/// 实际可视区域（排除 DWM 的不可见调整边框），与 GetWindowRect 对比，
/// 计算出四边不可见边框的像素宽度。
///
/// 对于 DWM 无法正确报告的右侧和底部（常见于自定义窗口框架如 Windows Terminal），
/// 使用 GetSystemMetrics(SM_CXSIZEFRAME + SM_CXPADDEDBORDER) 作为备用值。
///
/// 返回 (left_off, top_off, right_off, bottom_off)。
/// 用于贴边吸附时补偿不可见边框，使窗口内容紧贴屏幕边缘。
pub fn get_dwm_frame_offsets(hwnd: HWND) -> (i32, i32, i32, i32) {
    unsafe {
        use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXPADDEDBORDER, SM_CXSIZEFRAME,
        };

        let wr = get_window_rect(hwnd);
        let mut dr: windows::Win32::Foundation::RECT = std::mem::zeroed();
        let size = std::mem::size_of::<windows::Win32::Foundation::RECT>() as u32;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut dr as *mut _ as *mut std::ffi::c_void,
            size,
        )
        .is_err()
        {
            return (0, 0, 0, 0);
        }

        // dr (DWM 可视区域) 排除了不可见的调整边框
        // 正常情况: dr.left >= wr.0, dr.right <= wr.2, dr.top >= wr.1, dr.bottom <= wr.3
        let left_off = dr.left - wr.0;
        let top_off = dr.top - wr.1;
        let right_off = wr.2 - dr.right;
        let bottom_off = wr.3 - dr.bottom;

        // 自绘无边框窗口（如 Trae/WPS/WeCom 等）：DWM 视觉区域 == 窗口帧，四边偏移均为 0。
        // 此时不能套用系统边框宽度，否则视觉区域被高估、缩放最大时窗口底部/右侧多出。
        if left_off == 0 && top_off == 0 && right_off == 0 && bottom_off == 0 {
            return (0, 0, 0, 0);
        }

        // 系统可调整边框宽度（包含不可见调整边框和阴影）
        // 用于 DWM 对右/下报告为 0 时备用（某些窗口的 DWM 扩展边框不完整，但左/上有偏移说明确有边框）
        let sys_border = (GetSystemMetrics(SM_CXSIZEFRAME) + GetSystemMetrics(SM_CXPADDEDBORDER)) as i32;

        (
            left_off.max(0),
            top_off.max(0),
            if right_off > 0 { right_off } else { sys_border },
            if bottom_off > 0 { bottom_off } else { sys_border },
        )
    }
}

/// 计算等比例缩放后的目标窗口帧矩形（不实际移动窗口）
///
/// 使用窗口所在显示器的工作区作为客户区最大尺寸上限，
/// 返回 (left, top, right, bottom) 即窗口帧四边。
/// 供动画缩放使用：先算目标矩形，再从当前位置平滑过渡。
pub fn scale_window_target(hwnd: HWND, factor: f64) -> Option<(i32, i32, i32, i32)> {
    scale_rect_target(hwnd, get_window_rect(hwnd), factor)
}

/// 与 `scale_window_target` 相同，但基于给定的窗口帧矩形计算
/// （而非实时读取窗口当前位置），供连续滚动缩放时在上一个目标矩形上继续累加。
pub fn scale_rect_target(
    hwnd: HWND,
    rect: (i32, i32, i32, i32),
    factor: f64,
) -> Option<(i32, i32, i32, i32)> {
    // 视觉区域偏移（DWM 不可见 resize 边框+阴影），与贴边逻辑保持一致
    let (lo, to, ro, bo) = get_dwm_frame_offsets(hwnd);
    let (left, _top, right, bottom) = rect;
    // 视觉区域尺寸 = 窗口帧 - DWM 不可见边框偏移
    let vw = (right - left) - lo - ro;
    let vh = (bottom - _top) - to - bo;
    if vw <= 0 || vh <= 0 {
        return None;
    }
    // 视觉区域按因子等比例缩放（所见内容等比）
    let nw = ((vw as f64) * (1.0 + factor)).round() as i32;
    let nh = ((vh as f64) * (1.0 + factor)).round() as i32;
    // 视觉区域最大 = 工作区：最大时视觉内容精确贴合工作区四边，无缝隙
    let (wa_l, wa_t, wa_r, wa_b) = get_monitor_work_area(hwnd);
    let nw = nw.min(wa_r - wa_l);
    let nh = nh.min(wa_b - wa_t);
    // 视觉区域居中于工作区
    let vx = wa_l + ((wa_r - wa_l) - nw) / 2;
    let vy = wa_t + ((wa_b - wa_t) - nh) / 2;
    // 映射回窗口帧（帧 = 视觉区域 + DWM 不可见边框偏移）
    let fx = vx - lo;
    let fy = vy - to;
    let fw = nw + lo + ro;
    let fh = nh + to + bo;
    Some((fx, fy, fx + fw, fy + fh))
}

/// 等比例缩放窗口（保持客户区比例）
///
/// 计算目标矩形后直接定位到目标位置（瞬移）。
/// 需要动画时由主程序先调用 `scale_window_target` 计算目标再平滑过渡。
pub fn scale_window(hwnd: HWND, factor: f64) -> bool {
    match scale_window_target(hwnd, factor) {
        Some((x, y, r, b)) => set_window_pos(hwnd, x, y, r - x, b - y),
        None => false,
    }
}

/// 9 宫格定位
pub fn pos_by_key(key: char, hwnd: HWND) -> Option<(i32, i32)> {
    let (left, _top, right, bottom) = get_window_rect(hwnd);
    let w = right - left;
    let h = bottom - _top;
    let (sw, sh) = screen_size();
    let (lo, to, ro, bo) = get_dwm_frame_offsets(hwnd);
    match key {
        '1' => Some((-lo, sh - h + bo)),
        '2' => Some(((sw - w) / 2, sh - h + bo)),
        '3' => Some((sw - w + ro, sh - h + bo)),
        '4' => Some((-lo, (sh - h) / 2)),
        '5' => Some(((sw - w) / 2, (sh - h) / 2)),
        '6' => Some((sw - w + ro, (sh - h) / 2)),
        '7' => Some((-lo, -to)),
        '8' => Some(((sw - w) / 2, -to)),
        '9' => Some((sw - w + ro, -to)),
        _ => None,
    }
}

// ── 鼠标位置查询 ──

/// 获取鼠标屏幕坐标
pub fn get_cursor_pos() -> (i32, i32) {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut pt = std::mem::zeroed();
        GetCursorPos(&mut pt).ok();
        (pt.x, pt.y)
    }
}

/// 获取屏幕指定位置的窗口句柄
pub fn window_from_point(x: i32, y: i32) -> HWND {
    unsafe {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::WindowFromPoint;
        WindowFromPoint(POINT { x, y })
    }
}

/// 根据鼠标在窗口中的位置计算 SC_SIZE 方向
///
/// 返回 HTLEFT/HTRIGHT/HTTOP/HTBOTTOM 等方向常量值：
///   10=左, 11=右, 12=上, 13=左上, 14=右上, 15=下, 16=左下, 17=右下
pub fn resize_direction(cx: i32, cy: i32, l: i32, t: i32, r: i32, b: i32) -> u32 {
    let w = r - l;
    let h = b - t;
    if w <= 0 || h <= 0 {
        return 11; // HTRIGHT — 默认
    }

    let rx = (cx - l) as f64 / w as f64; // 0..1 窗口内横向相对位置
    let ry = (cy - t) as f64 / h as f64; // 0..1 窗口内纵向相对位置

    let edge_threshold = 0.33;

    // 水平方向
    let horiz = if rx < edge_threshold {
        -1 // 左
    } else if rx > 1.0 - edge_threshold {
        1 // 右
    } else {
        0 // 中
    };

    // 垂直方向
    let vert = if ry < edge_threshold {
        -1 // 上
    } else if ry > 1.0 - edge_threshold {
        1 // 下
    } else {
        0 // 中
    };

    match (horiz, vert) {
        (-1, -1) => 13, // 左上
        (0, -1)  => 12, // 上
        (1, -1)  => 14, // 右上
        (-1, 0)  => 10, // 左
        (1, 0)   => 11, // 右
        (-1, 1)  => 16, // 左下
        (0, 1)   => 15, // 下
        (1, 1)   => 17, // 右下
        _        => 17, // 中间区域 → 默认右下
    }
}

/// 获取窗口的顶级父窗口（处理子控件被点击的情况）
pub fn get_root_window(hwnd: HWND) -> HWND {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GA_ROOT};
        GetAncestor(hwnd, GA_ROOT)
    }
}


