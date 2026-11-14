//! The Windows console: ANSI output, window size, and raw VT mode for TTY sessions.
//! Console I/O goes through ReadFile/WriteFile so bytes pass through untouched (UTF-8 code page).

use std::io;
use std::path::Path;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo,
    GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCP, SetConsoleMode, SetConsoleOutputCP,
};

const PROCESSED_INPUT: u32 = 0x1;
const LINE_INPUT: u32 = 0x2;
const ECHO_INPUT: u32 = 0x4;
const MOUSE_INPUT: u32 = 0x10;
const QUICK_EDIT: u32 = 0x40;
const EXTENDED_FLAGS: u32 = 0x80;
const VIRTUAL_TERMINAL_INPUT: u32 = 0x200;
const VIRTUAL_TERMINAL_PROCESSING: u32 = 0x4;
const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x8;
const UTF8: u32 = 65001;

fn stdin() -> HANDLE {
    unsafe { GetStdHandle(STD_INPUT_HANDLE) }
}

fn stdout() -> HANDLE {
    unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }
}

fn mode(h: HANDLE) -> Option<u32> {
    let mut m = 0;
    (unsafe { GetConsoleMode(h, &mut m) } != 0).then_some(m)
}

/// Lets log lines use ANSI colors.
pub fn enable_vt_output() {
    if let Some(m) = mode(stdout()) {
        unsafe { SetConsoleMode(stdout(), m | VIRTUAL_TERMINAL_PROCESSING) };
    }
}

pub fn is_interactive() -> bool {
    mode(stdin()).is_some() && mode(stdout()).is_some()
}

/// (columns, rows) of the visible window.
pub fn window_size() -> Option<(u16, u16)> {
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(stdout(), &mut info) } == 0 {
        return None;
    }
    let w = info.srWindow;
    Some(((w.Right - w.Left + 1) as u16, (w.Bottom - w.Top + 1) as u16))
}

pub fn read_input(buf: &mut [u8]) -> io::Result<usize> {
    let mut read = 0;
    if unsafe { ReadFile(stdin(), buf.as_mut_ptr(), buf.len() as u32, &mut read, std::ptr::null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(read as usize)
}

pub fn write_output(mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        let mut written = 0;
        if unsafe { WriteFile(stdout(), data.as_ptr(), data.len() as u32, &mut written, std::ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        data = &data[written as usize..];
    }
    Ok(())
}

/// Raw VT mode for a TTY session: keys (Ctrl-C, arrows, ...) arrive as bytes and escape sequences,
/// ANSI output renders, UTF-8 both ways. Everything is restored on drop.
pub struct RawConsole {
    in_mode: u32,
    out_mode: u32,
    in_cp: u32,
    out_cp: u32,
}

impl RawConsole {
    pub fn enter(log_path: &Path) -> Self {
        let raw = RawConsole {
            in_mode: mode(stdin()).unwrap_or(0),
            out_mode: mode(stdout()).unwrap_or(0),
            in_cp: unsafe { GetConsoleCP() },
            out_cp: unsafe { GetConsoleOutputCP() },
        };
        crate::log::mute_to(Some(log_path.to_path_buf()));
        unsafe {
            SetConsoleCP(UTF8);
            SetConsoleOutputCP(UTF8);
            SetConsoleMode(
                stdin(),
                (raw.in_mode & !(PROCESSED_INPUT | LINE_INPUT | ECHO_INPUT | MOUSE_INPUT | QUICK_EDIT))
                    | EXTENDED_FLAGS
                    | VIRTUAL_TERMINAL_INPUT,
            );
            SetConsoleMode(stdout(), raw.out_mode | VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN);
        }
        raw
    }
}

impl Drop for RawConsole {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(stdin(), self.in_mode);
            SetConsoleMode(stdout(), self.out_mode);
            SetConsoleCP(self.in_cp);
            SetConsoleOutputCP(self.out_cp);
        }
        crate::log::mute_to(None);
        let _ = write_output(b"\r\n");
    }
}
