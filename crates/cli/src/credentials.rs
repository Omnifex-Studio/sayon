//! The registry tokens `sayon login` keeps, one for each registry: in the
//! Windows Credential Manager, or elsewhere in a file only its user can
//! read, `credentials` in Sayon's configuration folder. `SAYON_CREDENTIALS`
//! names a file to use instead on every system.

use std::path::PathBuf;

/// A token, and the account it belongs to.
pub struct Saved {
    pub account: String,
    pub token: String,
}

/// The file `SAYON_CREDENTIALS` names, or, but on Windows, the one in the
/// configuration folder.
fn file() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SAYON_CREDENTIALS") {
        return Some(PathBuf::from(path));
    }
    if cfg!(windows) {
        return None;
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(base) => PathBuf::from(base),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("sayon").join("credentials"))
}

/// Where tokens are kept, for messages.
pub fn place() -> String {
    match file() {
        Some(path) => path.display().to_string(),
        None => "the Windows Credential Manager".to_string(),
    }
}

pub fn load(registry: &str) -> Result<Option<Saved>, String> {
    match file() {
        Some(path) => Ok(read_file(&path)?
            .into_iter()
            .find(|(address, _)| address == registry)
            .map(|(_, saved)| saved)),
        None => windows::load(registry),
    }
}

pub fn save(registry: &str, saved: &Saved) -> Result<(), String> {
    match file() {
        Some(path) => {
            let mut entries = read_file(&path)?;
            entries.retain(|(address, _)| address != registry);
            entries.push((
                registry.to_string(),
                Saved {
                    account: saved.account.clone(),
                    token: saved.token.clone(),
                },
            ));
            write_file(&path, &entries)
        }
        None => windows::save(registry, saved),
    }
}

/// Forgets a registry's token; whether there was one.
pub fn forget(registry: &str) -> Result<bool, String> {
    match file() {
        Some(path) => {
            let mut entries = read_file(&path)?;
            let before = entries.len();
            entries.retain(|(address, _)| address != registry);
            if entries.len() == before {
                return Ok(false);
            }
            write_file(&path, &entries)?;
            Ok(true)
        }
        None => windows::forget(registry),
    }
}

/// The file's lines: a registry's address, the account, and the token.
fn read_file(path: &std::path::Path) -> Result<Vec<(String, Saved)>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let (address, account, token) = (words.next()?, words.next()?, words.next()?);
            Some((
                address.to_string(),
                Saved {
                    account: account.to_string(),
                    token: token.to_string(),
                },
            ))
        })
        .collect())
}

fn write_file(path: &std::path::Path, entries: &[(String, Saved)]) -> Result<(), String> {
    let fail = |error: std::io::Error| format!("cannot write {}: {error}", path.display());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(fail)?;
    }
    let mut text = String::from("# Registry tokens of `sayon login`; keep this file private.\n");
    for (address, saved) in entries {
        text.push_str(&format!("{address} {} {}\n", saved.account, saved.token));
    }
    let temporary = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut out = options.open(&temporary).map_err(fail)?;
    std::io::Write::write_all(&mut out, text.as_bytes()).map_err(fail)?;
    drop(out);
    std::fs::rename(&temporary, path).map_err(fail)
}

#[cfg(windows)]
mod windows {
    use super::Saved;
    use std::ffi::c_void;

    const CRED_TYPE_GENERIC: u32 = 1;
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;
    const ERROR_NOT_FOUND: u32 = 1168;

    /// `CREDENTIALW`.
    #[repr(C)]
    struct Credential {
        flags: u32,
        kind: u32,
        target_name: *mut u16,
        comment: *mut u16,
        last_written: [u32; 2],
        blob_size: u32,
        blob: *mut u8,
        persist: u32,
        attribute_count: u32,
        attributes: *mut c_void,
        target_alias: *mut u16,
        user_name: *mut u16,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn CredWriteW(credential: *const Credential, flags: u32) -> i32;
        fn CredReadW(
            target: *const u16,
            kind: u32,
            flags: u32,
            credential: *mut *mut Credential,
        ) -> i32;
        fn CredDeleteW(target: *const u16, kind: u32, flags: u32) -> i32;
        fn CredFree(buffer: *mut c_void);
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLastError() -> u32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The credential's name, as the Credential Manager lists it.
    fn target(registry: &str) -> Vec<u16> {
        wide(&format!("sayon-registry:{registry}"))
    }

    fn failed(what: &str) -> String {
        let code = unsafe { GetLastError() };
        format!("cannot {what} the token in the Windows Credential Manager (error {code})")
    }

    pub fn load(registry: &str) -> Result<Option<Saved>, String> {
        let name = target(registry);
        let mut found: *mut Credential = std::ptr::null_mut();
        if unsafe { CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut found) } == 0 {
            if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                return Ok(None);
            }
            return Err(failed("read"));
        }
        // SAFETY: CredReadW succeeded, so `found` is a credential it
        // allocated, freed below.
        let saved = unsafe {
            let credential = &*found;
            let blob = std::slice::from_raw_parts(credential.blob, credential.blob_size as usize);
            let mut account = String::new();
            if !credential.user_name.is_null() {
                let mut length = 0;
                while *credential.user_name.add(length) != 0 {
                    length += 1;
                }
                account = String::from_utf16_lossy(std::slice::from_raw_parts(
                    credential.user_name,
                    length,
                ));
            }
            Saved {
                account,
                token: String::from_utf8_lossy(blob).into_owned(),
            }
        };
        unsafe { CredFree(found.cast()) };
        Ok(Some(saved))
    }

    pub fn save(registry: &str, saved: &Saved) -> Result<(), String> {
        let mut name = target(registry);
        let mut user = wide(&saved.account);
        let mut comment = wide("The registry token of `sayon login`");
        let mut token = saved.token.clone().into_bytes();
        let credential = Credential {
            flags: 0,
            kind: CRED_TYPE_GENERIC,
            target_name: name.as_mut_ptr(),
            comment: comment.as_mut_ptr(),
            last_written: [0, 0],
            blob_size: token.len() as u32,
            blob: token.as_mut_ptr(),
            persist: CRED_PERSIST_LOCAL_MACHINE,
            attribute_count: 0,
            attributes: std::ptr::null_mut(),
            target_alias: std::ptr::null_mut(),
            user_name: user.as_mut_ptr(),
        };
        if unsafe { CredWriteW(&credential, 0) } == 0 {
            return Err(failed("keep"));
        }
        Ok(())
    }

    pub fn forget(registry: &str) -> Result<bool, String> {
        let name = target(registry);
        if unsafe { CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                return Ok(false);
            }
            return Err(failed("remove"));
        }
        Ok(true)
    }
}

/// Elsewhere tokens are always in a file.
#[cfg(not(windows))]
mod windows {
    use super::Saved;

    pub fn load(_registry: &str) -> Result<Option<Saved>, String> {
        Err("there is no folder to keep tokens in: set HOME or SAYON_CREDENTIALS".to_string())
    }

    pub fn save(registry: &str, _saved: &Saved) -> Result<(), String> {
        load(registry).map(|_| ())
    }

    pub fn forget(registry: &str) -> Result<bool, String> {
        load(registry).map(|_| false)
    }
}
