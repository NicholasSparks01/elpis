//! Host-side secrets for the credential proxy, like `sbx secret set`: one per kit service (`anthropic`,
//! `openai`, `github`, …), kept in Windows Credential Manager as generic credentials named `elpis/SERVICE`
//! (private to the Windows user, encrypted at rest by Windows). They never enter a box: the proxy adds them
//! to matching requests on the way out.

use anyhow::{Result, bail};
use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, FILETIME, GetLastError};
use windows_sys::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredEnumerateW, CredFree, CredReadW, CredWriteW,
};

const PREFIX: &str = "elpis/";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn check_service(service: &str) -> Result<()> {
    if service.is_empty() || !service.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
        bail!("service names are letters, digits, '-', '_' and '.' (got {service:?})");
    }
    Ok(())
}

/// Stores (or replaces) the secret for `service`.
pub fn set(service: &str, secret: &str) -> Result<()> {
    check_service(service)?;
    let mut target = wide(&format!("{PREFIX}{service}"));
    let mut user = wide("elpis");
    let blob = secret.as_bytes();
    let cred = CREDENTIALW {
        Flags: 0,
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_mut_ptr(),
        Comment: std::ptr::null_mut(),
        LastWritten: FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 },
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_ptr() as *mut u8,
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: std::ptr::null_mut(),
        TargetAlias: std::ptr::null_mut(),
        UserName: user.as_mut_ptr(),
    };
    // SAFETY: every pointer in `cred` is valid for the call
    if unsafe { CredWriteW(&cred, 0) } == 0 {
        bail!("saving the secret: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// The secret for `service`, if one is stored.
pub fn get(service: &str) -> Option<String> {
    check_service(service).ok()?;
    let target = wide(&format!("{PREFIX}{service}"));
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: target is NUL-terminated; on success cred points to memory freed with CredFree below
    if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) } == 0 {
        return None;
    }
    let value = unsafe {
        let c = &*cred;
        let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
        String::from_utf8_lossy(bytes).into_owned()
    };
    unsafe { CredFree(cred as _) };
    Some(value)
}

/// Removes the secret for `service`; true if there was one.
pub fn remove(service: &str) -> Result<bool> {
    check_service(service)?;
    let target = wide(&format!("{PREFIX}{service}"));
    // SAFETY: target is NUL-terminated
    if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
        if unsafe { GetLastError() } == ERROR_NOT_FOUND {
            return Ok(false);
        }
        bail!("removing the secret: {}", std::io::Error::last_os_error());
    }
    Ok(true)
}

/// The services that have a secret stored.
pub fn list() -> Vec<String> {
    let filter = wide(&format!("{PREFIX}*"));
    let (mut count, mut creds): (u32, *mut *mut CREDENTIALW) = (0, std::ptr::null_mut());
    // SAFETY: on success creds is an array of `count` pointers, freed with CredFree below
    if unsafe { CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut creds) } == 0 {
        return Vec::new();
    }
    let mut services = Vec::new();
    for i in 0..count as usize {
        let name = unsafe {
            let c = &**creds.add(i);
            let mut len = 0;
            while *c.TargetName.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(c.TargetName, len))
        };
        if let Some(service) = name.strip_prefix(PREFIX) {
            services.push(service.to_owned());
        }
    }
    unsafe { CredFree(creds as _) };
    services.sort();
    services
}
