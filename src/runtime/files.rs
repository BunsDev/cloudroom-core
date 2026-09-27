//! Open agent-controlled files under the agent identity, then retain the opened inode.
use std::{
    fs::File,
    io::{self, BufRead, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::Path,
    process::Stdio,
};
use tokio::process::Command;

pub(super) type Identity = Option<(u32, u32)>;

/// Reads one JSONL record of at most `max` bytes. Returns `Ok(None)` at end of file and while
/// the last record is still being appended. UTF-8 is checked only after the newline arrives,
/// so a record that currently ends inside a multibyte character is not an error.
pub(super) fn complete_line(
    reader: &mut impl BufRead,
    max: usize,
    too_large: &'static str,
) -> io::Result<Option<(usize, String)>> {
    let mut raw = Vec::new();
    let n = (&mut *reader)
        .take((max + 1) as u64)
        .read_until(b'\n', &mut raw)?;
    if n > max {
        return Err(io::Error::other(too_large));
    }
    if raw.last() != Some(&b'\n') {
        return Ok(None);
    }
    String::from_utf8(raw)
        .map(|line| Some((n, line)))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(super) fn checkpoint(
    previous: &serde_json::Value,
    data: &serde_json::Value,
    native: Option<&str>,
) -> io::Result<serde_json::Value> {
    let offset = data["offset"]
        .as_u64()
        .ok_or_else(|| io::Error::other("missing native offset"))?;
    if offset != previous["offset"].as_u64().unwrap_or(0) {
        return Err(io::Error::other("native record offset mismatch"));
    }
    let length = native
        .ok_or_else(|| io::Error::other("missing native record"))?
        .len() as u64;
    Ok(
        serde_json::json!({"offset":offset.checked_add(length).ok_or_else(||io::Error::other("native offset overflow"))?}),
    )
}

pub(super) fn images(
    handle: &super::Handle,
    input: &serde_json::Value,
    mut remaining: usize,
) -> io::Result<Vec<serde_json::Value>> {
    use serde_json::json;
    let mut images = Vec::new();
    let mut push = |path: &str| -> io::Result<()> {
        let limit = 10 * 1024 * 1024;
        let file = open(&handle.repository, Path::new(path), handle.file_identity)?;
        if file.metadata()?.len() > limit {
            return Err(io::Error::other("image exceeds the attachment size limit"));
        }
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(io::Error::other("image exceeds the attachment size limit"));
        }
        let mime = match Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("heic") => "image/heic",
            _ => "application/octet-stream",
        };
        let image = json!({"type":"image","data":base64_encode(&bytes),"mimeType":mime});
        remaining = remaining
            .checked_sub(serde_json::to_vec(&image)?.len() + 1)
            .ok_or_else(|| io::Error::other("images exceed the native request size limit"))?;
        images.push(image);
        Ok(())
    };
    if let Some(content) = input["content"].as_array() {
        for part in content {
            if matches!(part["type"].as_str(), Some("image" | "localImage"))
                && let Some(path) = part["path"].as_str().or_else(|| part["url"].as_str())
            {
                push(path)?;
            }
        }
    }
    if let Some(attachments) = input["attachments"].as_array() {
        for attachment in attachments {
            if attachment["kind"] == "image"
                && let Some(path) = attachment["path"].as_str()
            {
                push(path)?;
            }
        }
    }
    Ok(images)
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(TABLE[(n >> 18) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub(super) fn open(root: &Path, path: &Path, identity: Identity) -> io::Result<File> {
    open_entry(root, path, identity, false)
}

pub(super) fn open_directory(root: &Path, path: &Path, identity: Identity) -> io::Result<File> {
    open_entry(root, path, identity, true)
}

fn open_entry(root: &Path, path: &Path, identity: Identity, directory: bool) -> io::Result<File> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let root = root.canonicalize()?;
    // Normalize platform aliases (e.g. macOS /var), never resolve the final file link.
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("file has no parent"))?;
    let path = parent.canonicalize()?.join(
        path.file_name()
            .ok_or_else(|| io::Error::other("file has no name"))?,
    );
    let relative = path.strip_prefix(&root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "file is outside its allowed directory",
        )
    })?;
    let (mut socket, child_socket) = UnixStream::pair()?;
    socket.set_read_timeout(Some(super::SHUTDOWN_GRACE))?;
    let mut command = Command::new("python3");
    command
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .current_dir("/")
        .args(["-I", "-c", include_str!("../sync/files.py")])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(OwnedFd::from(child_socket)))
        .stderr(Stdio::null());
    if let Some((uid, gid)) = identity {
        super::linux::as_agent(&mut command, uid, gid, None)?;
    }
    let mut child = command.as_std_mut().spawn()?;
    drop(command);
    let result = (|| {
        let request = serde_json::json!({"op":"open", "tree":{"root":root}, "path":relative, "directory":directory});
        let mut input = child.stdin.take().unwrap();
        writeln!(input, "{request}")?;
        drop(input);
        let file = receive(&mut socket)?;
        let metadata = file.metadata()?;
        if if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file()
        } {
            return Err(io::Error::other("unexpected file type"));
        }
        Ok(file)
    })();
    // The file is already open; no helper or pathname is needed while tailing it.
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(target_os = "linux")]
type Length = usize;
#[cfg(not(target_os = "linux"))]
type Length = u32;

#[repr(C)]
struct IoVec {
    base: *mut u8,
    len: usize,
}
#[repr(C)]
struct Message {
    name: *mut u8,
    name_len: u32,
    iov: *mut IoVec,
    iov_len: Length,
    control: *mut Rights,
    control_len: Length,
    flags: i32,
}
#[repr(C)]
struct Rights {
    len: Length,
    level: i32,
    kind: i32,
    fd: i32,
}

fn receive(socket: &mut UnixStream) -> io::Result<File> {
    unsafe extern "C" {
        fn recvmsg(fd: i32, message: *mut Message, flags: i32) -> isize;
        fn fcntl(fd: i32, command: i32, argument: i32) -> i32;
    }
    #[cfg(target_os = "linux")]
    const SOCKET_LEVEL: i32 = 1;
    #[cfg(not(target_os = "linux"))]
    const SOCKET_LEVEL: i32 = 0xffff;
    let mut bytes = [0u8; 4096];
    let mut iov = IoVec {
        base: bytes.as_mut_ptr(),
        len: bytes.len(),
    };
    let mut rights = Rights {
        len: 0,
        level: 0,
        kind: 0,
        fd: -1,
    };
    let mut message = Message {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov: &mut iov,
        iov_len: 1,
        control: &mut rights,
        control_len: size_of::<Rights>() as Length,
        flags: 0,
    };
    // Linux can set CLOEXEC atomically; macOS needs fcntl on the received descriptor.
    let flags = if cfg!(target_os = "linux") {
        0x40000000
    } else {
        0
    };
    // These C layouts match msghdr/iovec/cmsghdr on the supported Linux/macOS targets.
    // A child exiting (SIGCHLD) can interrupt the wait; retry instead of failing the session.
    let count = loop {
        let count = unsafe { recvmsg(socket.as_raw_fd(), &mut message, flags) };
        if count >= 0 {
            break count;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    if rights.level == SOCKET_LEVEL && rights.kind == 1 && rights.fd >= 0 {
        // The private socket has exactly one trusted sender and one SCM_RIGHTS fd.
        let file = unsafe { File::from_raw_fd(rights.fd) };
        if unsafe { fcntl(file.as_raw_fd(), 2, 1) } < 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(file);
    }
    let mut error = bytes[..count as usize].to_vec();
    socket.take(4096).read_to_end(&mut error)?;
    let error: serde_json::Value = serde_json::from_slice(&error)
        .map_err(|_| io::Error::other("agent file opener returned no file"))?;
    Err(error["errno"]
        .as_i64()
        .map(|code| io::Error::from_raw_os_error(code as i32))
        .unwrap_or_else(|| io::Error::other("agent file path is invalid")))
}

#[cfg(test)]
mod tests {
    use super::complete_line;
    use std::io::Cursor;

    #[test]
    fn partial_multibyte_record_waits_for_its_newline() {
        let mut reader = Cursor::new(b"{\"a\":\"\xC3".to_vec());
        assert!(
            complete_line(&mut reader, 64, "too large")
                .unwrap()
                .is_none()
        );

        let mut reader =
            Cursor::new("{\"a\":\"\u{e9}\"}\n{\"b\":\"\u{e9}".as_bytes()[..17].to_vec());
        let (n, line) = complete_line(&mut reader, 64, "too large")
            .unwrap()
            .unwrap();
        assert_eq!((n, line.as_str()), (11, "{\"a\":\"\u{e9}\"}\n"));
        assert!(
            complete_line(&mut reader, 64, "too large")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn empty_oversized_and_invalid_records() {
        assert!(
            complete_line(&mut Cursor::new(Vec::new()), 64, "too large")
                .unwrap()
                .is_none()
        );
        let error = complete_line(&mut Cursor::new(vec![b'x'; 65]), 64, "too large").unwrap_err();
        assert_eq!(error.to_string(), "too large");
        let error =
            complete_line(&mut Cursor::new(b"\xC3\n".to_vec()), 64, "too large").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
