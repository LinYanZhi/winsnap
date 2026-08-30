//! 系统托盘：隐藏窗口 + 消息循环 + 右键菜单 + 低层鼠标钩子 + 看门狗

use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::thread;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    HBRUSH, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, RGBQUAD,
    CreateDIBSection, DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC,
};
use windows::Win32::System::Console::{AllocConsole, GetConsoleWindow, SetConsoleTitleW};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BringWindowToTop, CallNextHookEx, CreateIconIndirect, CreatePopupMenu,
    CreateWindowExW, CS_HREDRAW, DefWindowProcW, DestroyIcon, DestroyMenu, DispatchMessageW,
    EnumWindows, GetIconInfo, GetMessageW, GetWindowLongPtrW, GWL_STYLE, HCURSOR, HHOOK, HICON,
    ICONINFO, IDI_APPLICATION, IDI_WINLOGO, IsWindowVisible, LoadIconW, MF_CHECKED, MF_SEPARATOR,
    MF_STRING, MSG, PostMessageW, RegisterClassExW, RegisterWindowMessageW, SetForegroundWindow,
    SetWindowsHookExW, ShowWindow, SW_HIDE, SW_SHOW, TrackPopupMenu, TranslateMessage,
    TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_NULL, WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE,
    WNDCLASSEXW, WS_CAPTION, GetDesktopWindow,
};

use crate::anim::ANIMATE_ENABLED;
use crate::autostart::is_auto_start_enabled;
use crate::log::{c, CLR_FAIL, CLR_INTERACT, CLR_PAUSE, CLR_POSITION, CLR_RESUME, CLR_SCALE, CLR_SUCCESS, CLR_TIP};
use crate::snap::{SNAP_SCREEN_ENABLED, SNAP_WINDOW_ENABLED};
use crate::state::{
    is_alt_held, HOOK_DRAG_REQUEST, HOOK_HANDLE, HOOK_INSTALLED, HOOK_PAUSED,
    MOUSE_LEFT_HOOK, MOUSE_RIGHT_HOOK,
};
use crate::config as cfg;

// ── 托盘状态 ──

pub static TRAY_WINDOW: AtomicIsize = AtomicIsize::new(0);   // 托盘隐藏窗口句柄
pub static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);
pub static GRAY_ICON_CACHE: AtomicIsize = AtomicIsize::new(0);
pub static CURRENT_MENU: AtomicIsize = AtomicIsize::new(0);  // 正在弹出的右键菜单句柄
/// 上次自动重启托盘线程的时间（毫秒时间戳），防止看门狗在窗口未就绪期间重复重启
static TRAY_RESPAWN_MS: AtomicU64 = AtomicU64::new(0);

// ── 托盘消息/菜单 ID ──

pub const WM_TRAYICON: u32 = WM_APP + 1;
pub const WM_TRAY_REFRESH: u32 = WM_APP + 2;
pub const WM_TRAY_RESTORE: u32 = WM_APP + 3; // 新实例请求旧实例重建托盘图标 // 监听状态变化 → 刷新托盘图标/提示
const ID_TRAY_CENTER: u16 = 1000;
const ID_TRAY_AUTOSTART: u16 = 1002;
const ID_TRAY_PAUSE: u16 = 1003;
const ID_TRAY_EXIT: u16 = 1001;
const ID_TRAY_ANIMATE: u16 = 1004;
const ID_TRAY_SNAP_SCREEN: u16 = 1005;
const ID_TRAY_SNAP_WINDOW: u16 = 1006;

/// 启动托盘图标线程（隐藏窗口 + 消息循环）
pub fn spawn_tray() {
    thread::spawn(move || {
        unsafe {
            // COM 初始化（Shell_NotifyIconW 需要）
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

            let hinstance = GetModuleHandleW(None)
                .expect("GetModuleHandleW 失败");

            let class_name = w!("winsnap_tray_window");

            // 载入嵌入的自定义箭头图标（winres 默认资源 ID 为 1）
            let app_icon = LoadIconW(hinstance, PCWSTR(1 as *const u16))
                .unwrap_or_default();

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW,
                lpfnWndProc: Some(tray_wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance.into(),
                hIcon: app_icon,
                hCursor: HCURSOR::default(),
                hbrBackground: HBRUSH::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: class_name,
                hIconSm: app_icon,
            };
            let atom = RegisterClassExW(&wc);
            if atom == 0 {
                // 类已注册（此前托盘线程注册过）→ 复用，允许托盘线程重启
                if windows::Win32::Foundation::GetLastError()
                    != windows::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS
                {
                    eprintln!("注册托盘窗口类失败");
                    return;
                }
            }

            // 提前注册 Explorer 重启消息（任务栏重建后托盘图标被清空，据此自动重建）
            // 必须在创建窗口之前注册，避免 Explorer 恰好此时重启而漏掉广播
            TASKBAR_CREATED_MSG.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                w!("winsnap_tray"),
                WINDOW_STYLE::default(),
                0, 0, 0, 0, None, None, hinstance, None,
            ) {
                Ok(h) => h,
                Err(_) => {
                    eprintln!("创建托盘窗口失败");
                    return;
                }
            };
            // 保存窗口句柄，供主线程通知刷新图标
            TRAY_WINDOW.store(hwnd.0 as isize, Ordering::Relaxed);

            // 创建托盘图标
            if add_tray_icon() {
                println!("{}", c("托盘图标已创建", CLR_SUCCESS));

                // 按当前监听状态刷新图标/提示（初始为彩色）
                update_tray_icon();

                // 安装低层鼠标钩子，追踪全局鼠标按键状态
                // 钩子回调由本线程的消息泵同步调用，通过原子变量共享给主线程
                if let Ok(hhk) = SetWindowsHookExW(
                    WH_MOUSE_LL,
                    Some(low_level_mouse_proc),
                    hinstance,
                    0,
                ) {
                    HOOK_HANDLE.store(hhk.0 as isize, Ordering::Relaxed);
                    HOOK_INSTALLED.store(true, Ordering::SeqCst);
                }

                // 消息循环
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                // 退出时删除托盘图标
                let mut del = NOTIFYICONDATAW::default();
                del.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
                del.hWnd = hwnd;
                del.uID = 1;
                let _ = Shell_NotifyIconW(NIM_DELETE, &del);
                // 释放灰度图标缓存
                let gray = GRAY_ICON_CACHE.swap(0, Ordering::Relaxed);
                if gray != 0 {
                    let _ = DestroyIcon(HICON(gray as *mut std::ffi::c_void));
                }
            } else {
                eprintln!("创建托盘图标失败");
            }
        }
    });
}

/// 托盘看门狗线程：每 5 秒检查托盘图标是否还在，
/// 丢失（Explorer 重启/异常清除）则通知托盘线程重建；
/// 托盘窗口已失效（托盘线程退出）则自动重启托盘线程
pub fn spawn_tray_watchdog() {
    thread::spawn(move || {
        unsafe {
            use windows::Win32::UI::Shell::{Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER};
            use windows::Win32::UI::WindowsAndMessaging::IsWindow;

            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let hwnd = HWND(TRAY_WINDOW.load(Ordering::Relaxed) as *mut std::ffi::c_void);
                if hwnd.0.is_null() {
                    continue; // 托盘窗口尚未创建
                }
                if !IsWindow(hwnd).as_bool() {
                    // 托盘窗口已失效 → 托盘线程已退出，重启托盘线程
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let last = TRAY_RESPAWN_MS.load(Ordering::Relaxed);
                    if now_ms.saturating_sub(last) >= 5000 {
                        TRAY_RESPAWN_MS.store(now_ms, Ordering::Relaxed);
                        eprintln!("托盘窗口失效，自动重启托盘线程");
                        spawn_tray();
                    }
                    continue;
                }
                // 图标不在通知区域（Explorer 重启/异常清除）→ 请求托盘线程重建
                let nid = NOTIFYICONIDENTIFIER {
                    cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
                    hWnd: hwnd,
                    uID: 1,
                    ..Default::default()
                };
                if Shell_NotifyIconGetRect(&nid).is_err() {
                    let _ = PostMessageW(hwnd, WM_TRAY_RESTORE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

/// 托盘隐藏窗口的窗口过程
unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAYICON => {
                let event = lparam.0 as u32;
                match event {
                    0x0202 => { // WM_LBUTTONUP — 切换控制台显隐
                        toggle_console();
                        LRESULT(0)
                    }
                    0x0205 => { // WM_RBUTTONUP — 显示右键菜单
                        if let Ok(menu) = CreatePopupMenu() {
                            if let Err(e) = AppendMenuW(menu, MF_STRING, ID_TRAY_CENTER as usize, w!("全部居中")) {
                                eprintln!("AppendMenuW 失败: {e}");
                            }
                            // 开机自启开关
                            let auto_enabled = is_auto_start_enabled();
                            let flags = if auto_enabled { MF_STRING | MF_CHECKED } else { MF_STRING };
                            if let Err(e) = AppendMenuW(menu, flags, ID_TRAY_AUTOSTART as usize, w!("开机自启")) {
                                eprintln!("AppendMenuW 失败: {e}");
                            }
                            // 动画移动开关（Alt+数字/方向键贴边/Alt+滚轮缩放以动画平滑移动）
                            let anim_enabled = ANIMATE_ENABLED.load(Ordering::Relaxed);
                            let anim_flags = if anim_enabled { MF_STRING | MF_CHECKED } else { MF_STRING };
                            if let Err(e) = AppendMenuW(menu, anim_flags, ID_TRAY_ANIMATE as usize, w!("开启动画")) {
                                eprintln!("AppendMenuW 失败: {e}");
                            }
                            // 屏幕吸附开关（Alt+左键拖拽贴屏幕边缘，默认开启）
                            let snap_sc = SNAP_SCREEN_ENABLED.load(Ordering::Relaxed);
                            let snap_sc_flags = if snap_sc { MF_STRING | MF_CHECKED } else { MF_STRING };
                            if let Err(e) = AppendMenuW(menu, snap_sc_flags, ID_TRAY_SNAP_SCREEN as usize, w!("屏幕吸附")) {
                                eprintln!("AppendMenuW 失败: {e}");
                            }
                            // 窗口吸附开关（Alt+左键拖拽贴其它窗口边缘，默认关闭）
                            let snap_win = SNAP_WINDOW_ENABLED.load(Ordering::Relaxed);
                            let snap_win_flags = if snap_win { MF_STRING | MF_CHECKED } else { MF_STRING };
                            if let Err(e) = AppendMenuW(menu, snap_win_flags, ID_TRAY_SNAP_WINDOW as usize, w!("窗口吸附")) {
                                eprintln!("AppendMenuW 失败: {e}");
                            }
                            // 监听开关
                            let paused = HOOK_PAUSED.load(Ordering::Relaxed);
                            let pause_flags = if paused { MF_STRING | MF_CHECKED } else { MF_STRING };
                            if let Err(e) = AppendMenuW(menu, pause_flags, ID_TRAY_PAUSE as usize, w!("禁用监听")) {
                                eprintln!("AppendMenuW 失败: {e}");
                            }
                            if let Err(e) = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) {
                                eprintln!("AppendMenuW 失败: {e}");
                            }
                            if let Err(e) = AppendMenuW(menu, MF_STRING, ID_TRAY_EXIT as usize, w!("退出")) {
                                eprintln!("AppendMenuW 失败: {e}");
                            }
                            let _ = SetForegroundWindow(hwnd);
                            let (x, y) = cfg::get_cursor_pos();
                            // 保存菜单句柄，供监听状态变化时实时同步勾选
                            CURRENT_MENU.store(menu.0 as isize, Ordering::Relaxed);
                            let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, x, y, 0, hwnd, None);
                            // TrackPopupMenu 返回后菜单已关闭，句柄失效
                            CURRENT_MENU.store(0, Ordering::Relaxed);
                            let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
                            let _ = DestroyMenu(menu);
                        }
                        LRESULT(0)
                    }
                    _ => DefWindowProcW(hwnd, msg, wparam, lparam),
                }
            }
            WM_COMMAND => {
                let id = wparam.0 as u32 & 0xFFFF;
                if id == ID_TRAY_CENTER as u32 {
                    // 居中要对所有带标题栏窗口做 SetWindowPos，放到后台线程避免阻塞托盘消息泵
                    thread::spawn(center_all_windows);
                    return LRESULT(0);
                } else if id == ID_TRAY_AUTOSTART as u32 {
                    // 开机自启操作（PowerShell 启动慢）在后台线程执行，不阻塞消息泵
                    thread::spawn(|| {
                        let new_state = crate::autostart::toggle_auto_start();
                        if new_state {
                            println!("{}", c("开机自启：已开启", CLR_SUCCESS));
                        } else {
                            println!("{}", c("开机自启：已关闭", CLR_PAUSE));
                        }
                    });
                    return LRESULT(0);
                } else if id == ID_TRAY_ANIMATE as u32 {
                    let was_on = ANIMATE_ENABLED.swap(!ANIMATE_ENABLED.load(Ordering::Relaxed), Ordering::Relaxed);
                    if was_on {
                        println!("{}", c("动画移动：已关闭（贴边/缩放瞬移）", CLR_PAUSE));
                    } else {
                        println!("{}", c("动画移动：已开启（Alt+数字/方向键贴边/Alt+滚轮缩放平滑移动）", CLR_SUCCESS));
                    }
                    return LRESULT(0);
                } else if id == ID_TRAY_SNAP_SCREEN as u32 {
                    let was_on = SNAP_SCREEN_ENABLED.swap(!SNAP_SCREEN_ENABLED.load(Ordering::Relaxed), Ordering::Relaxed);
                    if was_on {
                        println!("{}", c("屏幕吸附：已关闭（Alt+左键拖拽不再贴屏幕边缘）", CLR_PAUSE));
                    } else {
                        println!("{}", c("屏幕吸附：已开启（Alt+左键拖拽自动贴屏幕边缘）", CLR_SUCCESS));
                    }
                    return LRESULT(0);
                } else if id == ID_TRAY_SNAP_WINDOW as u32 {
                    let was_on = SNAP_WINDOW_ENABLED.swap(!SNAP_WINDOW_ENABLED.load(Ordering::Relaxed), Ordering::Relaxed);
                    if was_on {
                        println!("{}", c("窗口吸附：已关闭（Alt+左键拖拽不再贴其它窗口边缘）", CLR_PAUSE));
                    } else {
                        println!("{}", c("窗口吸附：已开启（Alt+左键拖拽自动贴其它窗口边缘）", CLR_SUCCESS));
                    }
                    return LRESULT(0);
                } else if id == ID_TRAY_PAUSE as u32 {
                    let was_paused = HOOK_PAUSED.swap(!HOOK_PAUSED.load(Ordering::Relaxed), Ordering::Relaxed);
                    if was_paused {
                        println!("{}", c("按键监听已恢复", CLR_RESUME));
                    } else {
                        println!("{}", c("按键监听已暂停", CLR_PAUSE));
                    }
                    // 立即刷新托盘图标（禁用变灰）/提示
                    update_tray_icon();
                    return LRESULT(0);
                } else if id == ID_TRAY_EXIT as u32 {
                    println!("{}", c("程序通过托盘菜单退出", CLR_FAIL));
                    std::process::exit(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            msg if msg == TASKBAR_CREATED_MSG.load(Ordering::Relaxed) => {
                // Explorer 重启 → 任务栏重建，托盘图标被清空，重新添加
                if add_tray_icon() {
                    println!("{}", c("托盘图标已重建（任务栏重启）", CLR_SUCCESS));
                }
                LRESULT(0)
            }
            WM_TRAY_RESTORE => {
                // 新实例启动 → 请求重建托盘图标，完成后置位确认事件
                if add_tray_icon() {
                    println!("{}", c("托盘图标已重建（新实例请求）", CLR_SUCCESS));
                }
                // 通知等待中的新实例：图标已重建
                {
                    use windows::Win32::Foundation::CloseHandle;
                    use windows::Win32::System::Threading::{CreateEventW, SetEvent};
                    if let Ok(ev) = CreateEventW(None, false, false, w!("Local\\winsnap_restored")) {
                        let _ = SetEvent(ev);
                        let _ = CloseHandle(ev);
                    }
                }
                LRESULT(0)
            }
            WM_TRAY_REFRESH => {
                // 监听状态已变化 → 刷新托盘图标/提示
                update_tray_icon();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 切换控制台：无控制台则创建，有则切换显隐
fn toggle_console() {
    unsafe {
        let console = GetConsoleWindow();
        if console.0.is_null() {
            // 没有控制台 → 创建一个
            if AllocConsole().is_ok() {
                // 获取新控制台的窗口句柄并确保可见
                let new_con = GetConsoleWindow();
                let _ = ShowWindow(new_con, SW_SHOW);
                let _ = SetForegroundWindow(new_con);
                // 设置控制台标题
                let _ = SetConsoleTitleW(w!("winsnap - 窗口管理工具"));
                // 显示欢迎信息
                color::enable_ansi();
                println!("{}", c("控制台已创建", CLR_SUCCESS));
                println!("{} 窗口管理工具已启动", c("winsnap", CLR_INTERACT));
                println!("  Alt + 鼠标左键拖拽：{}", c("移动窗口", CLR_POSITION));
                println!("  Alt + 鼠标右键拖拽：{}", c("调整窗口大小", CLR_SCALE));
                println!("  Alt + 方向键：{}", c("快速贴边", CLR_POSITION));
                println!("{} 输入 ` 可暂停/恢复快捷键和鼠标监听", c("提示：", CLR_TIP));
            }
        } else {
            // 已有控制台 → 切换显隐
            if IsWindowVisible(console).as_bool() {
                let _ = ShowWindow(console, SW_HIDE);
            } else {
                let _ = ShowWindow(console, SW_SHOW);
                let _ = SetForegroundWindow(console);
                let _ = BringWindowToTop(console);
            }
        }
    }
}

/// 居中所有可见的顶层窗口（一次性操作）
fn center_all_windows() {
    unsafe {
        extern "system" fn enum_proc(hwnd: HWND, _: LPARAM) -> BOOL {
            unsafe {
                // 跳过不可见窗口
                if !IsWindowVisible(hwnd).as_bool() {
                    return BOOL(1);
                }
                // 跳过桌面
                if hwnd == GetDesktopWindow() {
                    return BOOL(1);
                }
                // 跳过 winsnap 自身的控制台窗口
                if hwnd == GetConsoleWindow() {
                    return BOOL(1);
                }
                // 跳过没有标题栏的窗口（托盘窗口、工具窗口等）
                let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
                if style & (WS_CAPTION.0 as u32) == 0 {
                    return BOOL(1);
                }
                // 获取窗口大小
                let (l, t, r, b) = cfg::get_window_rect(hwnd);
                let w = r - l;
                let h = b - t;
                if w <= 0 || h <= 0 {
                    return BOOL(1);
                }
                // 获取窗口所在显示器的工作区
                let (wl, wt, wr, wb) = cfg::get_monitor_work_area(hwnd);
                let wa_w = wr - wl;
                let wa_h = wb - wt;
                // 计算居中位置
                let nx = wl + (wa_w - w) / 2;
                let ny = wt + (wa_h - h) / 2;
                let _ = cfg::move_window_to(hwnd, nx.max(0), ny.max(0));
                BOOL(1)
            }
        }
        let _ = EnumWindows(Some(enum_proc), LPARAM(0));
        println!("{}", c("已居中所有窗口", CLR_SUCCESS));
    }
}

/// 将图标转换为灰度版本（保留 alpha 通道），失败时返回原图标
fn make_gray_icon(color_icon: HICON) -> HICON {
    unsafe {
        // 获取图标位图信息
        let mut info = ICONINFO::default();
        if !GetIconInfo(color_icon, &mut info).is_ok() {
            return color_icon;
        }

        // 读取彩色位图的尺寸/位深
        let mut bm: BITMAP = std::mem::zeroed();
        let got = GetObjectW(
            info.hbmColor,
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut std::ffi::c_void),
        );
        // 仅支持 32bpp（带 alpha）图标，否则回退原图标
        if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 || bm.bmBitsPixel != 32 {
            let _ = DeleteObject(info.hbmColor);
            let _ = DeleteObject(info.hbmMask);
            return color_icon;
        }

        let w = bm.bmWidth;
        let h = bm.bmHeight;

        let screen = GetDC(None);
        if screen.0.is_null() {
            let _ = DeleteObject(info.hbmColor);
            let _ = DeleteObject(info.hbmMask);
            return color_icon;
        }

        // 创建 32bpp top-down DIB，逐像素读取源位图
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default()],
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib = match CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(d) => d,
            Err(_) => {
                let _ = ReleaseDC(None, screen);
                let _ = DeleteObject(info.hbmColor);
                let _ = DeleteObject(info.hbmMask);
                return color_icon;
            }
        };
        if dib.0.is_null() || bits.is_null() {
            let _ = DeleteObject(dib);
            let _ = ReleaseDC(None, screen);
            let _ = DeleteObject(info.hbmColor);
            let _ = DeleteObject(info.hbmMask);
            return color_icon;
        }

        // 拷贝源图标像素到 DIB
        let n = GetDIBits(screen, info.hbmColor, 0, h as u32, Some(bits), &mut bmi, DIB_RGB_COLORS);
        if n <= 0 {
            let _ = DeleteObject(dib);
            let _ = ReleaseDC(None, screen);
            let _ = DeleteObject(info.hbmColor);
            let _ = DeleteObject(info.hbmMask);
            return color_icon;
        }

        // 灰度化（Rec.601 亮度公式，保留 alpha）
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        for p in pixels.iter_mut() {
            let a = (*p >> 24) & 0xFF;
            let r = (*p >> 16) & 0xFF;
            let g = (*p >> 8) & 0xFF;
            let b = *p & 0xFF;
            let gray = ((r as u32 * 77 + g as u32 * 150 + b as u32 * 29) >> 8) & 0xFF;
            *p = (a << 24) | (gray << 16) | (gray << 8) | gray;
        }

        // 用灰度 DIB 生成新图标（CreateIconIndirect 会复制数据，之后可释放位图）
        let ii = ICONINFO {
            fIcon: BOOL(1),
            xHotspot: info.xHotspot,
            yHotspot: info.yHotspot,
            hbmColor: dib,
            hbmMask: info.hbmMask,
        };
        let gray_icon = match CreateIconIndirect(&ii) {
            Ok(g) => g,
            Err(_) => HICON::default(),
        };

        let _ = DeleteObject(dib);
            let _ = DeleteObject(info.hbmColor);
            let _ = DeleteObject(info.hbmMask);
        let _ = ReleaseDC(None, screen);

        if gray_icon.0.is_null() {
            color_icon
        } else {
            gray_icon
        }
    }
}

/// 根据监听状态刷新托盘图标（禁用时变灰）和悬浮提示
fn update_tray_icon() {
    unsafe {
        let hwnd = HWND(TRAY_WINDOW.load(Ordering::Relaxed) as *mut std::ffi::c_void);
        if hwnd.0.is_null() {
            return;
        }
        let paused = HOOK_PAUSED.load(Ordering::Relaxed);

        // 彩色图标从资源重新加载（LoadIcon 返回共享句柄，无需销毁）
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let color_icon = LoadIconW(hinstance, PCWSTR(1 as *const u16)).unwrap_or_default();

        let icon = if paused {
            // 禁用 → 灰色图标（首次生成并缓存）
            let cached = GRAY_ICON_CACHE.load(Ordering::Relaxed);
            if cached != 0 {
                HICON(cached as *mut std::ffi::c_void)
            } else {
                let gray = make_gray_icon(color_icon);
                if !gray.0.is_null() && gray != color_icon {
                    GRAY_ICON_CACHE.store(gray.0 as isize, Ordering::Relaxed);
                }
                gray
            }
        } else {
            color_icon
        };

        // 更新图标 + 提示文字
        let tip = if paused {
            format!("winsnap v{} - 监听已禁用（输入 ` 恢复）\0", env!("CARGO_PKG_VERSION"))
        } else {
            format!("winsnap v{} - 窗口管理工具（输入 ` 禁用）\0", env!("CARGO_PKG_VERSION"))
        };
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_TIP,
            hIcon: icon,
            ..NOTIFYICONDATAW::default()
        };
        for (i, c) in tip.encode_utf16().enumerate().take(127) {
            nid.szTip[i] = c;
        }
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// 添加/重建托盘图标（初始创建、TaskbarCreated、WM_TRAY_RESTORE 均复用此函数）
fn add_tray_icon() -> bool {
    unsafe {
        let hwnd = HWND(TRAY_WINDOW.load(Ordering::Relaxed) as *mut std::ffi::c_void);
        if hwnd.0.is_null() {
            return false;
        }
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let app_icon = LoadIconW(hinstance, PCWSTR(1 as *const u16)).unwrap_or_default();
        let icon = if !app_icon.0.is_null() {
            app_icon
        } else {
            LoadIconW(None, IDI_WINLOGO)
                .or_else(|_| LoadIconW(None, IDI_APPLICATION))
                .unwrap_or_default()
        };

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAYICON,
            hIcon: icon,
            ..NOTIFYICONDATAW::default()
        };
        // 设置提示文字
        let tip = format!("winsnap v{} - 窗口管理工具\0", env!("CARGO_PKG_VERSION"));
        for (i, c) in tip.encode_utf16().enumerate().take(127) {
            nid.szTip[i] = c;
        }

        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            return false;
        }
        // 按当前监听状态刷新图标（禁用时变灰）和提示
        update_tray_icon();
        true
    }
}

/// 低层鼠标钩子回调——由托盘线程的消息泵在收到鼠标事件时调用
/// 通过原子变量将鼠标按键状态传递给主线程的轮询循环。
/// 同时检测 Alt+按键组合，直接设置拖拽请求标志，
/// 避免轮询循环因钩子事件延迟而漏检。
unsafe extern "system" fn low_level_mouse_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 {
        match wparam.0 as u32 {
            0x0201 => {  // WM_LBUTTONDOWN
                MOUSE_LEFT_HOOK.store(true, Ordering::SeqCst);
                if is_alt_held() {
                    HOOK_DRAG_REQUEST.store(1, Ordering::SeqCst);
                    return LRESULT(1); // 吞掉消息，阻断窗口内部交互
                }
            }
            0x0202 => {  // WM_LBUTTONUP
                MOUSE_LEFT_HOOK.store(false, Ordering::SeqCst);
                if is_alt_held() {
                    return LRESULT(1); // 同步吞掉弹起消息
                }
            }
            0x0204 => {  // WM_RBUTTONDOWN
                MOUSE_RIGHT_HOOK.store(true, Ordering::SeqCst);
                if is_alt_held() {
                    HOOK_DRAG_REQUEST.store(2, Ordering::SeqCst);
                    return LRESULT(1); // 吞掉消息
                }
            }
            0x0205 => {  // WM_RBUTTONUP
                MOUSE_RIGHT_HOOK.store(false, Ordering::SeqCst);
                if is_alt_held() {
                    return LRESULT(1); // 同步吞掉弹起消息
                }
            }
            _ => {}
        }
    }
    let hhk = HHOOK(HOOK_HANDLE.load(Ordering::Relaxed) as *mut std::ffi::c_void);
    unsafe { CallNextHookEx(hhk, code, wparam, lparam) }
}
