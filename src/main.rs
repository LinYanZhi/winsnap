//! winsnap — 窗口位置和大小管理工具
//!
//! 默认无控制台窗口（仅托盘图标），加 -c 参数显示控制台。
//! 交互模式：Alt+鼠标拖拽移动/调整大小，Alt+方向键快速贴边，
//!           Alt+小键盘数字快速定位 (1-9)，Alt+滚轮等比例缩放。
//! 所有配置仅存内存，关闭即清空。
//!
//! 模块划分（每个文件职责单一，避免单个文件过长）：
//! - state.rs    全局原子状态与修饰键/鼠标按键查询
//! - log.rs      日志辅助（时间戳、ANSI 颜色、窗口信息）
//! - anim.rs     窗口移动动画（后台线程平滑移动）
//! - single.rs   单例控制（互斥体 + 托盘接管）
//! - autostart.rs 开机自启（启动文件夹 + 注册表）
//! - keyboard.rs rdev 键盘/滚轮事件分发与按键动作
//! - snap.rs     窗口吸附（矩形区间裁剪实现被遮挡边缘的可见性判断）
//! - tray.rs     系统托盘（隐藏窗口 + 菜单 + 低层鼠标钩子 + 看门狗）

#![windows_subsystem = "windows"]

mod anim;
mod autostart;
mod cmd;
mod config;
mod keyboard;
mod log;
mod single;
mod snap;
mod state;
mod tray;

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rdev::Key;
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::System::Console::GetConsoleWindow;
use windows::Win32::UI::WindowsAndMessaging::ShowWindow;

// 提高系统定时器分辨率到 1ms：Windows 默认为 15.6ms，导致 sleep 实际间隔
// 忽大忽小（如 sleep(22) 实际 31ms 或 22ms 交替），动画帧间隔抖动产生卡顿感。
// 动画线程和拖拽轮询都依赖精确 sleep，声明后即可均匀推进。
#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}

use crate::log::{
    c, now, CLR_INTERACT, CLR_NUDGE, CLR_POSITION, CLR_SCALE, CLR_TIP,
};
use crate::snap::{
    ensure_snap_candidates, refresh_candidate_visibility, snap_anchor_x, snap_anchor_y,
    snap_correction, store_drag_visual, DRAG_VIS_OFF_L, DRAG_VIS_OFF_T, DRAG_VIS_H, DRAG_VIS_W,
    LAST_SNAP_DIAG, SNAP_CANDIDATES, SNAP_ESCAPE, SNAP_H, SNAP_SCREEN_ENABLED, SNAP_THRESHOLD,
    SNAP_V, SNAP_WINDOW_ENABLED,
};
use crate::state::{
    is_alt_held, is_left_down, is_right_down, DRAG_HT, DRAG_HWND, DRAG_LEFT_UP_COUNT, DRAG_MODE,
    DRAG_START_MX, DRAG_START_MY, DRAG_WIN_B, DRAG_WIN_L, DRAG_WIN_R, DRAG_WIN_T, HOOK_DRAG_REQUEST,
    HOOK_PAUSED, MOUSE_LEFT_HOOK, MOUSE_RIGHT_HOOK, ReleaseCapture,
};
use crate::tray::{spawn_tray, spawn_tray_watchdog};

use config as cfg;

// ── 入口 ────────────────────────────────────────────

fn main() {
    // 声明 Per-Monitor V2 DPI 感知：使 GetWindowRect / GetSystemMetrics / GetMonitorInfoW
    // 等返回物理像素，与 DwmGetWindowAttribute(EXTENDED_FRAME_BOUNDS) 保持同一坐标系。
    // 否则在系统缩放（如 125%）下 GetWindowRect 返回虚拟化坐标而 DWM 返回物理像素，
    // 导致 DWM 边框偏移计算错乱、缩放/贴边尺寸不准。须在创建任何窗口前调用。
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // CLI 一次性命令模式（供 AI/脚本调用窗口控制）——不启动托盘/交互，用完即退
    let cli_args: Vec<String> = std::env::args().collect();
    if cli_args.iter().any(|a| a == "--cmd") {
        unsafe {
            use windows::Win32::System::Console::AttachConsole;
            let _ = AttachConsole(windows::Win32::System::Console::ATTACH_PARENT_PROCESS);
        }
        std::process::exit(crate::cmd::run(&cli_args));
    }

    // 提高系统定时器分辨率到 1ms，保证动画 sleep 帧间隔精确均匀（进程退出时系统自动恢复）
    unsafe { let _ = timeBeginPeriod(1); }

    // 单例控制：已有实例时请其重建托盘图标，托盘失效则接管，无法接管则退出
    if !single::ensure_single_instance() {
        return;
    }
    // 尝试继承父进程控制台（从 cmd/powershell 启动时）
    unsafe {
        use windows::Win32::System::Console::AttachConsole;
        let _ = AttachConsole(windows::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }

    // 解析参数
    let args: Vec<String> = std::env::args().collect();
    let show_console = args.iter().any(|a| a == "-c" || a == "--console");

    // -l <path>：将 stdout 重定向到日志文件（后台/无人值守运行时收集日志，
    // 避免依赖控制台窗口。必须在任何 println 之前设置；句柄保持打开，进程退出时由系统关闭）
    if let Some(i) = args.iter().position(|a| a == "-l") {
        if let Some(path) = args.get(i + 1) {
            use std::os::windows::io::AsRawHandle;
            use windows::Win32::System::Console::{SetStdHandle, STD_OUTPUT_HANDLE};
            if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let h = windows::Win32::Foundation::HANDLE(
                    f.as_raw_handle() as *mut std::ffi::c_void,
                );
                std::mem::forget(f); // 防止 File drop 关闭句柄
                unsafe { let _ = SetStdHandle(STD_OUTPUT_HANDLE, h); }
            }
        }
    }

    if show_console {
        unsafe {
            use windows::Win32::System::Console::AllocConsole;
            if GetConsoleWindow().0.is_null() {
                let _ = AllocConsole();
            }
        }
    }

    let has_console = unsafe { !GetConsoleWindow().0.is_null() };

    if has_console {
        color::enable_ansi();
        println!("{} 窗口管理工具已启动——Alt+鼠标拖拽移动/调整大小，Alt+方向键快速贴边", c("winsnap", CLR_INTERACT));
        println!("  Alt + 小键盘数字键：{}", c("快速定位", CLR_POSITION));
        println!("  Alt + 滚轮：{}", c("等比例缩放", CLR_SCALE));
        println!("  Alt + 方向键：{}", c("快速贴边", CLR_POSITION));
        println!("  Alt + 鼠标左键拖拽：{}", c("移动窗口", CLR_POSITION));
        println!("  Alt + 鼠标右键拖拽：{}", c("调整窗口大小", CLR_SCALE));
        println!("{} 输入 ` 可暂停/恢复快捷键和鼠标监听", c("提示：", CLR_TIP));
        println!("通过托盘右键菜单可退出程序");
    }

    // 注册控制台关闭事件处理器（点击 X 只隐藏窗口，不退出进程）
    let _ = unsafe {
        use windows::Win32::System::Console::SetConsoleCtrlHandler;
        SetConsoleCtrlHandler(Some(console_ctrl_handler), BOOL(1))
    };

    run_interactive();
    // 恢复系统默认定时器分辨率（提前 return 的路径由进程退出时系统自动恢复）
    unsafe { let _ = timeEndPeriod(1); }
}

// ── 交互模式 ────────────────────────────────────────
//
// 键盘事件：通过 rdev 异步监听
// 鼠标事件：通过 Win32 API（GetAsyncKeyState + GetCursorPos）在主线程轮询
//          不依赖 rdev 的鼠标事件，避免事件丢失问题

/// 控制台关闭事件处理器（点击 X 时触发）
/// 先隐藏窗口，再异步断开控制台，防止进程被终止
unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    if ctrl_type == windows::Win32::System::Console::CTRL_CLOSE_EVENT {
        unsafe {
            let console = GetConsoleWindow();
            if !console.0.is_null() {
                let _ = ShowWindow(console, windows::Win32::UI::WindowsAndMessaging::SW_HIDE);
            }
        }
        // 在单独的线程中延迟断开控制台，避免在 Ctrl 处理器线程中直接
        // FreeConsole 导致后续 AllocConsole 在某些 Windows 版本上失败
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            unsafe {
                use windows::Win32::System::Console::FreeConsole;
                let _ = FreeConsole();
            }
        });
        BOOL(1)
    } else {
        BOOL(0)
    }
}

fn run_interactive() {
    println!("{}", c("交互模式已启动", CLR_INTERACT));

    let modifiers: Arc<Mutex<HashSet<Key>>> = Arc::new(Mutex::new(HashSet::new()));
    let m = modifiers.clone();

    // rdev 线程——只处理键盘和滚轮事件
    thread::spawn(move || {
        if let Err(e) = rdev::listen(move |event| {
            keyboard::handle_key_event(event, &m);
        }) {
            eprintln!("监听失败: {e:?}");
        }
    });

    // 系统托盘图标线程
    spawn_tray();
    spawn_tray_watchdog();
    // 主线程：轮询鼠标状态 + 心跳诊断
    let mut tick = 0u64;
    loop {
        if !HOOK_PAUSED.load(Ordering::Relaxed) {
            poll_mouse(&mut tick);
        }
        thread::sleep(Duration::from_millis(8)); // ~120Hz
    }
}

// ── 鼠标轮询 ────────────────────────────────────────

fn poll_mouse(tick: &mut u64) {
    *tick += 1;
    let alt = is_alt_held();
    let left = is_left_down();
    let right = is_right_down();
    let mode = DRAG_MODE.load(Ordering::Relaxed);
    let hook_l = MOUSE_LEFT_HOOK.load(Ordering::Relaxed);
    let hook_r = MOUSE_RIGHT_HOOK.load(Ordering::Relaxed);

    // 每 ~5 秒输出一次诊断信息（约 625 ticks）
    if *tick % 625 == 0 {
        let (mx, my) = cfg::get_cursor_pos();
        println!("[{}] [DIAG] alt={alt} left={left} right={right} mode={mode} hook_l={hook_l} hook_r={hook_r} cursor=({mx},{my})",
            now());
    }

    if mode == 0 {
        // ── 先检查钩子回调触发的拖拽请求（零延迟，精确同步） ──
        let drag_req = HOOK_DRAG_REQUEST.swap(0, Ordering::AcqRel);
        if drag_req != 0 {
            println!("[{}] [HOOK-REQ] drag_req={drag_req} alt={alt} hook_l={hook_l} hook_r={hook_r} left={left} right={right} cursor=({},{})",
                now(), cfg::get_cursor_pos().0, cfg::get_cursor_pos().1);
        }
        if drag_req == 1 && alt {
            // 钩子确认 Alt 按下时左键被点击 → 直接开始拖拽移动
            let hwnd = {
                let (mx, my) = cfg::get_cursor_pos();
                cfg::get_root_window(cfg::window_from_point(mx, my))
            };
            if !hwnd.0.is_null() {
                let (mx, my) = cfg::get_cursor_pos();
                let (l, t, r, b) = cfg::get_window_rect(hwnd);
                DRAG_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
                DRAG_START_MX.store(mx, Ordering::Relaxed);
                DRAG_START_MY.store(my, Ordering::Relaxed);
                DRAG_WIN_L.store(l, Ordering::Relaxed);
                DRAG_WIN_T.store(t, Ordering::Relaxed);
                DRAG_WIN_R.store(r, Ordering::Relaxed);
                DRAG_WIN_B.store(b, Ordering::Relaxed);
                DRAG_MODE.store(1, Ordering::Relaxed);
                DRAG_LEFT_UP_COUNT.store(0, Ordering::Relaxed);
                SNAP_H.store(0, Ordering::Relaxed);
                SNAP_V.store(0, Ordering::Relaxed);
                store_drag_visual(hwnd, l, t);
                let pn = cfg::get_process_name(hwnd);
                println!("[{}] {} Alt+左键拖拽开始(钩子触发): {} 窗口=({l},{t}) cursor=({mx},{my})",
                    now(), c("[DRAG]", CLR_NUDGE), c(&format!("[{pn}]"), CLR_POSITION));
                unsafe { ReleaseCapture(); }
                return;
            }
        } else if drag_req == 2 && alt {
            // 钩子确认 Alt 按下时右键被点击 → 直接开始调整大小
            let hwnd = {
                let (mx, my) = cfg::get_cursor_pos();
                cfg::get_root_window(cfg::window_from_point(mx, my))
            };
            if !hwnd.0.is_null() {
                let (mx, my) = cfg::get_cursor_pos();
                let (l, t, r, b) = cfg::get_window_rect(hwnd);
                let ht = cfg::resize_direction(mx, my, l, t, r, b);
                DRAG_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
                DRAG_START_MX.store(mx, Ordering::Relaxed);
                DRAG_START_MY.store(my, Ordering::Relaxed);
                DRAG_WIN_L.store(l, Ordering::Relaxed);
                DRAG_WIN_T.store(t, Ordering::Relaxed);
                DRAG_WIN_R.store(r, Ordering::Relaxed);
                DRAG_WIN_B.store(b, Ordering::Relaxed);
                DRAG_HT.store(ht, Ordering::Relaxed);
                DRAG_MODE.store(2, Ordering::Relaxed);
                return;
            }
        }

        // ── 原轮询检测（作为钩子触发的补充/兜底） ──
        if alt && left && !right {
            // Alt + 左键 → 开始拖拽移动
            let hwnd = {
                let (mx, my) = cfg::get_cursor_pos();
                cfg::get_root_window(cfg::window_from_point(mx, my))
            };
            if !hwnd.0.is_null() {
                let (mx, my) = cfg::get_cursor_pos();
                let (l, t, r, b) = cfg::get_window_rect(hwnd);
                DRAG_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
                DRAG_START_MX.store(mx, Ordering::Relaxed);
                DRAG_START_MY.store(my, Ordering::Relaxed);
                DRAG_WIN_L.store(l, Ordering::Relaxed);
                DRAG_WIN_T.store(t, Ordering::Relaxed);
                DRAG_WIN_R.store(r, Ordering::Relaxed);
                DRAG_WIN_B.store(b, Ordering::Relaxed);
                DRAG_MODE.store(1, Ordering::Relaxed);
                DRAG_LEFT_UP_COUNT.store(0, Ordering::Relaxed);
                SNAP_H.store(0, Ordering::Relaxed);
                SNAP_V.store(0, Ordering::Relaxed);
                store_drag_visual(hwnd, l, t);
                let pn = cfg::get_process_name(hwnd);
                println!("[{}] {} 检测到 Alt+左键拖拽开始: {} 窗口=({l},{t}) cursor=({mx},{my})",
                    now(), c("[DRAG]", CLR_NUDGE), c(&format!("[{pn}]"), CLR_POSITION));

                // 释放鼠标捕获，阻断窗口内部交互
                unsafe { ReleaseCapture(); }
                return;
            }
        } else if alt && right && !left {
            // Alt + 右键 → 开始拖拽调整大小
            let hwnd = {
                let (mx, my) = cfg::get_cursor_pos();
                cfg::get_root_window(cfg::window_from_point(mx, my))
            };
            if !hwnd.0.is_null() {
                let (mx, my) = cfg::get_cursor_pos();
                let (l, t, r, b) = cfg::get_window_rect(hwnd);
                let ht = cfg::resize_direction(mx, my, l, t, r, b);
                DRAG_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
                DRAG_START_MX.store(mx, Ordering::Relaxed);
                DRAG_START_MY.store(my, Ordering::Relaxed);
                DRAG_WIN_L.store(l, Ordering::Relaxed);
                DRAG_WIN_T.store(t, Ordering::Relaxed);
                DRAG_WIN_R.store(r, Ordering::Relaxed);
                DRAG_WIN_B.store(b, Ordering::Relaxed);
                DRAG_HT.store(ht, Ordering::Relaxed);
                DRAG_MODE.store(2, Ordering::Relaxed);
                return;
            }
        }
    } else {
        let hwnd_val = DRAG_HWND.load(Ordering::Relaxed);
        if hwnd_val == 0 {
            DRAG_MODE.store(0, Ordering::Relaxed);
            return;
        }
        let hwnd = HWND(hwnd_val as *mut std::ffi::c_void);

        // ── 计算当前鼠标偏移（用于诊断和移动更新） ──
        let (cur_mx, cur_my) = cfg::get_cursor_pos();
        let start_mx = DRAG_START_MX.load(Ordering::Relaxed);
        let start_my = DRAG_START_MY.load(Ordering::Relaxed);
        let dx = cur_mx - start_mx;
        let dy = cur_my - start_my;

        // ── 检测拖拽结束 ──
        let mut should_end = if !alt {
            true
        } else {
            match mode {
                1 => !left,  // 左键释放 → 检查去抖
                2 => !right, // 右键释放 → 立即结束
                _ => true,
            }
        };
        // 左键拖拽（mode=1）加入去抖：MOUSE_LEFT_HOOK 短暂抖动（<24ms）不结束拖拽
        if mode == 1 && should_end && !alt {
            // Alt 已释放 → 立即结束
        } else if mode == 1 && should_end && alt {
            // 左键看似弹起，但可能只是钩子状态抖动
            let count = DRAG_LEFT_UP_COUNT.fetch_add(1, Ordering::Relaxed);
            if count < 2 {
                // 连续弹起次数 < 3（24ms）→ 视为抖动，继续拖拽
                should_end = false;
            } else {
                // 连续弹起 >= 3 次 → 确认左键已释放，结束拖拽
                DRAG_LEFT_UP_COUNT.store(0, Ordering::Relaxed);
            }
        } else if mode == 1 {
            // 左键正常按下 → 重置去抖计数器
            DRAG_LEFT_UP_COUNT.store(0, Ordering::Relaxed);
        }

        if should_end {
            let up_count = DRAG_LEFT_UP_COUNT.load(Ordering::Relaxed);
            if mode == 1 {
                println!("[{}] {} 拖拽结束(alt={alt} left={left} up_count={up_count} dx={dx} dy={dy})",
                    now(), c("[DRAG]", CLR_NUDGE));
                snap_correction(hwnd);
            } else if mode == 2 {
                println!("[{}] {} 调整大小结束", now(), c("[DRAG]", CLR_NUDGE));
            }
            // 清除窗口吸附候选缓存（下次拖拽重新收集）
            SNAP_CANDIDATES.lock().unwrap().clear();
            DRAG_MODE.store(0, Ordering::Relaxed);
            DRAG_HWND.store(0, Ordering::Relaxed);
            return;
        }

        // ── 更新拖拽位置 ──
        if dx.abs() < 2 && dy.abs() < 2 { return; }

        if mode == 1 {
            // ── 移动窗口 + 拖拽中实时贴边（全部基于可视矩形：DWM 可视区域，排除不可见边框） ──
            let l = DRAG_WIN_L.load(Ordering::Relaxed);
            let t = DRAG_WIN_T.load(Ordering::Relaxed);
            let mut nx = l + dx;
            let mut ny = t + dy;

            // 拖拽窗口当前可视矩形 = 帧目标位置 + 拖拽开始时记录的可视偏移/尺寸
            let off_lo = DRAG_VIS_OFF_L.load(Ordering::Relaxed);
            let off_to = DRAG_VIS_OFF_T.load(Ordering::Relaxed);
            let vw = DRAG_VIS_W.load(Ordering::Relaxed);
            let vh = DRAG_VIS_H.load(Ordering::Relaxed);
            let mut vx = nx + off_lo; // 可视左边缘
            let mut vy = ny + off_to; // 可视上边缘

            let (sw, sht) = cfg::screen_size();
            let screen_snap_on = SNAP_SCREEN_ENABLED.load(Ordering::Relaxed);

            // 窗口吸附开启时收集候选窗口（拖拽开始惰性收集一次，借用整个移动块）
            let window_snap_on = SNAP_WINDOW_ENABLED.load(Ordering::Relaxed);
            let cand_guard;
            if window_snap_on {
                ensure_snap_candidates();
                // 每帧重算候选四边可见性（内部自行加锁 SNAP_CANDIDATES/SNAP_ALL），
                // 必须在外部 lock 之前调用，否则死锁
                refresh_candidate_visibility();
                cand_guard = Some(SNAP_CANDIDATES.lock().unwrap());
            } else {
                cand_guard = None;
            }
            let cands: &[snap::SnapCandidate] = cand_guard.as_deref().map(|v| v.as_slice()).unwrap_or(&[]);

            // 拖拽诊断（节流 1s）：输出拖拽窗口可视矩形、移动量、候选及四边可见性。
            // 用于定位"右贴右/下贴下不生效"类问题：若某候选右/下可见性为 0，
            // 说明该边被其它窗口盖住（遮挡过滤生效）；若全为 1 但仍不吸，则是锚点逻辑问题
            let diag_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            if diag_ms - LAST_SNAP_DIAG.load(Ordering::Relaxed) > 1000 {
                LAST_SNAP_DIAG.store(diag_ms, Ordering::Relaxed);
                let cand_desc: Vec<String> = cands
                    .iter()
                    .map(|c| {
                        format!(
                            "{}[左{}上{}右{}下{}]",
                            cfg::get_process_name(HWND(c.hwnd as *mut std::ffi::c_void)),
                            c.vis_l as u8, c.vis_t as u8, c.vis_r as u8, c.vis_b as u8
                        )
                    })
                    .collect();
                println!(
                    "[{}] {} 拖拽诊断: 可视=({vx},{vy},{vw}x{vh}) dx={dx} dy={dy} 候选 {} 个: {}",
                    now(), c("[SNAP]", CLR_NUDGE),
                    cands.len(), cand_desc.join(" ")
                );
            }

            // 水平贴边：可视边缘贴近最近锚点（屏幕边缘或垂直重叠的窗口边缘）即吸附
            let sh = SNAP_H.load(Ordering::Relaxed);
            if sh == 0 {
                if let Some((target_vx, dir, dist)) =
                    snap_anchor_x(dx, vx, vw, vy, vh, sw, screen_snap_on, cands)
                {
                    if dist <= SNAP_THRESHOLD {
                        vx = target_vx;
                        nx = vx - off_lo;
                        SNAP_H.store(dir, Ordering::Relaxed);
                        println!("[{}] {} 贴边-{}(dist={dist})", now(), c("[SNAP]", CLR_NUDGE), if dir == 1 { "左" } else { "右" });
                    }
                }
            } else {
                match snap_anchor_x(0, vx, vw, vy, vh, sw, screen_snap_on, cands) {
                    Some((target_vx, dir, dist)) => {
                        if dist > SNAP_ESCAPE {
                            SNAP_H.store(0, Ordering::Relaxed);
                            println!("[{}] {} 脱开贴边", now(), c("[SNAP]", CLR_NUDGE));
                        } else {
                            vx = target_vx;
                            nx = vx - off_lo;
                            if dir != sh {
                                SNAP_H.store(dir, Ordering::Relaxed);
                            }
                        }
                    }
                    None => {
                        // 无任何锚点（吸附开关全部关闭）→ 脱开
                        SNAP_H.store(0, Ordering::Relaxed);
                        println!("[{}] {} 脱开贴边", now(), c("[SNAP]", CLR_NUDGE));
                    }
                }
            }

            // 垂直贴边（同理）
            let sv = SNAP_V.load(Ordering::Relaxed);
            if sv == 0 {
                if let Some((target_vy, dir, dist)) =
                    snap_anchor_y(dy, vy, vh, vx, vw, sht, screen_snap_on, cands)
                {
                    if dist <= SNAP_THRESHOLD {
                        vy = target_vy;
                        ny = vy - off_to;
                        SNAP_V.store(dir, Ordering::Relaxed);
                        println!("[{}] {} 贴边-{}(dist={dist})", now(), c("[SNAP]", CLR_NUDGE), if dir == 1 { "上" } else { "下" });
                    }
                }
            } else {
                match snap_anchor_y(0, vy, vh, vx, vw, sht, screen_snap_on, cands) {
                    Some((target_vy, dir, dist)) => {
                        if dist > SNAP_ESCAPE {
                            SNAP_V.store(0, Ordering::Relaxed);
                            println!("[{}] {} 脱开贴边", now(), c("[SNAP]", CLR_NUDGE));
                        } else {
                            vy = target_vy;
                            ny = vy - off_to;
                            if dir != sv {
                                SNAP_V.store(dir, Ordering::Relaxed);
                            }
                        }
                    }
                    None => {
                        SNAP_V.store(0, Ordering::Relaxed);
                        println!("[{}] {} 脱开贴边", now(), c("[SNAP]", CLR_NUDGE));
                    }
                }
            }

            cfg::move_window_to(hwnd, nx, ny);
        } else if mode == 2 {
            let l = DRAG_WIN_L.load(Ordering::Relaxed);
            let t = DRAG_WIN_T.load(Ordering::Relaxed);
            let r = DRAG_WIN_R.load(Ordering::Relaxed);
            let b = DRAG_WIN_B.load(Ordering::Relaxed);
            let ht = DRAG_HT.load(Ordering::Relaxed);

            let (mut nl, mut nt, mut nr, mut nb) = (l, t, r, b);
            match ht {
                10 => nl = l + dx,
                11 => nr = r + dx,
                12 => nt = t + dy,
                13 => { nl = l + dx; nt = t + dy; }
                14 => { nr = r + dx; nt = t + dy; }
                15 => nb = b + dy,
                16 => { nl = l + dx; nb = b + dy; }
                17 => { nr = r + dx; nb = b + dy; }
                _ => {}
            }

            const MIN_W: i32 = 100;
            const MIN_H: i32 = 100;
            if nr - nl < MIN_W { nr = nl + MIN_W; }
            if nb - nt < MIN_H { nb = nt + MIN_H; }

            cfg::set_window_pos(hwnd, nl, nt, nr - nl, nb - nt);
        }
    }
}
