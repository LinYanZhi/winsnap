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
