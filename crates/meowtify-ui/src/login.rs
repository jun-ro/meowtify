//! Windows only: the in-app "Sign in to YouTube" flow.
//!
//! The sign-in window is this same program started with `--login` (see
//! `crates/meowtify/src/login.rs`), so the auto-updater only ever replaces meowtify.exe and
//! no second program ships. It is a separate process with its own WebView2 window, which
//! costs about 490 MB while it is open; the app itself only keeps a waiting thread, and
//! closing the window leaves nothing behind.
//!
//! What the helper writes is a Netscape cookies.txt. It is encrypted with DPAPI for this
//! Windows user into `config_dir()/youtube-login.bin`. A plaintext copy exists only as
//! `config_dir()/tmp/session-cookies.txt` while the app runs: it is made at start-up (or
//! right after signing in) and shredded at exit. Cookie values are never logged.

use std::io::Write;
use std::path::{Path, PathBuf};

use meowtify_library::config_dir;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
};

/// Where the encrypted cookie file lives. Its existence means "signed in".
pub fn bin_path() -> PathBuf {
    config_dir().join("youtube-login.bin")
}

/// The decrypted copy yt-dlp reads, for this run only.
pub fn session_path() -> PathBuf {
    config_dir().join("tmp").join("session-cookies.txt")
}

/// What the helper writes, before the app encrypts it and shreds it.
fn sign_in_path() -> PathBuf {
    config_dir().join("tmp").join("login-result.txt")
}

/// Prepares this run's plaintext copy and returns whether the user is signed in.
///
/// A file this Windows user cannot decrypt (another user or profile) is dropped: the app is
/// signed out rather than retrying a file it can never read.
pub fn start_session() -> bool {
    // A copy left by an earlier run (a crash) is never trusted.
    shred(&session_path());
    shred(&sign_in_path());
    if !bin_path().is_file() {
        meowtify_provider::set_login_cookies(None);
        return false;
    }
    let decrypted = std::fs::read(bin_path()).ok().and_then(|c| unprotect(&c).ok());
    let Some(cookies) = decrypted else {
        shred(&bin_path());
        meowtify_provider::set_login_cookies(None);
        return false;
    };
    if write_session(&cookies).is_err() {
        meowtify_provider::set_login_cookies(None);
        return false;
    }
    meowtify_provider::set_login_cookies(Some(session_path()));
    true
}

/// Runs the sign-in window and, when it succeeded, encrypts what it wrote. Called on a
/// background thread: the window is its own process and takes as long as the user needs.
/// `Ok(false)` means the user closed the window without signing in.
pub fn sign_in() -> Result<bool, String> {
    let temp = sign_in_path();
    let _ = std::fs::create_dir_all(temp.parent().unwrap_or(Path::new(".")));
    shred(&temp);
    let exe = std::env::current_exe().map_err(|e| format!("could not find this program: {e}"))?;
    let status = std::process::Command::new(exe)
        .arg("--login")
        .arg(&temp)
        .status()
        .map_err(|e| format!("could not open the sign-in window: {e}"))?;
    if status.code() != Some(0) {
        shred(&temp);
        return match status.code() {
            Some(2) => Ok(false),
            _ => Err("The sign-in window closed before it finished. Try again.".to_string()),
        };
    }
    let cookies = std::fs::read(&temp).map_err(|e| format!("could not read the sign-in: {e}"))?;
    if cookies.is_empty() {
        shred(&temp);
        return Err("The sign-in window wrote no cookies. Try again.".to_string());
    }
    // The plaintext serves this run; the encrypted copy serves the next ones.
    let saved = write_session(&cookies)
        .and_then(|()| protect(&cookies))
        .and_then(|sealed| std::fs::write(bin_path(), sealed).map_err(|e| format!("could not save the sign-in: {e}")));
    shred(&temp);
    match saved {
        Ok(()) => {
            meowtify_provider::set_login_cookies(Some(session_path()));
            Ok(true)
        }
        Err(e) => {
            // Put back whatever was signed in before, if anything still can be.
            shred(&session_path());
            start_session();
            Err(e)
        }
    }
}

/// Forgets the sign-in: both files go and yt-dlp stops being given cookies.
pub fn sign_out() {
    shred(&bin_path());
    shred(&session_path());
    meowtify_provider::set_login_cookies(None);
}

/// Deletes this run's plaintext copy, at app exit.
pub fn cleanup_session() {
    shred(&session_path());
    shred(&sign_in_path());
    meowtify_provider::set_login_cookies(None);
}

/// Writes the cookies where yt-dlp can read them: `config_dir()/tmp/session-cookies.txt`.
fn write_session(cookies: &[u8]) -> Result<(), String> {
    let path = session_path();
    let dir = path.parent().ok_or("no temp folder for the sign-in")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    std::fs::write(&path, cookies).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Overwrites a plaintext file with zeros, then removes it. Best effort: a missing file is
/// fine, and a file that cannot be written is still removed.
fn shred(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        let len = meta.len() as usize;
        if len > 0 {
            if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(path) {
                let zeros = [0u8; 4096];
                let mut left = len;
                while left > 0 {
                    let n = left.min(zeros.len());
                    if f.write_all(&zeros[..n]).is_err() {
                        break;
                    }
                    left -= n;
                }
                let _ = f.flush();
            }
        }
    }
    let _ = std::fs::remove_file(path);
}

/// Encrypts bytes for this Windows user (DPAPI, user scope). Never shows a prompt.
fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        let ok = CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        );
        if ok == 0 {
            return Err(format!("could not encrypt the sign-in: {}", std::io::Error::last_os_error()));
        }
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as *mut core::ffi::c_void);
        Ok(bytes)
    }
}

/// The reverse of `protect`. Fails for a file another Windows user or profile encrypted.
fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        let ok = CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        );
        if ok == 0 {
            return Err(format!("could not read the sign-in: {}", std::io::Error::last_os_error()));
        }
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as *mut core::ffi::c_void);
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trip_hides_the_plaintext() {
        let plain = b"# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tabc\n";
        let sealed = protect(plain).unwrap();
        assert_ne!(sealed.as_slice(), &plain[..], "the stored file must not hold the cookies in the clear");
        assert!(sealed.len() > plain.len(), "DPAPI adds its own header");
        assert_eq!(unprotect(&sealed).unwrap(), plain.to_vec());
    }

    #[test]
    fn shredding_overwrites_and_removes() {
        let dir = std::env::temp_dir().join(format!("meowtify-shred-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cookies.txt");
        std::fs::write(&path, b"secret").unwrap();
        shred(&path);
        assert!(!path.exists());
        shred(&path); // already gone: still fine
        let _ = std::fs::remove_dir_all(&dir);
    }
}
