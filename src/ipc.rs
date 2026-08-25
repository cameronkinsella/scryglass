//! Single-instance coordination over a per-user local socket (a named pipe on
//! Windows, a Unix-domain / namespaced socket elsewhere). A second launch hands
//! its file argument to the running process and exits, so every file opens as a
//! new window under one process. Limitation: only the argv path is forwarded.
//! macOS delivers a Finder open into a running app as an Apple Event, unseen by
//! iced/winit 0.14, so until a handler exists only CLI and `open -a` work there.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{GenericNamespaced, Listener, ListenerOptions, Stream, ToNsName};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Launches forwarded by later instances (a path to open, or None for a bare
/// relaunch), set by [`establish`] on the primary and drained by the subscription.
static FORWARDS: OnceLock<Mutex<Option<UnboundedReceiver<Option<PathBuf>>>>> = OnceLock::new();

/// Per-user socket name so separate users (and RDP sessions) don't collide.
fn socket_name() -> String {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    format!("scryglass-{user}.sock")
}

/// Largest accepted forward payload. A path is tiny even at the Windows
/// extended-length limit, and the cap keeps a hostile peer from ballooning
/// the read buffer.
const MAX_PAYLOAD: u64 = 64 * 1024;

/// How long a forwarding launch waits on the primary before giving up and
/// running standalone.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(1);

/// Encode a path losslessly for the wire: UTF-16 code units (little endian)
/// on Windows, raw bytes elsewhere. A string round-trip would mangle the
/// unusual but legal names (unpaired surrogates, non-UTF-8 bytes).
#[cfg(windows)]
fn encode_path(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(windows)]
fn decode_path(bytes: &[u8]) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let wide: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    std::ffi::OsString::from_wide(&wide).into()
}

#[cfg(unix)]
fn encode_path(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(unix)]
fn decode_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(bytes).into()
}

/// What this process should do after coordinating with any running instance.
pub enum Role {
    /// This is the sole instance: run the app. Forwarded opens arrive via the
    /// receiver stashed for [`take_forwards`].
    Primary,
    /// Another instance is running and has been handed the path. Exit quietly.
    Forwarded,
}

/// Forward the launch path to a running instance, or claim the role of primary.
/// On the primary, spawns the accept loop that feeds forwarded paths through.
pub fn establish(initial_path: Option<&Path>) -> Role {
    let name = socket_name();
    // Resolve against this launch's working directory before forwarding.
    // The primary's differs, so a relative path would open the wrong file
    // there.
    let payload = initial_path
        .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))
        .map(|p| encode_path(&p))
        .unwrap_or_default();
    let mut attempts = 0;
    loop {
        // Cap the handoff dance so a socket that stays claimed but keeps
        // failing cannot spin forever. Standalone still opens the file, it
        // only gives up on receiving future forwards.
        attempts += 1;
        if attempts > 3 {
            return Role::Primary;
        }
        // A running instance answers the socket: hand it the path and exit.
        // The write failing (the listener died between accept and read) falls
        // through to claim primary rather than dropping the open.
        if try_forward(&name, payload.clone()) {
            return Role::Forwarded;
        }

        // Nobody listening: try to become the primary by binding the socket.
        let Ok(addr) = name.as_str().to_ns_name::<GenericNamespaced>() else {
            return Role::Primary;
        };
        match ListenerOptions::new().name(addr).create_sync() {
            Ok(listener) => {
                spawn_accept_loop(listener);
                return Role::Primary;
            }
            // Lost the bind race: loop back and forward to whoever won.
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            // Any other bind failure: run standalone, no forwarding.
            Err(_) => return Role::Primary,
        }
    }
}

/// Connect to a running primary and hand it the payload, bounded by
/// [`FORWARD_TIMEOUT`]. The named-pipe connect waits unboundedly while a
/// wedged primary holds the socket, which would leave this launch hanging
/// with no window. Timing out degrades to standalone instead.
fn try_forward(name: &str, payload: Vec<u8>) -> bool {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let socket = name.to_owned();
    std::thread::spawn(move || {
        let delivered = socket
            .as_str()
            .to_ns_name::<GenericNamespaced>()
            .ok()
            .and_then(|addr| Stream::connect(addr).ok())
            .is_some_and(|mut stream| stream.write_all(&payload).is_ok());
        let _ = done_tx.send(delivered);
    });
    done_rx
        .recv_timeout(FORWARD_TIMEOUT)
        .is_ok_and(|delivered| delivered)
}

/// The accept loop's reaction to a run of consecutive `accept()` errors. A
/// healthy accept resets the streak, so this only ever sees it growing.
enum AcceptRetry {
    /// Pause this long, then try again.
    Backoff(Duration),
    /// Too many failures in a row: stop the accept thread.
    GiveUp,
}

/// Decide how to react after `failures` consecutive `accept()` errors. Mirrors
/// [`establish`]'s bounded retry: back off briefly instead of spinning a core,
/// and give up once a broken listener keeps failing.
fn accept_retry(failures: u32) -> AcceptRetry {
    const MAX_ACCEPT_FAILURES: u32 = 5;
    if failures >= MAX_ACCEPT_FAILURES {
        AcceptRetry::GiveUp
    } else {
        // Grow the pause with the streak so a lone blip barely delays a real
        // connection while a stuck listener winds down toward give-up.
        AcceptRetry::Backoff(Duration::from_millis(50 * u64::from(failures)))
    }
}

/// One blocking thread owns the listener: each connection carries a single path.
fn spawn_accept_loop(listener: Listener) {
    let (tx, rx) = unbounded_channel();
    let _ = FORWARDS.set(Mutex::new(Some(rx)));
    std::thread::spawn(move || {
        // Consecutive accept() failures. A success resets it; a stuck listener
        // backs off and eventually gives up instead of spinning a full core.
        let mut failures: u32 = 0;
        loop {
            if tx.is_closed() {
                return; // the app is gone
            }
            let stream = match listener.accept() {
                Ok(stream) => {
                    failures = 0;
                    stream
                }
                Err(_) => {
                    failures += 1;
                    match accept_retry(failures) {
                        AcceptRetry::Backoff(delay) => {
                            std::thread::sleep(delay);
                            continue;
                        }
                        AcceptRetry::GiveUp => return,
                    }
                }
            };
            // Read on a throwaway thread: the read blocks until the client
            // closes its end, and one stalled client must not wedge the
            // accept loop (and with it every future forward).
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut payload = Vec::new();
                if stream.take(MAX_PAYLOAD).read_to_end(&mut payload).is_ok() {
                    // A bare relaunch sends an empty payload. Forward it as
                    // None so the primary still opens an (empty) window.
                    let forwarded = (!payload.is_empty()).then(|| decode_path(&payload));
                    let _ = tx.send(forwarded);
                }
            });
        }
    });
}

/// Take the receiver of forwarded launches. Returns `Some` once on the primary.
pub fn take_forwards() -> Option<UnboundedReceiver<Option<PathBuf>>> {
    FORWARDS
        .get()
        .and_then(|m| m.lock().ok().and_then(|mut g| g.take()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_encoding_round_trips_unusual_names() {
        // Trailing whitespace and non-ASCII both survive. The old string
        // round-trip trimmed one and could mangle the other.
        let path = Path::new("aria/más allá 🦀 .png");
        assert_eq!(decode_path(&encode_path(path)), path);
        assert!(encode_path(Path::new("")).is_empty());
    }

    #[test]
    fn accept_backs_off_with_the_streak_then_gives_up() {
        // A short streak backs off, and the pause grows with each failure so a
        // broken listener cannot spin a core.
        let (first, second) = match (accept_retry(1), accept_retry(2)) {
            (AcceptRetry::Backoff(a), AcceptRetry::Backoff(b)) => (a, b),
            _ => panic!("early failures should back off, not give up"),
        };
        assert!(second > first, "backoff grows with the streak");
        // Enough consecutive failures stops the accept thread.
        assert!(matches!(accept_retry(5), AcceptRetry::GiveUp));
        assert!(matches!(accept_retry(50), AcceptRetry::GiveUp));
    }
}
