//! Small Win32 helpers: local time for log lines, and finding the long-lived shim process.

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, SYSTEMTIME};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

/// "HH:mm:ss.fff" in local time.
pub fn local_time() -> String {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!("{:02}:{:02}:{:02}.{:03}", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds)
}

/// (pid, parent pid, exe name) for every running process.
fn processes() -> Vec<(u32, u32, String)> {
    let mut list = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return list;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snapshot, &mut entry);
        while ok != 0 {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            list.push((entry.th32ProcessID, entry.th32ParentProcessID, String::from_utf16_lossy(&entry.szExeFile[..len])));
            ok = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
    }
    list
}

/// The shim's "start" launcher starts the long-lived shim as its child, then exits. It has other children
/// too (its console host), so match the executable name.
pub fn find_child(parent: u32, exe: &str) -> Option<u32> {
    processes().into_iter().find(|(_, p, name)| *p == parent && name.eq_ignore_ascii_case(exe)).map(|(pid, ..)| pid)
}

/// Works even for the shim, whose process object Sailor locks down.
pub fn process_exists(pid: u32) -> bool {
    processes().iter().any(|(p, ..)| *p == pid)
}

/// CPU time used so far and private memory of a process. For the shim this is the VM: its vCPUs are
/// shim threads and guest RAM is shim memory. Sailor's hardened DACL still allows limited-query access.
#[derive(Clone, Copy, Debug)]
pub struct ProcessSample {
    /// Total kernel + user CPU time, in 100 ns units
    pub cpu_time: u64,
    pub private_bytes: u64,
    pub taken: std::time::Instant,
}

pub fn sample_process(pid: u32) -> Option<ProcessSample> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
            (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed());
        let mut mem: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        mem.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let times_ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        let mem_ok = GetProcessMemoryInfo(handle, &mut mem as *mut _ as *mut _, mem.cb) != 0;
        CloseHandle(handle);
        if !times_ok || !mem_ok {
            return None;
        }
        let ft = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        Some(ProcessSample { cpu_time: ft(kernel) + ft(user), private_bytes: mem.PrivateUsage as u64, taken: std::time::Instant::now() })
    }
}

/// CPU use between two samples, as a percentage of the whole machine (all logical CPUs = 100%).
pub fn cpu_percent(before: &ProcessSample, after: &ProcessSample) -> f64 {
    let wall = after.taken.duration_since(before.taken).as_secs_f64();
    if wall <= 0.0 {
        return 0.0;
    }
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    let used = after.cpu_time.saturating_sub(before.cpu_time) as f64 / 10_000_000.0;
    (used / wall / cpus * 100.0).clamp(0.0, 100.0)
}

/// (total, available) physical memory in bytes.
pub fn host_memory() -> (u64, u64) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut status) == 0 {
            return (0, 0);
        }
        (status.ullTotalPhys, status.ullAvailPhys)
    }
}

/// Copy a sparse file, writing only the ranges that hold data, so a mostly-empty 8 GB disk image stays small.
pub fn sparse_copy(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{FILE_ALLOCATED_RANGE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES, FSCTL_SET_SPARSE};

    let mut input = std::fs::File::open(src)?;
    let len = input.metadata()?.len();
    let mut output = std::fs::File::create(dst)?;
    let mut returned = 0u32;
    // SAFETY: valid handle; FSCTL_SET_SPARSE takes no input
    unsafe {
        DeviceIoControl(output.as_raw_handle() as _, FSCTL_SET_SPARSE, std::ptr::null(), 0, std::ptr::null_mut(), 0, &mut returned, std::ptr::null_mut())
    };
    output.set_len(len)?;

    let mut ranges = Vec::new();
    let mut query = FILE_ALLOCATED_RANGE_BUFFER { FileOffset: 0, Length: len as i64 };
    loop {
        let mut buf = vec![FILE_ALLOCATED_RANGE_BUFFER { FileOffset: 0, Length: 0 }; 512];
        let size = std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
        // SAFETY: in/out buffers are sized as passed; ERROR_MORE_DATA (234) means call again from the last range
        let ok = unsafe {
            DeviceIoControl(
                input.as_raw_handle() as _,
                FSCTL_QUERY_ALLOCATED_RANGES,
                &query as *const _ as _,
                size as u32,
                buf.as_mut_ptr() as _,
                (buf.len() * size) as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        let more = ok == 0 && std::io::Error::last_os_error().raw_os_error() == Some(234);
        if ok == 0 && !more {
            return Err(std::io::Error::last_os_error());
        }
        let got = &buf[..returned as usize / size];
        ranges.extend(got.iter().map(|r| (r.FileOffset as u64, r.Length as u64)));
        match (more, got.last()) {
            (true, Some(last)) => {
                let next = (last.FileOffset + last.Length) as u64;
                query = FILE_ALLOCATED_RANGE_BUFFER { FileOffset: next as i64, Length: (len - next) as i64 };
            }
            _ => break,
        }
    }

    let mut chunk = vec![0u8; 1 << 20];
    for (offset, length) in ranges {
        input.seek(SeekFrom::Start(offset))?;
        output.seek(SeekFrom::Start(offset))?;
        let mut left = length;
        while left > 0 {
            let n = left.min(chunk.len() as u64) as usize;
            input.read_exact(&mut chunk[..n])?;
            // Skip zero blocks, keeping them sparse
            if chunk[..n].iter().all(|&b| b == 0) {
                output.seek(SeekFrom::Current(n as i64))?;
            } else {
                output.write_all(&chunk[..n])?;
            }
            left -= n as u64;
        }
    }
    output.set_len(len)?;
    output.sync_all()
}

/// Disk space a (possibly sparse or compressed) file actually takes.
pub fn allocated_size(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetCompressedFileSizeW, INVALID_FILE_SIZE};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut high = 0u32;
    // SAFETY: NUL-terminated path, valid out pointer
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
    if low == INVALID_FILE_SIZE && std::io::Error::last_os_error().raw_os_error() != Some(0) {
        return None;
    }
    Some(((high as u64) << 32) | low as u64)
}

/// Make `pid` end when this process does, however it ends (Ctrl-C, closed window, crash): it joins a
/// kill-on-close job object whose only handle this process holds until it exits.
pub fn tie_to_this_process(pid: u32) -> std::io::Result<()> {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| {
        // SAFETY: plain Win32 calls; the handle is kept (never closed) for the life of the process
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if !job.is_null() {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
            }
            job as usize
        }
    });
    if job == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: valid pid lookup; the process handle is closed after assignment
    unsafe {
        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let ok = AssignProcessToJobObject(job as _, process);
        let err = std::io::Error::last_os_error();
        CloseHandle(process);
        if ok == 0 { Err(err) } else { Ok(()) }
    }
}
