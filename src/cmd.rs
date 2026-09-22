//! winsnap CLI 模式（--cmd）— 一次性窗口控制命令，供 AI/脚本调用。
//! 复用 config.rs 的核心（get_visual_rect 处理 DWM 边框、get_monitor_work_area 按显示器取工作区）。
//! 命令：
//!   winsnap --cmd snap --pid <n>|--title <s> --align left|right|center|full|left-edge|right-edge|proportional [--scale N] [--gap N]
//!   winsnap --cmd list [--pid N] [--title S] [--top N]
//!   winsnap --cmd screen
//! 输出：stdout JSON。D:\ 无关——由 main.rs 在 --cmd 时 attach 父控制台。

use std::cell::RefCell;

use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, ShowWindow, SW_RESTORE, GA_ROOT,
};

use crate::config as cfg;

#[derive(Clone)]
pub struct WinInfo {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

thread_local! {
    static ACC: RefCell<Vec<WinInfo>> = RefCell::new(Vec::new());
}

unsafe extern "system" fn enum_proc(hwnd: HWND, _lp: LPARAM) -> BOOL {
    let mut root = hwnd;
    let mut pid: u32 = 0;
    let mut title = String::new();
    let mut r = windows::Win32::Foundation::RECT::default();
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        // 取根窗口（避免子窗口/工具窗口重复）
        root = GetAncestor(hwnd, GA_ROOT);
        if root != hwnd {
            return BOOL(1);
        }
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd, &mut buf);
        title = String::from_utf16_lossy(&buf[..n.max(0) as usize]).trim().to_string();
        let _ = GetWindowRect(hwnd, &mut r);
    }
    ACC.with(|acc| {
        acc.borrow_mut().push(WinInfo {
            hwnd: hwnd.0 as isize,
            pid,
            title,
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
        });
    });
    BOOL(1)
}

fn all_windows() -> Vec<WinInfo> {
    ACC.with(|acc| acc.borrow_mut().clear());
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(0));
    }
    ACC.with(|acc| (*acc.borrow()).clone())
}

fn find_window(pid: Option<u32>, title: Option<&str>) -> Option<WinInfo> {
    let wins = all_windows();
    let meaningful = |w: &WinInfo| {
        w.w > 100 && w.h > 100 && !w.title.is_empty()
            && !matches!(w.title.as_str(), "还原页面" | "Restore pages" | "新标签页" | "New Tab")
    };
    if let Some(p) = pid {
        // 按 pid 找：不滤尺寸（最小化窗口可能 <50px），用 meaningful（有标题）优先
        let cands: Vec<&WinInfo> = wins.iter().filter(|w| w.pid == p).collect();
        let good: Vec<&WinInfo> = cands.iter().copied().filter(|w| meaningful(w)).collect();
        let pool = if good.is_empty() { cands } else { good };
        return pool.into_iter().max_by_key(|w| w.w * w.h).map(|w| w.clone());
    }
    if let Some(t) = title {
        let tl = t.to_lowercase();
        return wins.iter().find(|w| meaningful(w) && w.title.to_lowercase().contains(&tl)).cloned();
    }
    None
}

fn compute(align: &str, wa: (i32, i32, i32, i32), scale: f64, gap: i32, off: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let (wx, wy, ww, wh) = wa;
    let (lo, to, ro, bo) = off;
    // 与 winsnap 滚轮一致：按「内容(visual)」计算，frame = visual + DWM 不可见边框
    let (mut vx, mut vy, mut vw, mut vh) = (wx + gap, wy + gap, ww - gap * 2, wh - gap * 2);
    match align {
        "left" => vw = (vw as f64 * 0.5) as i32,
        "right" => { vw = (vw as f64 * 0.5) as i32; vx = wx + gap + ((ww - gap * 2) - vw); }
        "left-edge" => vw = (vw as f64 * 0.25) as i32,
        "right-edge" => { vw = (vw as f64 * 0.25) as i32; vx = wx + gap + ((ww - gap * 2) - vw); }
        "center" => {
            let r = if scale > 0.0 { scale } else { 0.7 };
            vw = (vw as f64 * r) as i32;
            vh = (vh as f64 * r) as i32;
            vx = wx + gap + ((ww - gap * 2) - vw) / 2;
            vy = wy + gap + ((wh - gap * 2) - vh) / 2;
        }
        "proportional" => {
            // 内容 = 工作区 × scale（无 gap，与 winsnap 滚轮 100%×0.9 完全一致），frame 加 DWM 边框
            let s = if scale > 0.0 { scale } else { 0.8 };
            vw = (ww as f64 * s) as i32;
            vh = (wh as f64 * s) as i32;
            vx = wx + (ww - vw) / 2;
            vy = wy + (wh - vh) / 2;
        }
        _ => {}
    }
    // 映射回窗口 frame（visual + DWM 不可见边框）
    (vx - lo, vy - to, vw + lo + ro, vh + to + bo)
}

pub fn run(args: &[String]) -> i32 {
    let sub = args.iter().position(|a| a == "--cmd").and_then(|i| args.get(i + 1)).map(|s| s.as_str()).unwrap_or("");
    let pid = args.iter().position(|a| a == "--pid").and_then(|i| args.get(i + 1)).and_then(|s| s.parse::<u32>().ok());
    let title = args.iter().position(|a| a == "--title").and_then(|i| args.get(i + 1)).map(|s| s.as_str());
    let align = args.iter().position(|a| a == "--align").and_then(|i| args.get(i + 1)).map(|s| s.as_str()).unwrap_or("proportional");
    let scale = args.iter().position(|a| a == "--scale").and_then(|i| args.get(i + 1)).and_then(|s| s.parse::<f64>().ok()).unwrap_or(-1.0);
    let gap = args.iter().position(|a| a == "--gap").and_then(|i| args.get(i + 1)).and_then(|s| s.parse::<i32>().ok()).unwrap_or(8);
    let top = args.iter().position(|a| a == "--top").and_then(|i| args.get(i + 1)).and_then(|s| s.parse::<usize>().ok()).unwrap_or(100);

    match sub {
        "screen" => {
            let (sw, sh) = cfg::screen_size();
            let wa = cfg::get_monitor_work_area(HWND(std::ptr::null_mut()));
            println!("{{\"screen\":{{\"w\":{sw},\"h\":{sh}}},\"workArea\":{{\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}}}", wa.0, wa.1, wa.2, wa.3);
            0
        }
        "list" => {
            let wins: Vec<WinInfo> = all_windows()
                .into_iter()
                .filter(|w| pid.map_or(true, |p| w.pid == p))
                .filter(|w| title.map_or(true, |t| w.title.to_lowercase().contains(&t.to_lowercase())))
                .filter(|w| w.w > 50 && w.h > 50)
                .collect();
            let mut out = String::from("{\"count\":");
            out.push_str(&wins.len().to_string());
            out.push_str(",\"windows\":[");
            for (i, w) in wins.iter().take(top).enumerate() {
                if i > 0 { out.push(','); }
                out.push_str(&format!("{{\"hwnd\":\"{:X}\",\"pid\":{},\"title\":{},\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
                    w.hwnd, w.pid, serde_json_title(&w.title), w.x, w.y, w.w, w.h));
            }
            out.push_str("]}");
            println!("{out}");
            0
        }
        "snap" => {
            let w = match find_window(pid, title) {
                Some(w) => w,
                None => { eprintln!("window not found (pid={pid:?} title={title:?})"); return 2; }
            };
            let hwnd = HWND(w.hwnd as *mut core::ffi::c_void);
            // 最小化的窗口先还原，再摆位
            unsafe {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
            }
            let wa = cfg::get_monitor_work_area(hwnd);
            let off = cfg::get_dwm_frame_offsets(hwnd);
            let (x, y, cw, ch) = compute(align, wa, scale, gap, off);
            let ok = cfg::move_window_to(hwnd, x, y) && cfg::set_window_pos(hwnd, x, y, cw, ch);
            println!("{{\"hwnd\":\"{:X}\",\"pid\":{},\"title\":{},\"align\":\"{}\",\"rect\":{{\"x\":{x},\"y\":{y},\"w\":{cw},\"h\":{ch}}},\"ok\":{ok}}}",
                w.hwnd, w.pid, serde_json_title(&w.title), align);
            0
        }
        "restore" => {
            let w = match find_window(pid, title) {
                Some(w) => w,
                None => { eprintln!("window not found (pid={pid:?} title={title:?})"); return 2; }
            };
            let hwnd = HWND(w.hwnd as *mut core::ffi::c_void);
            unsafe { let _ = ShowWindow(hwnd, SW_RESTORE); }
            println!("{{\"hwnd\":\"{:X}\",\"pid\":{},\"restored\":true}}", w.hwnd, w.pid);
            0
        }
        _ => {
            eprintln!("usage: winsnap --cmd snap|list|screen ...");
            1
        }
    }
}

/// 标题转 JSON 字符串（转义引号/反斜杠/控制字符）
fn serde_json_title(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
