//! 单例控制：确保只有一个 winsnap 实例在运行

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};

use crate::tray::WM_TRAY_RESTORE;

/// 单例控制：首次运行 → 记录 PID 返回 true；已有实例 → 先判断其托盘是否健康：
/// 健康则请其重建托盘图标并等待确认（确认后本实例退出），
/// 不健康则终止旧进程接管；无法接管则本实例退出。
pub fn ensure_single_instance() -> bool {
    unsafe {
        use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::core::w;

        let handle: windows::Win32::Foundation::HANDLE =
            match CreateMutexW(None, false, w!("Local\\winsnap_single_instance")) {
                Ok(h) => h,
                Err(_) => return true, // 创建失败仍允许运行
            };

        if GetLastError() == ERROR_ALREADY_EXISTS {
            // 已有实例在运行。先找它的托盘窗口，判断它是否还健康。
            let old_hwnd = match FindWindowW(w!("winsnap_tray_window"), None) {
                Ok(h) => h,
                Err(_) => HWND::default(),
            };
            if !old_hwnd.0.is_null() {
                if let Some((tid, old_pid)) = window_thread_pid(old_hwnd) {
                    if !thread_is_alive(tid) {
                        // 窗口所属线程已死 → 旧实例托盘已失效，直接终止旧进程接管
                        if process_is_winsnap(old_pid) {
                            return take_over(handle, old_pid);
                        }
                    } else {
                        // 线程还活着 → 发消息请它重建托盘图标并等待确认。
                        // 消息泵卡死则超时无响应，此时终止旧进程接管。
                        if request_restore_and_wait(old_hwnd) {
                            let _: core::result::Result<(), _> = CloseHandle(handle);
                            return false; // 旧实例已确认重建图标，本实例退出
                        }
                        if process_is_winsnap(old_pid) {
                            return take_over(handle, old_pid);
                        }
                    }
                }
            }

            // 托盘窗口找不到或无法确认 → 依次尝试 pid 文件、进程快照定位旧实例
            if let Some(old_pid) = read_runtime_pid() {
                if process_is_winsnap(old_pid) {
                    return take_over(handle, old_pid);
                }
            }
            if let Some(old_pid) = find_winsnap_process() {
                if old_pid != std::process::id() {
                    return take_over(handle, old_pid);
                }
            }
            let _: core::result::Result<(), _> = CloseHandle(handle);
            eprintln!("winsnap: 已有实例在运行且无法接管，本实例退出");
            return false;
        }

        // 首次运行：互斥体句柄保持开放（不关闭），使互斥体随进程存续
        write_runtime_pid();
        true
    }
}

/// 终止旧实例并接管单例互斥体；成功返回 true
fn take_over(handle: windows::Win32::Foundation::HANDLE, old_pid: u32) -> bool {
    unsafe {
        use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::core::w;

        let _ = CloseHandle(handle);
        if !kill_process(old_pid) {
            eprintln!("winsnap: 无法终止旧实例 (PID {old_pid})");
            return false;
        }
        // 等待旧进程退出，直到互斥体可被独占创建（最多 5 秒）
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if let Ok(h) = CreateMutexW(None, false, w!("Local\\winsnap_single_instance")) {
                if GetLastError() != ERROR_ALREADY_EXISTS {
                    let _ = h; // 保持句柄开放，使互斥体存续
                    write_runtime_pid();
                    return true;
                }
                let _: core::result::Result<(), _> = CloseHandle(h);
            } else {
                write_runtime_pid();
                return true;
            }
        }
        eprintln!("winsnap: 旧实例 5 秒内未退出，接管失败");
        false
    }
}

fn window_thread_pid(hwnd: HWND) -> Option<(u32, u32)> {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        let mut pid: u32 = 0;
        let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if tid == 0 { None } else { Some((tid, pid)) }
    }
}

/// 判断线程是否存活（句柄无法打开或已退出视为死亡）
fn thread_is_alive(tid: u32) -> bool {
    unsafe {
        use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows::Win32::System::Threading::{GetExitCodeThread, OpenThread, THREAD_QUERY_INFORMATION};

        let handle = match OpenThread(THREAD_QUERY_INFORMATION, false, tid) {
            Ok(h) => h,
            Err(_) => return false, // 线程已不存在或无权限
        };
        let mut code: u32 = 0;
        let ok = GetExitCodeThread(handle, &mut code).is_ok();
        let _ = CloseHandle(handle);
        ok && code == STILL_ACTIVE.0 as u32
    }
}

/// 请求旧实例重建托盘图标并等待其确认（最多 2 秒）；返回 true 表示旧实例已响应
fn request_restore_and_wait(old_hwnd: HWND) -> bool {
    unsafe {
        use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
        use windows::core::w;

        let ev = match CreateEventW(None, false, false, w!("Local\\winsnap_restored")) {
            Ok(e) => e,
            Err(_) => return false, // 无法创建确认事件 → 视为无响应
        };
        let _ = ResetEvent(ev);
        let _ = PostMessageW(old_hwnd, WM_TRAY_RESTORE, WPARAM(0), LPARAM(0));
        let r = WaitForSingleObject(ev, 2000);
        let _ = CloseHandle(ev);
        r == WAIT_OBJECT_0
    }
}

/// 通过进程快照查找与当前 exe 同路径的 winsnap 进程（不依赖 pid 文件）
fn find_winsnap_process() -> Option<u32> {
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
        };

        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(s) => s,
            Err(_) => return None,
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                if process_is_winsnap(entry.th32ProcessID) {
                    found = Some(entry.th32ProcessID);
                    break;
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// 运行时 PID 文件路径（供后续实例定位并接管失效的旧实例）
fn runtime_pid_path() -> std::path::PathBuf {
    std::env::temp_dir().join("winsnap_runtime.pid")
}

/// 记录当前进程 PID
fn write_runtime_pid() {
    let _ = std::fs::write(runtime_pid_path(), std::process::id().to_string());
}

/// 读取旧实例 PID（文件不存在或损坏时返回 None）
fn read_runtime_pid() -> Option<u32> {
    std::fs::read_to_string(runtime_pid_path()).ok()?.trim().parse().ok()
}

/// 校验指定 PID 的进程是同一个 winsnap.exe（防止误杀其它进程）
fn process_is_winsnap(pid: u32) -> bool {
    unsafe {
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
            PROCESS_QUERY_INFORMATION, PROCESS_TERMINATE,
        };
        use windows::Win32::Foundation::CloseHandle;

        let handle = match OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_TERMINATE, false, pid) {
            Ok(h) => h,
            Err(_) => return false, // 进程已不存在或无权限
        };
        let mut buf = vec![0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        ).is_ok();
        let _ = CloseHandle(handle);
        if !ok {
            return false;
        }
        let other = String::from_utf16_lossy(&buf[..len as usize]).to_lowercase();
        let mine = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        !mine.is_empty() && other == mine
    }
}

/// 强制终止指定进程
fn kill_process(pid: u32) -> bool {
    unsafe {
        use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        use windows::Win32::Foundation::CloseHandle;

        match OpenProcess(PROCESS_TERMINATE, false, pid) {
            Ok(h) => {
                let ok = TerminateProcess(h, 0).is_ok();
                let _ = CloseHandle(h);
                ok
            }
            Err(_) => false,
        }
    }
}
