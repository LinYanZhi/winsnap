//! 窗口吸附（拖拽贴其它窗口边缘）— 矩形区间裁剪实现被遮挡边缘的可见性判断

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::Mutex;

use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::System::Console::GetConsoleWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetDesktopWindow,
    GetWindowLongPtrW, GWL_STYLE, IsIconic, IsWindowVisible, WS_CAPTION, GA_ROOT,
};

use crate::log::{c, log_iaction, now, CLR_NUDGE};
use crate::state::DRAG_HWND;
use crate::config as cfg;

// ── 贴边吸附常量 ──

pub const SNAP_THRESHOLD: i32 = 7;    // 贴边触发距离（像素）
pub const SNAP_ESCAPE: i32 = 30;      // 贴边后拖离此距离才脱开

// ── 吸附开关（托盘右键菜单控制） ──

pub static SNAP_SCREEN_ENABLED: AtomicBool = AtomicBool::new(true);  // 屏幕吸附：拖拽贴屏幕边缘，默认开启
pub static SNAP_WINDOW_ENABLED: AtomicBool = AtomicBool::new(false); // 窗口吸附：拖拽贴其它窗口边缘，默认关闭

// ── 拖拽期间贴边吸附状态 ──

pub static SNAP_H: AtomicI32 = AtomicI32::new(0);  // 0=自由, 1=左吸附, 2=右吸附
pub static SNAP_V: AtomicI32 = AtomicI32::new(0);  // 0=自由, 1=上吸附, 2=下吸附

// ── 拖拽窗口可视矩形（拖拽开始时基于 DWM 可视区域记录，移动不改变尺寸/偏移） ──

pub static DRAG_VIS_OFF_L: AtomicI32 = AtomicI32::new(0); // 可视左 - 帧左
pub static DRAG_VIS_OFF_T: AtomicI32 = AtomicI32::new(0); // 可视上 - 帧上
pub static DRAG_VIS_W: AtomicI32 = AtomicI32::new(0);     // 可视宽
pub static DRAG_VIS_H: AtomicI32 = AtomicI32::new(0);     // 可视高

/// 窗口吸附候选窗口：拖拽开始时枚举一次，拖拽过程中仅刷新帧矩形（GetWindowRect 开销小）
pub struct SnapCandidate {
    pub hwnd: isize, // 原始句柄值（HWND 非 Send/Sync，静态缓存只存 isize，用时重建）
    pub z: usize,    // 在 SNAP_ALL 中的索引（Z 序：0 最顶层），可见性裁剪时据此遍历更顶层窗口
    lo: i32, to: i32, ro: i32, bo: i32, // 帧矩形与可视矩形（DWM 可视区域）的四边差值，收集时计算一次
    // 四条边是否可见（未被更高 Z 序窗口的面积覆盖）：由每帧的区间裁剪结果刷新。
    // 被遮挡的边不参与吸附（如大窗口盖住小窗口的某条边，该边就不会产生吸附锚点）
    pub vis_l: bool, pub vis_t: bool, pub vis_r: bool, pub vis_b: bool,
}
/// 当前拖拽的窗口吸附候选列表（拖拽开始惰性收集，结束清空）
pub static SNAP_CANDIDATES: Mutex<Vec<SnapCandidate>> = Mutex::new(Vec::new());

/// 拖拽开始时枚举到的所有可见窗口（遮挡物 + 候选）快照，按 Z 序存储（0 最顶层）。
/// 拖拽过程中 Z 序与可视偏移不变，仅每帧刷新帧矩形（GetWindowRect 开销小），
/// 即可用矩形区间裁剪精确求出每条候选边的可见部分，替代"点采样"的近似判断。
struct SnapWin {
    hwnd: isize,
    lo: i32, to: i32, ro: i32, bo: i32, // 可视矩形 - 帧矩形 的四边差值
    desktop: bool, // 桌面层（根祖先为 Progman/WorkerW）：其面积不遮挡任何窗口
}
/// 当前拖拽的窗口快照列表（拖拽开始惰性收集，结束清空）
static SNAP_ALL: Mutex<Vec<SnapWin>> = Mutex::new(Vec::new());
/// 拖拽诊断日志节流（1s 一次），避免刷屏
pub static LAST_SNAP_DIAG: AtomicU64 = AtomicU64::new(0);

/// 拖拽结束贴边修正：Alt+左键拖拽释放时，若窗口可视边缘贴近屏幕边缘则吸附
pub fn snap_correction(hwnd: HWND) {
    // 屏幕吸附关闭时跳过结束贴边修正（拖拽中也不贴边）
    if !SNAP_SCREEN_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let (l, t, r, b) = cfg::get_window_rect(hwnd);
    // 基于可视矩形（DWM 可视区域）检测与屏幕边缘的距离
    let (vl, vt, vr, vb) = cfg::get_visual_rect(hwnd);
    let vw = vr - vl;
    let vh = vb - vt;
    let lo = vl - l; // 可视左 - 帧左
    let to = vt - t; // 可视上 - 帧上
    let (sw, sh) = cfg::screen_size();

    let dist_left   = vl.abs();
    let dist_right  = (sw - vr).abs();
    let dist_top    = vt.abs();
    let dist_bottom = (sh - vb).abs();

    println!("[{}] 拖拽结束贴边检测: 窗口可视=({vl},{vt},{vw}x{vh}) 距边缘=(左{dist_left},右{dist_right},上{dist_top},下{dist_bottom})",
        now());

    if dist_left <= SNAP_THRESHOLD {
        let old = (l, t, r, b);
        cfg::move_window_to(hwnd, -lo, t);
        let new = cfg::get_window_rect(hwnd);
        if old != new {
            log_iaction("贴边-左", CLR_NUDGE, hwnd, old, new);
        }
    } else if dist_right <= SNAP_THRESHOLD {
        let old = (l, t, r, b);
        cfg::move_window_to(hwnd, sw - vw - lo, t);
        let new = cfg::get_window_rect(hwnd);
        if old != new {
            log_iaction("贴边-右", CLR_NUDGE, hwnd, old, new);
        }
    } else if dist_top <= SNAP_THRESHOLD {
        let old = (l, t, r, b);
        cfg::move_window_to(hwnd, l, -to);
        let new = cfg::get_window_rect(hwnd);
        if old != new {
            log_iaction("贴边-上", CLR_NUDGE, hwnd, old, new);
        }
    } else if dist_bottom <= SNAP_THRESHOLD {
        let old = (l, t, r, b);
        cfg::move_window_to(hwnd, l, sh - vh - to);
        let new = cfg::get_window_rect(hwnd);
        if old != new {
            log_iaction("贴边-下", CLR_NUDGE, hwnd, old, new);
        }
    }
}

/// 判断 hwnd 是否为桌面层窗口（根祖先类名为 Progman/WorkerW）。
/// 桌面壁纸/图标层（含其子孙，如 SysListView32 "FolderView"）不遮挡任何窗口：
/// 窗口边缘对着屏幕空白处时不应被判为被遮挡（否则右贴右/下贴下失效）。
fn is_desktop_layer(hwnd: HWND) -> bool {
    unsafe {
        let mut cls = [0u16; 32];
        let n = GetClassNameW(GetAncestor(hwnd, GA_ROOT), &mut cls);
        if n > 0 {
            let name = String::from_utf16_lossy(&cls[..n as usize]);
            return name == "Progman" || name == "WorkerW";
        }
        false
    }
}

/// 枚举所有可见窗口（遮挡物 + 候选）存入 SNAP_ALL，返回候选列表。
/// 候选：可见、带标题栏、非被拖拽窗口自身的顶层窗口。
/// 遮挡物：所有可见非最小化窗口（含无标题栏的工具窗/托盘窗等），
/// 其可视矩形面积用于裁剪候选边缘——Z 序更高（更顶层）的窗口盖住的
/// 边缘部分视为不可见。桌面层（Progman/WorkerW 及其子孙）不参与遮挡。
fn collect_snap_candidates() -> Vec<SnapCandidate> {
    unsafe {
        extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
            unsafe {
                let out = &mut *(lparam.0 as *mut (Vec<SnapCandidate>, Vec<SnapWin>));
                // 拖拽窗口自身进 SNAP_ALL（移动中会盖住其它窗口，其面积须参与遮挡裁剪），
                // 但拖拽窗口不作为候选吸附对象
                let is_drag = hwnd.0 as isize == DRAG_HWND.load(Ordering::Relaxed);
                // 跳过不可见窗口和桌面
                if !IsWindowVisible(hwnd).as_bool() || hwnd == GetDesktopWindow() {
                    return BOOL(1);
                }
                // 跳过最小化窗口：Win+D 显示桌面后最小化窗口仍返回"还原位置"矩形，
                // 会以看不见的位置参与吸附/遮挡，形成幽灵吸附点
                if IsIconic(hwnd).as_bool() {
                    return BOOL(1);
                }
                // 跳过自身的控制台窗口
                if hwnd == GetConsoleWindow() {
                    return BOOL(1);
                }
                // 跳过被系统判定为不可见的窗口（DWM cloak：UWP 宿主残留、Win+D 隐藏等）
                if cfg::is_system_hidden(hwnd) {
                    return BOOL(1);
                }
                // 记录帧矩形与可视矩形（DWM 可视区域）的差值，拖拽中每帧按当前帧矩形换算可视矩形
                let (l, t, r, b) = cfg::get_window_rect(hwnd);
                if r - l <= 0 || b - t <= 0 {
                    return BOOL(1);
                }
                let (vl, vt, vr, vb) = cfg::get_visual_rect(hwnd);
                if vr - vl <= 0 || vb - vt <= 0 {
                    return BOOL(1);
                }
                let desktop = is_desktop_layer(hwnd);
                let z = out.1.len();
                out.1.push(SnapWin {
                    hwnd: hwnd.0 as isize,
                    lo: vl - l, to: vt - t,
                    ro: r - vr, bo: b - vb,
                    desktop,
                });
                // 候选需带标题栏（拖拽窗口自身除外）
                let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
                if !is_drag && style & (WS_CAPTION.0 as u32) != 0 {
                    out.0.push(SnapCandidate {
                        hwnd: hwnd.0 as isize,
                        z,
                        lo: vl - l, to: vt - t,
                        ro: r - vr, bo: b - vb,
                        vis_l: true, vis_t: true, vis_r: true, vis_b: true,
                    });
                }
                BOOL(1)
            }
        }
        let mut out: (Vec<SnapCandidate>, Vec<SnapWin>) = (Vec::new(), Vec::new());
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut out as *mut _ as isize));
        *SNAP_ALL.lock().unwrap() = out.1;
        out.0
    }
}

/// 快照窗口当前可视矩形（帧矩形每帧刷新 + 收集时记录的差值）
fn snapwin_visual(w: &SnapWin) -> (i32, i32, i32, i32) {
    let (fl, ft, fr, fb) = cfg::get_window_rect(HWND(w.hwnd as *mut std::ffi::c_void));
    (fl + w.lo, ft + w.to, fr - w.ro, fb - w.bo)
}

/// 从区间列表 [a,b) 中扣除 [lo,hi)，返回剩余区间（轴对齐矩形的一维投影裁剪）
fn subtract_range(ranges: Vec<(i32, i32)>, lo: i32, hi: i32) -> Vec<(i32, i32)> {
    let mut out = Vec::with_capacity(ranges.len() + 1);
    for (s, e) in ranges {
        if hi <= s || lo >= e {
            out.push((s, e));
        } else {
            if s < lo { out.push((s, lo)); }
            if e > hi { out.push((hi, e)); }
        }
    }
    out
}

/// 区间总可见长度是否 ≥ 边长 total 的 1/3（露出的部分够多才参与吸附）
fn edge_visible(ranges: &[(i32, i32)], total: i32) -> bool {
    let vis: i32 = ranges.iter().map(|(s, e)| (e - s).max(0)).sum();
    vis * 3 >= total.max(1)
}

/// 每帧重算所有候选的四边可见性：对每条候选边，用所有更高 Z 序窗口的
/// 可视矩形做一维区间裁剪，被面积覆盖的部分视为不可见，剩余可见区间
/// 总长 ≥ 边长 1/3 则该边可见（"露出来的做贴边"）。
/// 桌面层不参与遮挡；拖拽窗口自身盖住的边同样视为被遮挡（视觉上不可见）。
pub fn refresh_candidate_visibility() {
    let mut cands = SNAP_CANDIDATES.lock().unwrap();
    let all = SNAP_ALL.lock().unwrap();
    for cand in cands.iter_mut() {
        let (cl, ct, cr, cb) = candidate_visual(cand);
        let h = cb - ct;
        let w = cr - cl;
        // 四条边各自独立的投影区间：左/右边缘共享 y 区间、上/下边缘共享 x 区间，
        // 但左右、上下之间必须独立裁剪——遮挡物可能只盖住一边（如只盖左不盖右）
        let mut ys_l = vec![(ct, cb)];
        let mut ys_r = vec![(ct, cb)];
        let mut xs_t = vec![(cl, cr)];
        let mut xs_b = vec![(cl, cr)];
        for w2 in all.iter().take(cand.z) {
            if w2.hwnd == cand.hwnd || w2.desktop {
                continue;
            }
            let (wl, wt, wr, wb) = snapwin_visual(w2);
            // 遮挡物面积覆盖候选边缘的 x/y 位置时，扣除其在边上的投影区间
            if wl <= cl && cl < wr { ys_l = subtract_range(ys_l, wt, wb); }
            if wl <= cr && cr < wr { ys_r = subtract_range(ys_r, wt, wb); }
            if wt <= ct && ct < wb { xs_t = subtract_range(xs_t, wl, wr); }
            if wt <= cb && cb < wb { xs_b = subtract_range(xs_b, wl, wr); }
        }
        cand.vis_l = edge_visible(&ys_l, h);
        cand.vis_r = edge_visible(&ys_r, h);
        cand.vis_t = edge_visible(&xs_t, w);
        cand.vis_b = edge_visible(&xs_b, w);
    }
}

/// 候选窗口当前可视矩形（帧矩形每帧刷新 + 收集时记录的差值）
fn candidate_visual(c: &SnapCandidate) -> (i32, i32, i32, i32) {
    let (cl, ct, cr, cb) = cfg::get_window_rect(HWND(c.hwnd as *mut std::ffi::c_void));
    (cl + c.lo, ct + c.to, cr - c.ro, cb - c.bo)
}

/// 水平吸附锚点：返回最近锚点的 (目标可视左 vx, 方向 1=左 2=右, 距离)
///
/// 基于可视矩形（DWM 可视区域）。屏幕左右边缘始终为锚点；窗口锚点包含
/// 相邻对齐（左贴右、右贴左）与同向对齐（左贴左、右贴右），仅当两窗口
/// 垂直范围重叠（含边缘相接）时参与，避免吸附到上下方向毫不相干的窗口。
pub fn snap_anchor_x(
    dx: i32, vx: i32, vw: i32, vt: i32, vh: i32,
    sw: i32, screen_on: bool, candidates: &[SnapCandidate],
) -> Option<(i32, i32, i32)> {
    let vis_r = vx + vw;  // 拖拽窗口可视右边缘
    let vis_b = vt + vh;  // 拖拽窗口可视下边缘
    // 方向约束：已贴近（≤阈值）或零移动时无条件参与；否则仅当该边缘与锚点
    // 正在靠近时参与，避免拖过不相干窗口时被其边缘"磕绊"。
    // 注意：右贴右/下贴下时用户拖拽方向与边缘靠近方向相反（如 A 在 B 右侧
    // 向右拖，A 右边缘在 B 右边缘外侧），若一律要求同向会失效，故贴近即可吸。
    let near = |edge: i32, target: i32| {
        (target - edge).abs() <= SNAP_THRESHOLD || dx == 0 || (target - edge) * dx > 0
    };
    let mut best_dist = i32::MAX;
    let mut best: Option<(i32, i32)> = None; // (目标可视左, 方向)
    if screen_on {
        let dl = vx.abs(); // 可视左贴屏幕左（0）
        if near(vx, 0) && dl < best_dist { best_dist = dl; best = Some((0, 1)); }
        let dr = (sw - vis_r).abs(); // 可视右贴屏幕右（sw）
        if near(vis_r, sw) && dr < best_dist { best_dist = dr; best = Some((sw - vw, 2)); }
    }
    for c in candidates {
        let (cl, ct, cr, cb) = candidate_visual(c);
        // 垂直范围重叠（或相接）才考虑水平吸附；仅候选可见的边缘产生锚点
        if vt <= cb && ct <= vis_b {
            // 相邻对齐：拖拽左 贴 候选右（候选右边缘可见）
            if c.vis_r {
                let d1 = (vx - cr).abs();
                if near(vx, cr) && d1 < best_dist { best_dist = d1; best = Some((cr, 1)); }
            }
            // 相邻对齐：拖拽右 贴 候选左（候选左边缘可见）
            if c.vis_l {
                let d2 = (vis_r - cl).abs();
                if near(vis_r, cl) && d2 < best_dist { best_dist = d2; best = Some((cl - vw, 2)); }
            }
            // 同向对齐：拖拽左 贴 候选左（候选左边缘可见）
            if c.vis_l {
                let d3 = (vx - cl).abs();
                if near(vx, cl) && d3 < best_dist { best_dist = d3; best = Some((cl, 1)); }
            }
            // 同向对齐：拖拽右 贴 候选右（候选右边缘可见）
            if c.vis_r {
                let d4 = (vis_r - cr).abs();
                if near(vis_r, cr) && d4 < best_dist { best_dist = d4; best = Some((cr - vw, 2)); }
            }
        }
    }
    best.map(|(target, dir)| (target, dir, best_dist))
}

/// 垂直吸附锚点：返回最近锚点的 (目标可视上 vy, 方向 1=上 2=下, 距离)
///
/// 窗口锚点仅当两窗口水平范围重叠（含边缘相接）时参与。
pub fn snap_anchor_y(
    dy: i32, vy: i32, vh: i32, vx: i32, vw: i32,
    sht: i32, screen_on: bool, candidates: &[SnapCandidate],
) -> Option<(i32, i32, i32)> {
    let vis_b = vy + vh;  // 拖拽窗口可视下边缘
    let vis_r = vx + vw;  // 拖拽窗口可视右边缘
    // 方向约束：已贴近（≤阈值）或零移动时无条件参与；否则仅当正在靠近时参与
    let near = |edge: i32, target: i32| {
        (target - edge).abs() <= SNAP_THRESHOLD || dy == 0 || (target - edge) * dy > 0
    };
    let mut best_dist = i32::MAX;
    let mut best: Option<(i32, i32)> = None; // (目标可视上, 方向)
    if screen_on {
        let dt = vy.abs(); // 可视上贴屏幕上（0）
        if near(vy, 0) && dt < best_dist { best_dist = dt; best = Some((0, 1)); }
        let db = (sht - vis_b).abs(); // 可视下贴屏幕下（sht）
        if near(vis_b, sht) && db < best_dist { best_dist = db; best = Some((sht - vh, 2)); }
    }
    for c in candidates {
        let (cl, ct, cr, cb) = candidate_visual(c);
        // 水平范围重叠（或相接）才考虑垂直吸附；仅候选可见的边缘产生锚点
        if vx <= cr && cl <= vis_r {
            // 相邻对齐：拖拽上 贴 候选下（候选下边缘可见）
            if c.vis_b {
                let d1 = (vy - cb).abs();
                if near(vy, cb) && d1 < best_dist { best_dist = d1; best = Some((cb, 1)); }
            }
            // 相邻对齐：拖拽下 贴 候选上（候选上边缘可见）
            if c.vis_t {
                let d2 = (vis_b - ct).abs();
                if near(vis_b, ct) && d2 < best_dist { best_dist = d2; best = Some((ct - vh, 2)); }
            }
            // 同向对齐：拖拽上 贴 候选上（候选上边缘可见）
            if c.vis_t {
                let d3 = (vy - ct).abs();
                if near(vy, ct) && d3 < best_dist { best_dist = d3; best = Some((ct, 1)); }
            }
            // 同向对齐：拖拽下 贴 候选下（候选下边缘可见）
            if c.vis_b {
                let d4 = (vis_b - cb).abs();
                if near(vis_b, cb) && d4 < best_dist { best_dist = d4; best = Some((cb - vh, 2)); }
            }
        }
    }
    best.map(|(target, dir)| (target, dir, best_dist))
}

/// 记录拖拽窗口的可视矩形（帧位置 + DWM 可视偏移/尺寸），拖拽期间保持
pub fn store_drag_visual(hwnd: HWND, l: i32, t: i32) {
    let (vl, vt, vr, vb) = cfg::get_visual_rect(hwnd);
    DRAG_VIS_OFF_L.store(vl - l, Ordering::Relaxed);
    DRAG_VIS_OFF_T.store(vt - t, Ordering::Relaxed);
    DRAG_VIS_W.store(vr - vl, Ordering::Relaxed);
    DRAG_VIS_H.store(vb - vt, Ordering::Relaxed);
}

/// 拖拽开始后惰性收集候选窗口（窗口吸附开启时调用）
pub fn ensure_snap_candidates() {
    let mut guard = SNAP_CANDIDATES.lock().unwrap();
    if guard.is_empty() {
        *guard = collect_snap_candidates();
        println!("[{}] {} 窗口吸附候选 {} 个: {}", now(), c("[SNAP]", CLR_NUDGE), guard.len(),
            guard.iter()
                .map(|c| cfg::get_process_name(HWND(c.hwnd as *mut std::ffi::c_void)))
                .collect::<Vec<_>>()
                .join(", "));
        for cand in guard.iter() {
            println!(
                "[{}] {}   边可见 左={} 上={} 右={} 下={} ({})",
                now(), c("[SNAP]", CLR_NUDGE),
                cand.vis_l, cand.vis_t, cand.vis_r, cand.vis_b,
                cfg::get_process_name(HWND(cand.hwnd as *mut std::ffi::c_void))
            );
        }
    }
}
