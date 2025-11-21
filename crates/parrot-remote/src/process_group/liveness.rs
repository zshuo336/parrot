//! Read OS group liveness while the unreaped guardian still pins its identity.
//! Zombies have relinquished their resources; a dead Linux thread-group leader
//! does not prove that its other threads have done so.
use std::{collections::BTreeSet, time::Instant};

fn check_deadline(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        Err("gateway group retirement observation deadline".into())
    } else {
        Ok(())
    }
}

#[cfg(any(target_os = "macos", test))]
fn any_live_member<T>(
    members: impl IntoIterator<Item = T>,
    deadline: Instant,
    mut observe: impl FnMut(T) -> Result<bool, String>,
) -> Result<bool, String> {
    let mut failure = None;
    for member in members {
        check_deadline(deadline)?;
        let result = observe(member);
        check_deadline(deadline)?;
        match result {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(error) => {
                failure.get_or_insert(error);
            }
        }
    }
    // One observed live member proves liveness. Proving absence requires every
    // member to be observable, so an earlier read error must survive this scan.
    failure.map_or(Ok(false), Err)
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

/// Return every live process group observed inside a pinned session.
///
/// A session leader is not required to keep descendants in its process group:
/// Ray and other runtimes may call `setpgrp()` while retaining the same
/// session.  Retirement must therefore snapshot all groups before signalling
/// the leader group.  The caller still owns the session leader, so these IDs
/// remain scoped to that session rather than to arbitrary process IDs.
#[cfg(target_os = "linux")]
pub(super) fn session_groups(session: u32, deadline: Instant) -> Result<Vec<u32>, String> {
    let mut groups = BTreeSet::new();
    for process in std::fs::read_dir("/proc").map_err(|e| e.to_string())? {
        check_deadline(deadline)?;
        let process = match process {
            Ok(process) => process,
            Err(error) if disappeared(&error) => continue,
            Err(error) => return Err(error.to_string()),
        };
        if !process
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let Some((group, actual_session, state)) = stat(&process.path().join("stat"))? else {
            continue;
        };
        if actual_session == session as i32 && group > 0 && !matches!(state, b'Z' | b'X' | b'x') {
            groups.insert(group as u32);
        }
    }
    Ok(groups.into_iter().collect())
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
    any_live_member(&pids[..count], deadline, |pid| {
        if *pid <= 0 {
            return Ok(false);
        }
        if session_scope {
            // Filter before requesting BSD process details for unrelated users.
            let sid = unsafe { libc::getsid(*pid) };
            if sid == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(false);
                }
                return Err(format!("read gateway session: {error}"));
            }
            if sid != group as i32 {
                return Ok(false);
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
                return Ok(false);
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
                    return Ok(false);
                }
                return Err(format!("read gateway session: {error}"));
            }
            sid == group as i32
        } else {
            info.pbi_pgid == group
        };
        Ok(same_scope && info.pbi_status != libc::SZOMB)
    })
}

#[cfg(target_os = "macos")]
fn all_pids(deadline: Instant) -> Result<Vec<i32>, String> {
    let mut pids = vec![0i32; 256];
    loop {
        check_deadline(deadline)?;
        let bytes = std::mem::size_of_val(pids.as_slice());
        let result = unsafe {
            *libc::__error() = 0;
            libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes as i32)
        };
        let error = std::io::Error::last_os_error();
        if result < 0
            || (result == 0 && !matches!(error.raw_os_error(), Some(0) | Some(libc::ESRCH)))
        {
            return Err(format!("list gateway session processes: {error}"));
        }
        if result as usize >= bytes {
            if pids.len() >= 65536 {
                return Err("gateway session exceeds retirement observation limit".into());
            }
            pids.resize(pids.len() * 2, 0);
            continue;
        }
        if !(result as usize).is_multiple_of(std::mem::size_of::<i32>()) {
            return Err("invalid session process list size".into());
        }
        return Ok(pids[..result as usize / std::mem::size_of::<i32>()].to_vec());
    }
}

#[cfg(target_os = "macos")]
pub(super) fn session_groups(session: u32, deadline: Instant) -> Result<Vec<u32>, String> {
    let mut groups = BTreeSet::new();
    for pid in all_pids(deadline)? {
        check_deadline(deadline)?;
        if pid <= 0 {
            continue;
        }
        let sid = unsafe { libc::getsid(pid) };
        if sid == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(format!("read gateway session: {error}"));
        }
        if sid != session as i32 {
            continue;
        }
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let expected = std::mem::size_of::<libc::proc_bsdinfo>();
        let bytes = unsafe {
            libc::proc_pidinfo(
                pid,
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
            return Err(format!("read gateway session member: {error}"));
        }
        let info = unsafe { info.assume_init() };
        if info.pbi_pgid > 0 && info.pbi_status != libc::SZOMB {
            groups.insert(info.pbi_pgid);
        }
    }
    Ok(groups.into_iter().collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn scan(results: impl IntoIterator<Item = Result<bool, String>>) -> Result<bool, String> {
        any_live_member(results, Instant::now() + Duration::from_secs(5), |r| r)
    }

    #[test]
    fn inaccessible_member_does_not_hide_a_later_live_member() {
        assert_eq!(
            scan([Err("permission denied".into()), Ok(false), Ok(true)]),
            Ok(true)
        );
        assert_eq!(scan([Ok(true), Err("permission denied".into())]), Ok(true));
    }

    #[test]
    fn read_failures_never_prove_a_group_is_empty() {
        assert_eq!(
            scan([Ok(false), Err("permission denied".into()), Ok(false)]),
            Err("permission denied".into())
        );
        assert_eq!(scan([Err("io failure".into())]), Err("io failure".into()));
        assert_eq!(scan([Ok(false), Ok(false)]), Ok(false));
        assert_eq!(scan([]), Ok(false));
    }

    #[test]
    fn a_live_result_after_the_deadline_does_not_hide_the_timeout() {
        let deadline = Instant::now();
        assert!(any_live_member([true], deadline, |_| panic!("already expired")).is_err());
        let deadline = Instant::now() + Duration::from_millis(10);
        assert!(any_live_member([true], deadline, |_| {
            while Instant::now() < deadline {
                std::hint::spin_loop();
            }
            Ok(true)
        })
        .is_err());
    }
}
