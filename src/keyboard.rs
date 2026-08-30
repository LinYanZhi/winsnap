//! rdev 键盘/滚轮事件分发与按键动作（定位、贴边、缩放、监听开关）

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rdev::Key;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, PostMessageW, WM_CANCELMODE};

use crate::anim::animated_move;
use crate::log::{
    c, log_change, log_iaction, log_iaction_detail, now,
    CLR_INTERACT, CLR_NUDGE, CLR_PAUSE, CLR_POSITION, CLR_RESUME, CLR_SCALE, CLR_SUCCESS, CLR_TIP,
};
use crate::state::{is_alt_held, is_ctrl_held, HOOK_PAUSED};
use crate::tray::{CURRENT_MENU, TRAY_WINDOW, WM_TRAY_REFRESH};
use crate::config as cfg;

/// rdev 事件入口：键盘按下（修饰键追踪、` 暂停开关）、释放、滚轮
pub fn handle_key_event(event: rdev::Event, modifiers: &Arc<Mutex<HashSet<Key>>>) {
    match event.event_type {
        rdev::EventType::KeyPress(key) => {
            let mut m = modifiers.lock().unwrap();
            m.insert(key);

            if key == Key::BackQuote {
                let was_paused = HOOK_PAUSED.swap(!HOOK_PAUSED.load(Ordering::Relaxed), Ordering::Relaxed);
                if was_paused {
                    println!("\n{} 按键监听 {} - 输入 ` 再次切换",
                        c("[控制]", CLR_INTERACT), c("已恢复", CLR_RESUME));
                    println!("{} 快捷键和鼠标监听已恢复", c("提示：", CLR_TIP));
                } else {
                    println!("\n{} 按键监听 {} - 输入 ` 再次切换",
                        c("[控制]", CLR_INTERACT), c("已暂停", CLR_PAUSE));
                    println!("{} 现在可以用 Ctrl+滚轮 调整控制台字体大小了！", c("提示：", CLR_TIP));
                }
                // 通知托盘线程：若菜单正开着先关闭（弹出菜单显示期间无法实时刷新勾选，
                // 关闭后重新打开即显示最新状态），再刷新图标（禁用变灰）和悬浮提示
                let tray = TRAY_WINDOW.load(Ordering::Relaxed);
                if tray != 0 {
                    unsafe {
                        let hwnd = HWND(tray as *mut std::ffi::c_void);
                        if CURRENT_MENU.load(Ordering::Relaxed) != 0 {
                            let _ = PostMessageW(hwnd, WM_CANCELMODE, WPARAM(0), LPARAM(0));
                        }
                        let _ = PostMessageW(hwnd, WM_TRAY_REFRESH, WPARAM(0), LPARAM(0));
                    }
                }
                drop(m);
                return;
            }

            drop(m);

            if HOOK_PAUSED.load(Ordering::Relaxed) { return; }
            on_keypress(key, modifiers);
        }
        rdev::EventType::KeyRelease(key) => {
            modifiers.lock().unwrap().remove(&key);
        }
        rdev::EventType::Wheel { delta_y, .. } => {
            if HOOK_PAUSED.load(Ordering::Relaxed) { return; }
            if is_alt_held() { on_scroll(delta_y); }
        }
        _ => {}
    }
}

fn on_keypress(key: Key, _modifiers: &Arc<Mutex<HashSet<Key>>>) {
    let alt = is_alt_held();
    let ctrl = is_ctrl_held();

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() { return; }

        if alt && !ctrl {
            if let Some(num) = key_to_num(key) {
                if let Some((x, y)) = cfg::pos_by_key(num, hwnd) {
                    let old = cfg::get_window_rect(hwnd);
                    // 动画开启时窗口异步移动，最终矩形按目标位置预测（尺寸不变）用于日志
                    let new = (x, y, x + (old.2 - old.0), y + (old.3 - old.1));
                    animated_move(hwnd, x, y);
                    if old != new {
                        let pn = cfg::get_process_name(hwnd);
                        let title = cfg::get_window_title(hwnd);
                        log_change("交互", CLR_INTERACT, &format!("定位-{num}"), CLR_POSITION, hwnd, &pn, &title, old, new);
                    }
                }
                return;
            }

            match key {
                Key::LeftArrow => snap_to_edge(hwnd, 0),
                Key::RightArrow => snap_to_edge(hwnd, 1),
                Key::UpArrow => snap_to_edge(hwnd, 2),
                Key::DownArrow => snap_to_edge(hwnd, 3),
                _ => {}
            }
        }
    }
}

/// Alt+方向键快速贴屏幕边缘（基于 DWM 可视矩形，方向 0左 1右 2上 3下）
fn snap_to_edge(hwnd: HWND, dir: u32) {
    let old = cfg::get_window_rect(hwnd);
    let (l, t, r, b) = old;
    let w = r - l;
    let h = b - t;
    let (sw, sh) = cfg::screen_size();
    let (lo, to, ro, bo) = cfg::get_dwm_frame_offsets(hwnd);
    let pn = cfg::get_process_name(hwnd);
    println!("[{}] {} {} DWM偏移=(lo={lo},to={to},ro={ro},bo={bo}) 窗口帧=({l},{t},{w}x{h}) 屏幕=({sw}x{sh})",
        now(), c("[DWM]", CLR_NUDGE), c(&format!("[{pn}]"), CLR_POSITION));
    let (nx, ny) = match dir {
        0 => (-lo, t),
        1 => (sw - w + ro, t),
        2 => (l, -to),
        3 => (l, sh - h + bo),
        _ => return,
    };
    // 动画开启时窗口异步移动，最终矩形按目标位置预测（尺寸不变）用于日志
    let new = (nx, ny, nx + w, ny + h);
    animated_move(hwnd, nx, ny);
    if old != new {
        let labels = ["左", "右", "上", "下"];
        log_iaction(labels[dir as usize], CLR_NUDGE, hwnd, old, new);
    }
}

/// 快速连续滚轮时抑制缩放日志：控制台 I/O 会拖慢 rdev 事件线程，导致缩放响应迟钝
static LAST_SCROLL_LOG: AtomicU64 = AtomicU64::new(0);

/// Alt+滚轮等比例缩放（瞬缩，不做动画）
fn on_scroll(dy: i64) {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() { return; }

        let old = cfg::get_window_rect(hwnd);
        let scale_factor = if dy > 0 { cfg::SCALE_STEP } else { -cfg::SCALE_STEP };
        if cfg::scale_window(hwnd, scale_factor) {
            let new = cfg::get_window_rect(hwnd);
            if old != new {
                // 距上次打印 < 250ms 视为连续滚动，跳过日志避免控制台 I/O 拖慢事件线程
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let last = LAST_SCROLL_LOG.load(Ordering::Relaxed);
                if now_ms.saturating_sub(last) >= 250 {
                    LAST_SCROLL_LOG.store(now_ms, Ordering::Relaxed);
                    let action = if dy > 0 { c("放大", CLR_SUCCESS) } else { c("缩小", CLR_PAUSE) };
                    log_iaction_detail("缩放", CLR_SCALE, &action, hwnd, old, new);
                }
            }
        }
    }
}

// ── 键盘码 → 数字 ──

fn key_to_num(key: Key) -> Option<char> {
    match key {
        Key::Kp1 => Some('1'),
        Key::Kp2 => Some('2'),
        Key::Kp3 => Some('3'),
        Key::Kp4 => Some('4'),
        Key::Kp5 => Some('5'),
        Key::Kp6 => Some('6'),
        Key::Kp7 => Some('7'),
        Key::Kp8 => Some('8'),
        Key::Kp9 => Some('9'),
        _ => None,
    }
}
