//! 开机自启：启动文件夹快捷方式 + 注册表 Run 键（双保险）

use std::path::PathBuf;

use winreg::enums::*;

fn startup_folder() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup")
}

fn shortcut_path() -> PathBuf {
    startup_folder().join("winsnap.lnk")
}

fn reg_run_key() -> std::io::Result<winreg::RegKey> {
    let key = winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Run", KEY_READ | KEY_WRITE);
    match key {
        Ok(k) => Ok(k),
        Err(_) => {
            let (key, _) = winreg::RegKey::predef(HKEY_CURRENT_USER)
                .create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")?;
            Ok(key)
        }
    }
}

/// 检测是否已开启开机自启
pub fn is_auto_start_enabled() -> bool {
    // 检测方式 1：启动文件夹是否有 winsnap.lnk
    if shortcut_path().exists() {
        return true;
    }
    // 检测方式 2：注册表 Run 键是否有 winsnap
    if let Ok(key) = reg_run_key() {
        if let Ok(val) = key.get_value::<String, _>("winsnap") {
            if !val.is_empty() {
                return true;
            }
        }
    }
    false
}

/// 创建快捷方式（使用 WScript COM 对象）
fn create_shortcut(target: &PathBuf) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    // 通过 PowerShell 创建快捷方式
    let script = format!(
        "$ws = New-Object -ComObject WScript.Shell; \
         $lnk = $ws.CreateShortcut('{}'); \
         $lnk.TargetPath = '{}'; \
         $lnk.WorkingDirectory = '{}'; \
         $lnk.Save()",
        shortcut_path().display().to_string().replace('\'', "''"),
        target.display().to_string().replace('\'', "''"),
        target.parent().unwrap().display().to_string().replace('\'', "''"),
    );
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

/// 开启开机自启
fn enable_auto_start() {
    let exe_path = std::env::current_exe().unwrap_or_default();

    // 方式 1：启动文件夹快捷方式
    create_shortcut(&exe_path);

    // 方式 2：注册表 Run 键
    if let Ok(key) = reg_run_key() {
        let _ = key.set_value("winsnap", &exe_path.to_string_lossy().to_string());
    }
}

/// 关闭开机自启
fn disable_auto_start() {
    // 方式 1：删除快捷方式
    let lnk = shortcut_path();
    if lnk.exists() {
        let _ = std::fs::remove_file(&lnk);
    }

    // 方式 2：删除注册表项
    if let Ok(key) = reg_run_key() {
        let _ = key.delete_value("winsnap");
    }
}

/// 切换开机自启状态，返回新状态
pub fn toggle_auto_start() -> bool {
    if is_auto_start_enabled() {
        disable_auto_start();
        false
    } else {
        enable_auto_start();
        true
    }
}
