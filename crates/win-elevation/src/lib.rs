//! A safe, first-party wrapper for the one Win32 query `cola doctor` needs on
//! Windows (spec #948, ticket #952).
//!
//! The main crate is `#![cfg_attr(not(test), forbid(unsafe_code))]`, so it
//! cannot call `windows-sys` directly. This crate owns that `unsafe` in one
//! small, reviewed place and exposes a safe `Result` — crucially one that
//! distinguishes a failed query from a non-elevated token. The `is_elevated`
//! crate used earlier collapsed those into `false`, which made a token nobody
//! could read look like a healthy non-elevated one.
//!
//! The token handle is owned for the duration of the query, so it is always
//! closed (the handle from `OpenProcessToken` is a real kernel handle).

#![cfg(windows)]

use std::io;
use std::mem;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `GetTokenInformation(TokenElevation)` for this process.
///
/// `Ok(true)` means the token is elevated (administrator); `Ok(false)` means
/// it is not. `Err` means the query itself failed, so the caller must not
/// assume either answer.
pub fn token_elevation() -> Result<bool, String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle, and `OpenProcessToken`
    // writes a new owned handle into `token` on success.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(format!("OpenProcessToken failed: {}", io::Error::last_os_error()));
    }

    // SAFETY: `OpenProcessToken` succeeded, so `token` is a valid owned handle.
    // `OwnedHandle` closes it on drop, including on the error path below.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };

    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    // SAFETY: the buffer is a correctly sized and aligned `TOKEN_ELEVATION`,
    // the token handle is live, and `returned` is a valid out-parameter.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevation,
            std::ptr::addr_of_mut!(elevation).cast(),
            mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    } == 0
    {
        return Err(format!(
            "GetTokenInformation(TokenElevation) failed: {}",
            io::Error::last_os_error()
        ));
    }

    Ok(elevation.TokenIsElevated != 0)
}

#[cfg(test)]
mod tests {
    /// A process token is always readable by its owner; the query must return
    /// an answer rather than the fail-open `false` the previous wrapper gave.
    #[test]
    fn the_token_query_answers() {
        assert!(super::token_elevation().is_ok());
    }
}
