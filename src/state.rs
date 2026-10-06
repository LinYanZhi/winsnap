//! 全局状态（原子变量，跨线程安全）与修饰键/鼠标按键查询

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering};

// windows crate 未导出 ReleaseCapture，此处统一声明
#[link(name = "user32")]
unsafe extern "system" {
    pub fn ReleaseCapture() -> i32;
}

// ── 全局状态 ──

pub static HOOK_PAUSED: AtomicBool = AtomicBool::new(false);

// 低层鼠标钩子记录的按键状态（由托盘线程的回调写入，主线程轮询读取）
pub static MOUSE_LEFT_HOOK: AtomicBool = AtomicBool::new(false);
pub static MOUSE_RIGHT_HOOK: AtomicBool = AtomicBool::new(false);
pub static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
pub static HOOK_HANDLE: AtomicIsize = AtomicIsize::new(0);

// 钩子回调检测到 Alt+鼠标点击 → 置 1 请求拖拽移动，置 2 请求调整大小
pub static HOOK_DRAG_REQUEST: AtomicU32 = AtomicU32::new(0);

// 左键拖拽结束时去抖计数（钩子状态抖动 <24ms 不结束拖拽）
pub static DRAG_LEFT_UP_COUNT: AtomicU32 = AtomicU32::new(0);

// ── 鼠标拖拽/调整大小状态（原子变量，跨线程安全） ──

pub static DRAG_MODE: AtomicU32 = AtomicU32::new(0); // 0=无, 1=移动, 2=调整大小
pub static DRAG_START_MX: AtomicI32 = AtomicI32::new(0);
pub static DRAG_START_MY: AtomicI32 = AtomicI32::new(0);
pub static DRAG_WIN_L: AtomicI32 = AtomicI32::new(0);
pub static DRAG_WIN_T: AtomicI32 = AtomicI32::new(0);
pub static DRAG_WIN_R: AtomicI32 = AtomicI32::new(0);
pub static DRAG_WIN_B: AtomicI32 = AtomicI32::new(0);
pub static DRAG_HT: AtomicU32 = AtomicU32::new(0);  // 调整大小方向（HT* 10-17）
pub static DRAG_HWND: AtomicIsize = AtomicIsize::new(0);

// ── 修饰键和鼠标按键查询 ──

pub fn is_alt_held() -> bool {
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        GetAsyncKeyState(0x12) < 0
    }
}

pub fn is_ctrl_held() -> bool {
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        GetAsyncKeyState(0x11) < 0
    }
}

pub fn is_left_down() -> bool {
    // 优先使用低层鼠标钩子状态，回退到 GetAsyncKeyState
    if HOOK_INSTALLED.load(Ordering::Relaxed) {
        MOUSE_LEFT_HOOK.load(Ordering::Relaxed)
    } else {
        unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
            GetAsyncKeyState(0x01) < 0
        }
    }
}

pub fn is_right_down() -> bool {
    if HOOK_INSTALLED.load(Ordering::Relaxed) {
        MOUSE_RIGHT_HOOK.load(Ordering::Relaxed)
    } else {
        unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
            GetAsyncKeyState(0x02) < 0
        }
    }
}

// ── 置顶窗口追踪（供托盘二级菜单展示 + 失效清理） ──
//
// 不持久化（winsnap 关闭即清空）；存 isize 而非 HWND（HWND 非 Send/Sync，
// 静态集合存裸指针值，用时重建 HWND）。Vec 而非 HashSet 是因为 HashSet::new
// 不是 const fn（rustc 1.97），窗口数 < 50 时 Vec 的 O(n) 查找完全够用。

use std::sync::Mutex;
use windows::Win32::Foundation::HWND;

/// 当前被置顶的窗口列表（用户视角的"置顶列表"）
pub static TOPMOST_HWNDS: Mutex<Vec<isize>> = Mutex::new(Vec::new());

/// 标记 hwnd 为置顶（加入追踪列表，已存在则跳过）
pub fn track_topmost(hwnd: HWND) {
    let h = hwnd.0 as isize;
    let mut v = TOPMOST_HWNDS.lock().unwrap();
    if !v.contains(&h) {
        v.push(h);
    }
}

/// 取消标记（从追踪列表移除）
pub fn untrack_topmost(hwnd: HWND) {
    let h = hwnd.0 as isize;
    TOPMOST_HWNDS.lock().unwrap().retain(|&x| x != h);
}

/// 当前置顶窗口数
pub fn topmost_count() -> usize {
    TOPMOST_HWNDS.lock().unwrap().len()
}

/// 看门狗调用：清理已关闭的窗口句柄（IsWindow 返回 false）
/// 返回清理的数量（用于日志）
pub fn cleanup_stale_topmost() -> usize {
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;
    let mut v = TOPMOST_HWNDS.lock().unwrap();
    let before = v.len();
    v.retain(|&h| unsafe { IsWindow(HWND(h as *mut _)).as_bool() });
    before - v.len()
}
