use std::collections::HashSet;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

/// Best-effort process-tree termination for environments where nested Job
/// Objects are unavailable. The known set is retained across passes so children
/// whose parents exit during traversal remain attributable to the original root.
pub(crate) fn terminate_process_tree(root: u32) {
    let mut known = HashSet::from([root]);
    for _ in 0..4 {
        let entries = process_entries();
        loop {
            let before = known.len();
            for (pid, parent) in &entries {
                if known.contains(parent) {
                    known.insert(*pid);
                }
            }
            if known.len() == before {
                break;
            }
        }
        terminate_process(root);
        for pid in known.iter().copied().filter(|pid| *pid != root) {
            terminate_process(pid);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn process_entries() -> Vec<(u32, u32)> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut entries = Vec::new();
        let mut has_entry = Process32FirstW(snapshot, &mut entry) != 0;
        while has_entry {
            entries.push((entry.th32ProcessID, entry.th32ParentProcessID));
            has_entry = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        entries
    }
}

fn terminate_process(pid: u32) {
    unsafe {
        let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !process.is_null() {
            TerminateProcess(process, 1);
            CloseHandle(process);
        }
    }
}
