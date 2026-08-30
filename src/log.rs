//! 日志辅助 — 时间戳、ANSI 颜色、窗口信息与变化格式化

use color::Style;
use windows::Win32::Foundation::HWND;

use crate::config as cfg;

// ── 颜色常量 ──

pub const CLR_INTERACT: u8 = 96;
pub const CLR_SCALE: u8 = 95;
pub const CLR_POSITION: u8 = 92;
pub const CLR_NUDGE: u8 = 93;
pub const CLR_SUCCESS: u8 = 92;
pub const CLR_FAIL: u8 = 91;
pub const CLR_PAUSE: u8 = 93;
pub const CLR_RESUME: u8 = 92;
pub const CLR_TIP: u8 = 93;

/// 本地时间戳（东八区硬编码）
pub fn now() -> String {
    use std::time::SystemTime;
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = t.as_secs() as i64;
    let total = secs + 8 * 3600;
    let days = total / 86400;
    let time_secs = total % 86400;
    let h = time_secs / 3600;
    let m = (time_secs % 3600) / 60;
    let s = time_secs % 60;

    let mut y = 1970i64;
    let mut d = days;
    loop {
        let yd = if is_leap(y) { 366 } else { 365 };
        if d < yd { break; }
        d -= yd;
        y += 1;
    }
    let leap = is_leap(y);
    let mdays: [i64; 12] = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 1;
    for &md in &mdays {
        if d < md { break; }
        d -= md;
        mo += 1;
    }
    format!("{y:04}-{mo:02}-{:02} {h:02}:{m:02}:{s:02}", d + 1)
}

fn is_leap(y: i64) -> bool { (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 }

/// ANSI 着色
pub fn c(text: &str, clr: u8) -> String { Style::new(clr).paint(text) }

/// 窗口信息：按句柄取稳定颜色 + 进程名 + 标题
pub fn win_info(hwnd: HWND, process: &str, title: &str) -> String {
    let h = hwnd.0 as usize;
    let colors: [u8; 12] = [94, 91, 93, 92, 96, 95, 31, 32, 33, 34, 35, 36];
    let clr = colors[h.wrapping_mul(3) % colors.len()];
    format!("{} {}",
        Style::new(clr).paint(format!("[{process}]")), title)
}

pub fn format_change(old: (i32, i32, i32, i32), new: (i32, i32, i32, i32)) -> String {
    let (ol, ot, orr, ob) = old;
    let (nl, nt, nr, nb) = new;
    let ow = orr - ol;
    let oh = ob - ot;
    let nw = nr - nl;
    let nh = nb - nt;
    format!("({ol},{ot},{ow}x{oh}) -> ({nl},{nt},{nw}x{nh})")
}

pub fn log_change(mode: &str, mode_clr: u8, action: &str, action_clr: u8, hwnd: HWND, pn: &str, title: &str, old: (i32, i32, i32, i32), new: (i32, i32, i32, i32)) {
    let info = win_info(hwnd, pn, title);
    let change = format_change(old, new);
    println!("[{}] {} {} {} {}",
        now(), c(&format!("[{mode}]"), mode_clr), c(&format!("[{action}]"), action_clr), info, change);
}

pub fn log_iaction(label: &str, action_clr: u8, hwnd: HWND, old: (i32, i32, i32, i32), new: (i32, i32, i32, i32)) {
    let pn = cfg::get_process_name(hwnd);
    let title = cfg::get_window_title(hwnd);
    log_change("交互", CLR_INTERACT, label, action_clr, hwnd, &pn, &title, old, new);
}

pub fn log_iaction_detail(action: &str, action_clr: u8, detail: &str, hwnd: HWND, old: (i32, i32, i32, i32), new: (i32, i32, i32, i32)) {
    let pn = cfg::get_process_name(hwnd);
    let title = cfg::get_window_title(hwnd);
    let info = win_info(hwnd, &pn, &title);
    let change = format_change(old, new);
    println!("[{}] {} {} {} {} {}",
        now(), c("[交互]", CLR_INTERACT), c(&format!("[{action}]"), action_clr), detail, info, change);
}
