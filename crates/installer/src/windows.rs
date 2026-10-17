//! Windows integration, for the current user only (no administrator rights):
//! the user's `PATH`, the entry in Settings > Apps that runs the
//! uninstaller, and the icon of `.sayon` files.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

type Hkey = *mut c_void;

const HKEY_CURRENT_USER: Hkey = 0x8000_0001u32 as i32 as isize as Hkey;
const KEY_READ_WRITE: u32 = 0x2_0019 | 0x2_0006;
const REG_SZ: u32 = 1;
const REG_EXPAND_SZ: u32 = 2;
const REG_DWORD: u32 = 4;
const ERROR_SUCCESS: i32 = 0;
const ERROR_FILE_NOT_FOUND: i32 = 2;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegCreateKeyExW(
        key: Hkey,
        subkey: *const u16,
        reserved: u32,
        class: *const u16,
        options: u32,
        access: u32,
        security: *const c_void,
        result: *mut Hkey,
        disposition: *mut u32,
    ) -> i32;
    fn RegOpenKeyExW(
        key: Hkey,
        subkey: *const u16,
        options: u32,
        access: u32,
        result: *mut Hkey,
    ) -> i32;
    fn RegQueryValueExW(
        key: Hkey,
        name: *const u16,
        reserved: *mut u32,
        kind: *mut u32,
        data: *mut u8,
        length: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        key: Hkey,
        name: *const u16,
        reserved: u32,
        kind: u32,
        data: *const u8,
        length: u32,
    ) -> i32;
    fn RegDeleteValueW(key: Hkey, name: *const u16) -> i32;
    fn RegDeleteTreeW(key: Hkey, subkey: *const u16) -> i32;
    fn RegQueryInfoKeyW(
        key: Hkey,
        class: *mut u16,
        class_length: *mut u32,
        reserved: *mut u32,
        subkeys: *mut u32,
        max_subkey_length: *mut u32,
        max_class_length: *mut u32,
        values: *mut u32,
        max_value_name_length: *mut u32,
        max_value_length: *mut u32,
        security_descriptor_length: *mut u32,
        last_write_time: *mut c_void,
    ) -> i32;
    fn RegCloseKey(key: Hkey) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SendMessageTimeoutW(
        window: isize,
        message: u32,
        wparam: usize,
        lparam: isize,
        flags: u32,
        timeout: u32,
        result: *mut usize,
    ) -> isize;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn SHChangeNotify(event: i32, flags: u32, first: *const c_void, second: *const c_void);
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// An open registry key under `HKEY_CURRENT_USER`.
struct Key(Hkey);

impl Key {
    fn open(path: &str) -> Result<Option<Key>, String> {
        let mut key: Hkey = std::ptr::null_mut();
        let path_w = wide(path);
        // SAFETY: the path is a valid null-terminated string and `key`
        // receives the opened handle.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                path_w.as_ptr(),
                0,
                KEY_READ_WRITE,
                &mut key,
            )
        };
        match status {
            ERROR_SUCCESS => Ok(Some(Key(key))),
            ERROR_FILE_NOT_FOUND => Ok(None),
            code => Err(format!(
                "cannot open the registry key {path} (error {code})"
            )),
        }
    }

    fn create(path: &str) -> Result<Key, String> {
        let mut key: Hkey = std::ptr::null_mut();
        let path_w = wide(path);
        // SAFETY: as above; the optional arguments are null.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path_w.as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_READ_WRITE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            )
        };
        if status == ERROR_SUCCESS {
            Ok(Key(key))
        } else {
            Err(format!(
                "cannot create the registry key {path} (error {status})"
            ))
        }
    }

    /// A string value and its type (`REG_SZ` or `REG_EXPAND_SZ`), unexpanded.
    fn string(&self, name: &str) -> Result<Option<(String, u32)>, String> {
        let name_w = wide(name);
        let mut kind = 0u32;
        let mut length = 0u32;
        // SAFETY: a null data pointer asks only for the type and size.
        let status = unsafe {
            RegQueryValueExW(
                self.0,
                name_w.as_ptr(),
                std::ptr::null_mut(),
                &mut kind,
                std::ptr::null_mut(),
                &mut length,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS || (kind != REG_SZ && kind != REG_EXPAND_SZ) {
            return Err(format!("cannot read the registry value {name}"));
        }
        let mut buffer = vec![0u16; (length as usize).div_ceil(2) + 1];
        let mut size = (buffer.len() * 2) as u32;
        // SAFETY: the buffer holds `size` bytes.
        let status = unsafe {
            RegQueryValueExW(
                self.0,
                name_w.as_ptr(),
                std::ptr::null_mut(),
                &mut kind,
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!("cannot read the registry value {name}"));
        }
        let units = (size as usize / 2).min(buffer.len());
        let end = buffer[..units]
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(units);
        Ok(Some((String::from_utf16_lossy(&buffer[..end]), kind)))
    }

    fn set_string(&self, name: &str, value: &str, kind: u32) -> Result<(), String> {
        let name_w = wide(name);
        let value_w = wide(value);
        // SAFETY: the data is the null-terminated UTF-16 value, with its
        // length in bytes.
        let status = unsafe {
            RegSetValueExW(
                self.0,
                name_w.as_ptr(),
                0,
                kind,
                value_w.as_ptr().cast(),
                (value_w.len() * 2) as u32,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!(
                "cannot write the registry value {name} (error {status})"
            ))
        }
    }

    fn set_dword(&self, name: &str, value: u32) -> Result<(), String> {
        let name_w = wide(name);
        // SAFETY: the data is four bytes.
        let status = unsafe {
            RegSetValueExW(
                self.0,
                name_w.as_ptr(),
                0,
                REG_DWORD,
                (&value as *const u32).cast(),
                4,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!(
                "cannot write the registry value {name} (error {status})"
            ))
        }
    }

    fn delete_value(&self, name: &str) -> Result<(), String> {
        let name_w = wide(name);
        // SAFETY: the name is a valid null-terminated string.
        match unsafe { RegDeleteValueW(self.0, name_w.as_ptr()) } {
            ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
            code => Err(format!(
                "cannot remove the registry value {name} (error {code})"
            )),
        }
    }

    /// Whether the key has no values and no subkeys.
    fn is_empty(&self) -> bool {
        let mut subkeys = 0u32;
        let mut values = 0u32;
        let none = std::ptr::null_mut::<u32>;
        // SAFETY: only the two counts are requested; every other output is null.
        let status = unsafe {
            RegQueryInfoKeyW(
                self.0,
                std::ptr::null_mut(),
                none(),
                none(),
                &mut subkeys,
                none(),
                none(),
                &mut values,
                none(),
                none(),
                none(),
                std::ptr::null_mut(),
            )
        };
        status == ERROR_SUCCESS && subkeys == 0 && values == 0
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the handle came from RegOpenKeyExW or RegCreateKeyExW.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn delete_tree(path: &str) -> Result<(), String> {
    let path_w = wide(path);
    // SAFETY: the path is a valid null-terminated string.
    match unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, path_w.as_ptr()) } {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
        code => Err(format!(
            "cannot remove the registry key {path} (error {code})"
        )),
    }
}

// ----- PATH --------------------------------------------------------------------------

fn same_directory(a: &str, b: &str) -> bool {
    let clean = |s: &str| s.trim().trim_end_matches(['\\', '/']).to_lowercase();
    clean(a) == clean(b)
}

/// `PATH` with `entry` first, or `None` when it is already there.
pub fn path_with(path: &str, entry: &str) -> Option<String> {
    if path.split(';').any(|e| same_directory(e, entry)) {
        return None;
    }
    if path.trim().is_empty() {
        Some(entry.to_string())
    } else {
        Some(format!("{entry};{path}"))
    }
}

/// `PATH` without `entry`, or `None` when it is not there.
pub fn path_without(path: &str, entry: &str) -> Option<String> {
    let entries: Vec<&str> = path.split(';').collect();
    if !entries.iter().any(|e| same_directory(e, entry)) {
        return None;
    }
    let kept: Vec<&str> = entries
        .into_iter()
        .filter(|e| !same_directory(e, entry))
        .collect();
    Some(kept.join(";"))
}

const ENVIRONMENT: &str = "Environment";

fn change_user_path(change: impl Fn(&str) -> Option<String>) -> Result<bool, String> {
    let key = Key::create(ENVIRONMENT)?;
    let (current, kind) = key
        .string("Path")?
        .unwrap_or((String::new(), REG_EXPAND_SZ));
    let Some(updated) = change(&current) else {
        return Ok(false);
    };
    if updated.is_empty() {
        key.delete_value("Path")?;
    } else {
        key.set_string("Path", &updated, kind)?;
    }
    Ok(true)
}

/// Tells running programs (Explorer in particular) that the environment
/// changed, so terminals opened from now on see the new `PATH`.
fn announce_environment() {
    const HWND_BROADCAST: isize = 0xFFFF;
    const WM_SETTINGCHANGE: u32 = 0x001A;
    const SMTO_ABORTIFHUNG: u32 = 0x0002;
    let name = wide(ENVIRONMENT);
    let mut result = 0usize;
    // SAFETY: the message carries a pointer to a null-terminated string that
    // outlives the call.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            name.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5000,
            &mut result,
        );
    }
}

// ----- registration -----------------------------------------------------------------------

const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Sayon";
const EXTENSION_KEY: &str = r"Software\Classes\.sayon";
const FILE_TYPE: &str = "Sayon.Source";
const FILE_TYPE_KEY: &str = r"Software\Classes\Sayon.Source";

pub const EXECUTABLE_SUFFIX: &str = ".exe";
pub const UNINSTALLER: &str = "uninstall.exe";

/// `%LOCALAPPDATA%\Programs\Sayon`, where per-user programs live.
pub fn default_prefix() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("Programs").join("Sayon"))
}

pub fn bin_directory(prefix: &Path) -> PathBuf {
    prefix.join("bin")
}

/// What `register` changes, for the confirmation prompt.
pub fn changes(prefix: &Path, modify_path: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if modify_path {
        lines.push(format!(
            "adds {} to your PATH, so `sayon` works in new terminals",
            bin_directory(prefix).display()
        ));
    }
    lines.push("adds Sayon to Settings > Apps, where it can be uninstalled".to_string());
    lines.push(
        "shows the Sayon icon on .sayon files, unless another program already opens them"
            .to_string(),
    );
    lines
}

/// Makes the installation available to the user.
pub fn register(prefix: &Path, version: &str, bytes: u64, modify_path: bool) -> Result<(), String> {
    let bin = bin_directory(prefix).display().to_string();
    if modify_path {
        change_user_path(|path| path_with(path, &bin))?;
    }

    let key = Key::create(UNINSTALL_KEY)?;
    let location = prefix.display().to_string();
    let uninstaller = prefix.join(UNINSTALLER).display().to_string();
    let icon = prefix.join("bin").join("sayon.exe").display().to_string();
    key.set_string("DisplayName", "Sayon", REG_SZ)?;
    key.set_string("DisplayVersion", version, REG_SZ)?;
    key.set_string("Publisher", "Omnifex Studio", REG_SZ)?;
    key.set_string(
        "URLInfoAbout",
        "https://github.com/Omnifex-Studio/sayon",
        REG_SZ,
    )?;
    key.set_string("InstallLocation", &location, REG_SZ)?;
    key.set_string("DisplayIcon", &icon, REG_SZ)?;
    key.set_string(
        "UninstallString",
        &format!("\"{uninstaller}\" uninstall"),
        REG_SZ,
    )?;
    key.set_string(
        "QuietUninstallString",
        &format!("\"{uninstaller}\" uninstall --yes"),
        REG_SZ,
    )?;
    key.set_dword("EstimatedSize", (bytes / 1024).min(u32::MAX as u64) as u32)?;
    key.set_dword("NoModify", 1)?;
    key.set_dword("NoRepair", 1)?;
    key.set_dword("SayonModifiedPath", modify_path as u32)?;

    Key::create(FILE_TYPE_KEY)?.set_string("", "Sayon source file", REG_SZ)?;
    Key::create(&format!(r"{FILE_TYPE_KEY}\DefaultIcon"))?.set_string(
        "",
        &format!("\"{icon}\",0"),
        REG_SZ,
    )?;
    // Claim `.sayon` only when no other program handles it, so a choice
    // the user made (such as "Open with" an editor) is never replaced.
    let extension = Key::create(EXTENSION_KEY)?;
    let free = match extension.string("")? {
        None => true,
        Some((value, _)) => value.is_empty() || value == FILE_TYPE,
    };
    if free {
        extension.set_string("", FILE_TYPE, REG_SZ)?;
    }
    refresh();
    Ok(())
}

/// Undoes `register` for the installation at `prefix`.
pub fn unregister(prefix: &Path) -> Result<(), String> {
    let bin = bin_directory(prefix).display().to_string();
    change_user_path(|path| path_without(path, &bin))?;
    delete_tree(UNINSTALL_KEY)?;
    // Only Sayon names its file type `Sayon.Source`; other programs'
    // settings for `.sayon` stay.
    if let Some(extension) = Key::open(EXTENSION_KEY)? {
        if extension
            .string("")?
            .is_some_and(|(value, _)| value == FILE_TYPE)
        {
            extension.delete_value("")?;
        }
        let empty = extension.is_empty();
        drop(extension);
        if empty {
            delete_tree(EXTENSION_KEY)?;
        }
    }
    delete_tree(FILE_TYPE_KEY)?;
    refresh();
    Ok(())
}

/// The installation location recorded by a previous installation.
pub fn registered_prefix() -> Option<PathBuf> {
    let key = Key::open(UNINSTALL_KEY).ok()??;
    let (location, _) = key.string("InstallLocation").ok()??;
    Some(PathBuf::from(location))
}

fn refresh() {
    const SHCNE_ASSOCCHANGED: i32 = 0x0800_0000;
    announce_environment();
    // SAFETY: SHCNE_ASSOCCHANGED takes no items.
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, 0, std::ptr::null(), std::ptr::null());
    }
}

// ----- the running program --------------------------------------------------------------------

/// Whether the installer has its own console window (it was opened from
/// Explorer or Settings rather than from a terminal), so it should wait
/// before the window closes.
pub fn owns_console() -> bool {
    let mut processes = [0u32; 2];
    // SAFETY: the list holds two entries.
    let count = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), 2) };
    count == 1
}

pub fn make_executable_if_needed(_path: &Path, _relative: &str) {}

/// Windows cannot delete a running program, so the uninstaller moves
/// itself out of the folder being removed. Returns where it went.
pub fn move_running_uninstaller(prefix: &Path) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(exe).ok()?;
    let prefix = std::fs::canonicalize(prefix).ok()?;
    if !exe.starts_with(&prefix) {
        return None;
    }
    let target = std::env::temp_dir().join(format!("sayon-uninstall-{}.exe", std::process::id()));
    std::fs::rename(&exe, &target).ok()?;
    Some(target)
}

/// Deletes the running program, and the folder it was in if that is empty,
/// a moment after this process exits.
pub fn delete_after_exit(file: &Path, folder: Option<&Path>) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = format!(
        "ping -n 3 127.0.0.1 > nul & del /f /q \"{}\"",
        file.display()
    );
    if let Some(folder) = folder {
        command.push_str(&format!(" & rmdir \"{}\"", folder.display()));
    }
    let _ = std::process::Command::new("cmd.exe")
        .raw_arg("/d /c")
        .raw_arg(format!("\"{command}\""))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_entries_are_added_once_and_removed() {
        let dir = r"C:\Users\Ada\AppData\Local\Programs\Sayon\bin";
        assert_eq!(path_with("", dir).as_deref(), Some(dir));
        let added = path_with(r"C:\Tools;%USERPROFILE%\bin", dir).unwrap();
        assert_eq!(added, format!(r"{dir};C:\Tools;%USERPROFILE%\bin"));
        assert_eq!(path_with(&added, &dir.to_uppercase()), None);
        assert_eq!(path_with(&format!(r"C:\Tools;{dir}\"), dir), None);
        assert_eq!(
            path_without(&added, dir).as_deref(),
            Some(r"C:\Tools;%USERPROFILE%\bin")
        );
        assert_eq!(path_without(r"C:\Tools", dir), None);
        assert_eq!(path_without(dir, dir).as_deref(), Some(""));
    }
}
