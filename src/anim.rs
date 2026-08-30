//! 窗口移动动画（后台线程平滑移动，不阻塞主循环鼠标轮询）

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::HWND;

use crate::state::DRAG_MODE;
use crate::config as cfg;

// 托盘右键菜单"开启动画"开关：开启后 Alt+数字定位 / Alt+方向键贴边 以动画平滑移动
pub static ANIMATE_ENABLED: AtomicBool = AtomicBool::new(false);
// 动画线程运行中标志（新动画请求会等上一个动画退出，避免同时移动同一窗口）
static ANIM_ACTIVE: AtomicBool = AtomicBool::new(false);
// 请求中止当前动画（新动画请求先中止上一个再启动）
static ANIM_ABORT: AtomicBool = AtomicBool::new(false);

/// 窗口移动入口：动画关闭时瞬移，开启时后台线程平滑移动（不阻塞主循环鼠标轮询）
pub fn animated_move(hwnd: HWND, tx: i32, ty: i32) {
    if !ANIMATE_ENABLED.load(Ordering::Relaxed) {
        cfg::move_window_to(hwnd, tx, ty);
        return;
    }
    // 中止上一个动画并等待其退出（最多等 200ms），避免两个动画线程同时移动同一窗口
    ANIM_ABORT.store(true, Ordering::SeqCst);
    for _ in 0..100 {
        if !ANIM_ACTIVE.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    ANIM_ABORT.store(false, Ordering::SeqCst);

    let hwnd_val = hwnd.0 as isize;
    thread::spawn(move || {
        // HWND 非 Send（内部为裸指针），跨线程传原始句柄值再重建
        let hwnd = HWND(hwnd_val as *mut std::ffi::c_void);
        ANIM_ACTIVE.store(true, Ordering::SeqCst);
        animate_window_to(hwnd, tx, ty);
        ANIM_ACTIVE.store(false, Ordering::SeqCst);
    });
}

/// 后台动画线程主体：从窗口当前位置平滑移动到目标位置（ease-out 三次缓动）
fn animate_window_to(hwnd: HWND, tx: i32, ty: i32) {
    let (x0, y0, _, _) = cfg::get_window_rect(hwnd);
    let dx = tx - x0;
    let dy = ty - y0;
    let dist = ((dx * dx + dy * dy) as f64).sqrt();
    if dist < 1.0 {
        cfg::move_window_to(hwnd, tx, ty);
        return;
    }
    // 时长随距离略增，限制在 200~400ms
    let duration_ms = (200.0 + dist * 0.1).min(400.0);
    const STEP_MS: f64 = 8.0;
    let steps = (duration_ms / STEP_MS).max(1.0) as u64;

    for i in 0..=steps {
        // 被新动画接管（ABORT）、用户开始拖拽或窗口销毁 → 提前结束
        if ANIM_ABORT.load(Ordering::SeqCst) || DRAG_MODE.load(Ordering::Relaxed) != 0 {
            return;
        }
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::IsWindow;
            if !IsWindow(hwnd).as_bool() {
                return;
            }
        }
        let t = i as f64 / steps as f64;
        let e = 1.0 - (1.0 - t).powi(3); // ease-out cubic：先快后慢
        let x = x0 + (dx as f64 * e).round() as i32;
        let y = y0 + (dy as f64 * e).round() as i32;
        // 窗口挂起导致 SetWindowPos 失败 → 停止动画
        if !cfg::move_window_to(hwnd, x, y) {
            return;
        }
        thread::sleep(Duration::from_millis(STEP_MS as u64));
    }
    // 收尾：精确到达目标位置
    cfg::move_window_to(hwnd, tx, ty);
}
