//! Tiny HTTP(S) GET client on top of WinHTTP (no TLS library needed).

use std::ffi::c_void;

use windows_sys::Win32::Networking::WinHttp::*;

use crate::win::wide;

struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// GET `url`. `insecure` accepts self-signed certificates (League's local API).
pub fn get(url: &str, timeout_ms: i32, insecure: bool) -> Result<Vec<u8>, String> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err("bad url".into());
    };
    let (hostport, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, "/".into()));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| "bad port")?),
        None => (hostport.to_string(), if secure { 443 } else { 80 }),
    };
    unsafe {
        let session = Handle(WinHttpOpen(wide("Fragtape").as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                                         std::ptr::null(), std::ptr::null(), 0));
        if session.0.is_null() {
            return Err("WinHttpOpen failed".into());
        }
        WinHttpSetTimeouts(session.0, timeout_ms, timeout_ms, timeout_ms, timeout_ms);
        let conn = Handle(WinHttpConnect(session.0, wide(&host).as_ptr(), port, 0));
        if conn.0.is_null() {
            return Err("connect failed".into());
        }
        let req = Handle(WinHttpOpenRequest(conn.0, wide("GET").as_ptr(), wide(&path).as_ptr(), std::ptr::null(),
                                            std::ptr::null(), std::ptr::null(),
                                            if secure { WINHTTP_FLAG_SECURE } else { 0 }));
        if req.0.is_null() {
            return Err("request failed".into());
        }
        if insecure {
            let flags: u32 = SECURITY_FLAG_IGNORE_UNKNOWN_CA | SECURITY_FLAG_IGNORE_CERT_CN_INVALID
                | SECURITY_FLAG_IGNORE_CERT_DATE_INVALID | SECURITY_FLAG_IGNORE_CERT_WRONG_USAGE;
            WinHttpSetOption(req.0, WINHTTP_OPTION_SECURITY_FLAGS, &flags as *const u32 as *const c_void, 4);
        }
        if WinHttpSendRequest(req.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(req.0, std::ptr::null_mut()) == 0
        {
            return Err(format!("no response from {host}:{port}"));
        }
        let mut status: u32 = 0;
        let mut len = 4u32;
        WinHttpQueryHeaders(req.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, std::ptr::null(),
                            &mut status as *mut u32 as *mut c_void, &mut len, std::ptr::null_mut());
        let mut body = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            if WinHttpReadData(req.0, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut read) == 0 {
                return Err("read failed".into());
            }
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buf[..read as usize]);
        }
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        Ok(body)
    }
}

pub fn get_json(url: &str, timeout_ms: i32, insecure: bool) -> Result<serde_json::Value, String> {
    serde_json::from_slice(&get(url, timeout_ms, insecure)?).map_err(|e| e.to_string())
}
