//! Read OS group liveness while the unreaped guardian still pins its identity.
//! Zombies have relinquished their resources; a dead Linux thread-group leader
//! does not prove that its other threads have done so.
use std::time::Instant;

fn check_deadline(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        Err("gateway group retirement observation deadline".into())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn disappeared(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

#[cfg(target_os = "linux")]
fn stat(path: &std::path::Path) -> Result<Option<(i32, i32, u8)>, String> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if disappeared(&error) => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let mut bytes = Vec::new();
    if let Err(error) = file.take(4097).read_to_end(&mut bytes) {
        if disappeared(&error) {
            return Ok(None);
        }
        return Err(error.to_string());
    }
    if bytes.len() > 4096 {
        return Err("process stat exceeds retirement observation limit".into());
    }
    // comm can contain spaces, parentheses and non-UTF-8 bytes.
    let end = bytes
        .iter()
        .rposition(|b| *b == b')')
        .ok_or("invalid process stat")?;
    let fields: Vec<_> = bytes[end + 1..]
        .split(u8::is_ascii_whitespace)
        .filter(|part| !part.is_empty())
        .take(4)
        .collect();
    if fields.len() != 4 || fields[0].len() != 1 {
        return Err("invalid process stat fields".into());
    }
    // Linux can report pgrp=-1 while a task loses its group during exit.
    // pid_t is signed; that value cannot match our pinned positive PGID.
    let group = std::str::from_utf8(fields[2])
        .map_err(|e| e.to_string())?
        .parse::<i32>()
        .map_err(|e| format!("{}: pgrp {:?}: {e}", path.display(), fields[2]))?;
    let session = std::str::from_utf8(fields[3])
        .map_err(|e| e.to_string())?
        .parse::<i32>()
        .map_err(|e| e.to_string())?;
    Ok(Some((group, session, fields[0][0])))
}

#[cfg(target_os = "linux")]
fn member_live(
    path: &std::path::Path,
    group: u32,
    session_scope: bool,
    deadline: Instant,
) -> Result<bool, String> {
    check_deadline(deadline)?;
    let Some((actual_group, actual_session, state)) = stat(&path.join("stat"))? else {
        return Ok(false);
    };
    if (if session_scope {
        actual_session
    } else {
        actual_group
    }) != group as i32
    {
        return Ok(false);
    }
    if !matches!(state, b'Z' | b'X' | b'x') {
        return Ok(true);
    }
    let tasks = match std::fs::read_dir(path.join("task")) {
        Ok(tasks) => tasks,
        Err(error) if disappeared(&error) => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    for task in tasks {
        check_deadline(deadline)?;
        let task = match task {
            Ok(task) => task,
            Err(error) if disappeared(&error) => continue,
            Err(error) => return Err(error.to_string()),
        };
        if let Some((actual_group, actual_session, state)) = stat(&task.path().join("stat"))? {
            if (if session_scope {
                actual_session
            } else {
                actual_group
            }) == group as i32
                && !matches!(state, b'Z' | b'X' | b'x')
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn has_live_scope(group: u32, session_scope: bool, deadline: Instant) -> Result<bool, String> {
    for process in std::fs::read_dir("/proc").map_err(|e| e.to_string())? {
        check_deadline(deadline)?;
        let process = match process {
            Ok(process) => process,
            Err(error) if disappeared(&error) => continue,
            Err(error) => return Err(error.to_string()),
        };
        if process
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
            && member_live(&process.path(), group, session_scope, deadline)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "macos")]
fn has_live_scope(group: u32, session_scope: bool, deadline: Instant) -> Result<bool, String> {
    // Darwin sys/proc_info.h: PROC_PGRP_ONLY. No stored PID is signalled here.
    const PROC_PGRP_ONLY: u32 = 2;
    let mut pids = vec![0i32; 256];
    let count = loop {
        check_deadline(deadline)?;
        let bytes = std::mem::size_of_val(pids.as_slice());
        let result = unsafe {
            *libc::__error() = 0;
            libc::proc_listpids(
                if session_scope { 1 } else { PROC_PGRP_ONLY },
                if session_scope { 0 } else { group },
                pids.as_mut_ptr().cast(),
                bytes as i32,
            )
        };
        let error = std::io::Error::last_os_error();
        if result < 0
            || (result == 0 && !matches!(error.raw_os_error(), Some(0) | Some(libc::ESRCH)))
        {
            return Err(format!("list gateway group: {error}"));
        }
        if result as usize >= bytes {
            if pids.len() >= 65536 {
                return Err("gateway group exceeds retirement observation limit".into());
            }
            pids.resize(pids.len() * 2, 0);
            continue;
        }
        if !(result as usize).is_multiple_of(std::mem::size_of::<i32>()) {
            return Err("invalid group membership size".into());
        }
        break result as usize / std::mem::size_of::<i32>();
    };
    for pid in &pids[..count] {
        check_deadline(deadline)?;
        if *pid <= 0 {
            continue;
        }
        if session_scope {
            // Filter before requesting BSD process details for unrelated users.
            let sid = unsafe { libc::getsid(*pid) };
            if sid == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(format!("read gateway session: {error}"));
            }
            if sid != group as i32 {
                continue;
            }
        }
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let expected = std::mem::size_of::<libc::proc_bsdinfo>();
        let bytes = unsafe {
            libc::proc_pidinfo(
                *pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                expected as i32,
            )
        };
        if bytes as usize != expected {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(format!("read gateway group member: {error}"));
        }
        let info = unsafe { info.assume_init() };
        let same_scope = if session_scope {
            // Read-only observation: an ID from this scan never authorizes a signal.
            let sid = unsafe { libc::getsid(*pid) };
            if sid == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(format!("read gateway session: {error}"));
            }
            sid == group as i32
        } else {
            info.pbi_pgid == group
        };
        if same_scope && info.pbi_status != libc::SZOMB {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Observe a pinned process group without signalling any enumerated PID.
pub fn has_live_group(group: u32, deadline: Instant) -> Result<bool, String> {
    has_live_scope(group, false, deadline)
}

/// Ray workers may create their own groups within the guardian's session.
/// Success requires their exit too, even though only the owned leader group
/// receives signals. A member that escaped the group can cause a failure receipt.
pub(super) fn has_live_session(session: u32, deadline: Instant) -> Result<bool, String> {
    has_live_scope(session, true, deadline)
}
