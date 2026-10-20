//! The gate's side of the audit-runner connection, with reverse authentication
//! (plans/M2-design.md §5).
//!
//! Through a socket that systemd created and handed to the runner, the client's `SO_PEERCRED`
//! names the systemd manager, not the runner (plans/M2-evidence/peercred-probe.txt). So the gate
//! sets `SO_PASSCRED` and reads the whole reply with `recvmsg`: the kernel attaches the sender's
//! real pid and uid to every piece of data. Every piece must carry credentials, all the same;
//! the uid must be the runner's, the pid must still be running, in the runner's service cgroup.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Who the reply must come from.
#[derive(Debug, Clone)]
pub struct Expected {
    pub uid: u32,
    /// cgroup v2 path, e.g. `/system.slice/audit-runner.service`.
    pub cgroup: String,
}

/// A reply and the kernel-attested identity of its sender.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub line: String,
    pub pid: i32,
    pub uid: u32,
    pub cgroup: String,
}

/// The longest reply accepted (a full Codex answer plus its record fits easily).
const MAX_REPLY: usize = 8 * 1024 * 1024;

/// Sends one request line and reads one authenticated reply line.
pub fn call(socket: &Path, request: &str, expect: &Expected, timeout: Duration) -> Result<Reply> {
    let s = send(socket, request, timeout)?;
    receive(&s, expect)
}

/// The sending half of [`call`]: connects, asks for the sender's credentials on everything it
/// will receive, and writes the request line. Once this returns, the job has been sent; read the
/// reply with [`receive`].
pub fn send(socket: &Path, request: &str, timeout: Duration) -> Result<UnixStream> {
    let mut s = UnixStream::connect(socket)
        .with_context(|| format!("connecting to {}", socket.display()))?;
    passcred(&s)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    s.write_all(request.as_bytes())?;
    s.write_all(b"\n")?;
    Ok(s)
}

fn passcred(s: &UnixStream) -> Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: setsockopt on an open socket with a correctly sized int option.
    let rc = unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PASSCRED,
            (&on as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        bail!("SO_PASSCRED: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// One `recvmsg`: the data read and the credentials attached to it (None if none came).
fn recv_piece(s: &UnixStream, buf: &mut [u8]) -> Result<(usize, Option<libc::ucred>)> {
    // Room for one SCM_CREDENTIALS message.
    let mut cmsg = [0u64; 8];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: msghdr is plain data; the pointers below stay valid for the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&cmsg) as _;
    // SAFETY: recvmsg on an open socket into buffers that outlive the call.
    let n = unsafe { libc::recvmsg(s.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        bail!(
            "reading the runner's reply: {}",
            std::io::Error::last_os_error()
        );
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        bail!("the credentials of the runner's reply were truncated");
    }
    let mut cred = None;
    let mut passed_fds = 0;
    // SAFETY: walking the control buffer recvmsg filled, with the libc helpers.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_CREDENTIALS {
                cred = Some(std::ptr::read_unaligned(
                    libc::CMSG_DATA(c).cast::<libc::ucred>(),
                ));
            }
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                // Descriptors the sender passed: never kept.
                let len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let fds = libc::CMSG_DATA(c).cast::<libc::c_int>();
                for i in 0..len / std::mem::size_of::<libc::c_int>() {
                    libc::close(std::ptr::read_unaligned(fds.add(i)));
                    passed_fds += 1;
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if passed_fds > 0 {
        bail!("the runner's reply carried {passed_fds} file descriptor(s): refused");
    }
    Ok((n as usize, cred))
}

/// Reads one reply line; every piece must carry the same credentials.
/// Tests: run once the whole reply is in and the uid checked, before the sender's liveness
/// check, for a reply from this pid only (so tests running in parallel never meet). E3 uses it
/// to make sure the sender has exited and been reaped by then.
#[cfg(test)]
pub(crate) type LivenessHook = (i32, std::sync::Arc<dyn Fn() + Send + std::marker::Sync>);

#[cfg(test)]
pub(crate) static BEFORE_LIVENESS: std::sync::Mutex<Vec<LivenessHook>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn before_liveness(pid: i32) {
    let hook = BEFORE_LIVENESS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .find(|(p, _)| *p == pid)
        .map(|(_, h)| h.clone());
    if let Some(h) = hook {
        h();
    }
}

pub fn receive(s: &UnixStream, expect: &Expected) -> Result<Reply> {
    let mut data = Vec::new();
    let mut who: Option<libc::ucred> = None;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let (n, cred) = recv_piece(s, &mut buf)?;
        if n == 0 {
            bail!("the runner closed the connection before a whole reply line");
        }
        let cred = cred.context("a piece of the reply came without credentials")?;
        match &who {
            None => who = Some(cred),
            Some(w) if (w.pid, w.uid) != (cred.pid, cred.uid) => {
                bail!(
                    "the reply came from two senders (pid {} uid {}, then pid {} uid {})",
                    w.pid,
                    w.uid,
                    cred.pid,
                    cred.uid
                );
            }
            Some(_) => {}
        }
        data.extend_from_slice(&buf[..n]);
        if data.len() > MAX_REPLY {
            bail!("reply over {MAX_REPLY} bytes");
        }
        if let Some(end) = data.iter().position(|b| *b == b'\n') {
            data.truncate(end);
            break;
        }
    }
    let w = who.expect("at least one piece was read");
    if w.uid != expect.uid {
        bail!(
            "the reply came from uid {}, not the runner's {}",
            w.uid,
            expect.uid
        );
    }
    #[cfg(test)]
    before_liveness(w.pid);
    // The pid is checked once the whole reply is in: a sender that is gone by then (exited, or
    // its pid reused) is not taken on trust.
    let cgroup = cgroup_of(w.pid).with_context(|| {
        format!(
            "sender pid {} is no longer running: not authenticated",
            w.pid
        )
    })?;
    if cgroup != expect.cgroup {
        bail!(
            "the reply came from cgroup {cgroup}, not the runner's {}",
            expect.cgroup
        );
    }
    Ok(Reply {
        line: String::from_utf8(data).context("reply is not UTF-8")?,
        pid: w.pid,
        uid: w.uid,
        cgroup,
    })
}

/// The cgroup v2 path of a running process.
pub fn cgroup_of(pid: i32) -> Result<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::to_owned)
        .context("no cgroup v2 entry")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::UnixListener;

    fn me() -> Expected {
        Expected {
            uid: std::fs::metadata("/proc/self").unwrap().uid(),
            cgroup: cgroup_of(std::process::id() as i32).unwrap(),
        }
    }

    /// A runner stand-in in this process: replies to one request with `reply`, in pieces.
    fn serve_once(dir: &Path, pieces: Vec<&'static str>) -> std::path::PathBuf {
        let path = dir.join("r.sock");
        let l = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut line = String::new();
            BufReader::new(c.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            for p in pieces {
                c.write_all(p.as_bytes()).unwrap();
                std::thread::sleep(Duration::from_millis(30));
            }
        });
        path
    }

    #[test]
    fn a_reply_from_the_expected_sender_is_authenticated_across_pieces() {
        let d = tempfile::tempdir().unwrap();
        let sock = serve_once(d.path(), vec!["{\"status\":", "\"completed\"}", "\n"]);
        let r = call(&sock, "{}", &me(), Duration::from_secs(5)).unwrap();
        assert_eq!(r.line, "{\"status\":\"completed\"}");
        assert_eq!(r.pid, std::process::id() as i32);
    }

    #[test]
    fn the_wrong_uid_or_cgroup_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let sock = serve_once(d.path(), vec!["ok\n"]);
        let mut e = me();
        e.uid += 1;
        let err = call(&sock, "{}", &e, Duration::from_secs(5))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not the runner's"), "{err}");

        let d = tempfile::tempdir().unwrap();
        let sock = serve_once(d.path(), vec!["ok\n"]);
        let mut e = me();
        e.cgroup = "/system.slice/audit-runner.service".into();
        let err = call(&sock, "{}", &e, Duration::from_secs(5))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cgroup"), "{err}");
    }

    /// Waits until the python stand-in is listening: it prints "ready" after listen(). The socket
    /// file appears at bind(), before listen(), so waiting for the file alone races (a connect in
    /// between is refused).
    fn listening(child: &mut std::process::Child) {
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().expect("piped stdout")),
            &mut line,
        )
        .unwrap();
        assert_eq!(line.trim(), "ready", "the stand-in did not start listening");
    }

    #[test]
    fn a_sender_that_is_gone_when_the_reply_is_in_is_not_authenticated() {
        // The reply comes from a separate process that exits right after sending.
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("r.sock");
        let mut child = std::process::Command::new("python3")
            .arg("-c")
            .arg(
                "import socket,sys\n\
                 s=socket.socket(socket.AF_UNIX);s.bind(sys.argv[1]);s.listen(1);print('ready',flush=True)\n\
                 c,_=s.accept();c.recv(100);c.sendall(b'ok\\n')\n",
            )
            .arg(&sock)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        listening(&mut child);
        let mut s = UnixStream::connect(&sock).unwrap();
        passcred(&s).unwrap();
        s.write_all(b"{}\n").unwrap();
        child.wait().unwrap(); // the sender has exited (and been reaped) before we read
        let err = receive(&s, &me()).unwrap_err().to_string();
        assert!(err.contains("no longer running"), "{err}");
    }

    #[test]
    fn a_reply_written_by_two_processes_is_refused() {
        // The first piece from the server, the rest from a forked child (another pid).
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("r.sock");
        let mut child = std::process::Command::new("python3")
            .arg("-c")
            .arg(
                "import socket,sys,os,time\n\
                 s=socket.socket(socket.AF_UNIX);s.bind(sys.argv[1]);s.listen(1);print('ready',flush=True)\n\
                 c,_=s.accept();c.recv(100);c.sendall(b'{\"a\":');time.sleep(0.2)\n\
                 pid=os.fork()\n\
                 if pid==0:\n c.sendall(b'1}\\n');time.sleep(1);os._exit(0)\n\
                 time.sleep(1.5)\n",
            )
            .arg(&sock)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        listening(&mut child);
        let err = call(&sock, "{}", &me(), Duration::from_secs(5))
            .unwrap_err()
            .to_string();
        child.wait().unwrap();
        assert!(err.contains("two senders"), "{err}");
    }

    #[test]
    fn a_reply_whose_control_data_does_not_fit_is_refused() {
        // A sender that also passes file descriptors overflows the credentials buffer.
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("r.sock");
        let mut child = std::process::Command::new("python3")
            .arg("-c")
            .arg(
                "import socket,sys,array,os\n\
                 s=socket.socket(socket.AF_UNIX);s.bind(sys.argv[1]);s.listen(1);print('ready',flush=True)\n\
                 c,_=s.accept();c.recv(100)\n\
                 fds=array.array('i',[os.open('/dev/null',os.O_RDONLY) for _ in range(12)])\n\
                 c.sendmsg([b'ok\\n'],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,fds)])\n\
                 c.recv(1)\n",
            )
            .arg(&sock)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        listening(&mut child);
        let res = call(&sock, "{}", &me(), Duration::from_secs(5));
        let _ = child.kill();
        child.wait().unwrap();
        let err = res.unwrap_err().to_string();
        assert!(err.contains("truncated"), "{err}");
    }

    /// Counts this process's open descriptors, so it runs alone in a child process: tests in
    /// other threads open and close descriptors all the time (git, pipes, temp files).
    #[test]
    fn passed_file_descriptors_are_closed_and_the_reply_refused() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "milestone::runner::tests::passed_file_descriptors_alone",
                "--ignored",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && text.contains("1 passed"),
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    #[ignore = "run alone by passed_file_descriptors_are_closed_and_the_reply_refused"]
    fn passed_file_descriptors_alone() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("r.sock");
        let mut child = std::process::Command::new("python3")
            .arg("-c")
            .arg(
                "import socket,sys,array,os\n\
                 s=socket.socket(socket.AF_UNIX);s.bind(sys.argv[1]);s.listen(1);print('ready',flush=True)\n\
                 c,_=s.accept();c.recv(100)\n\
                 fds=array.array('i',[os.open('/dev/null',os.O_RDONLY)])\n\
                 c.sendmsg([b'ok\\n'],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,fds)])\n\
                 c.recv(1)\n",
            )
            .arg(&sock)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        listening(&mut child);
        let open = || std::fs::read_dir("/proc/self/fd").unwrap().count();
        let before = open();
        let res = call(&sock, "{}", &me(), Duration::from_secs(5));
        let _ = child.kill();
        child.wait().unwrap();
        let err = res.unwrap_err().to_string();
        assert!(err.contains("file descriptor"), "{err}");
        assert!(open() <= before, "a passed descriptor was kept open");
    }

    #[test]
    fn a_closed_connection_without_a_whole_line_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let sock = serve_once(d.path(), vec!["partial"]);
        let err = call(&sock, "{}", &me(), Duration::from_secs(5))
            .unwrap_err()
            .to_string();
        assert!(err.contains("closed the connection"), "{err}");
    }

    #[test]
    fn a_reply_without_credentials_is_refused() {
        // Without SO_PASSCRED the kernel attaches no credentials: receive must refuse.
        let d = tempfile::tempdir().unwrap();
        let sock = serve_once(d.path(), vec!["ok\n"]);
        let mut s = UnixStream::connect(&sock).unwrap();
        s.write_all(b"{}\n").unwrap();
        let err = receive(&s, &me()).unwrap_err().to_string();
        assert!(err.contains("without credentials"), "{err}");
    }
}
