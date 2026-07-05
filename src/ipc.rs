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
        if let Ok(addr) = name.as_str().to_ns_name::<GenericNamespaced>()
            && let Ok(mut stream) = Stream::connect(addr)
        {
            let payload = initial_path
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            if stream.write_all(payload.as_bytes()).is_ok() {
                return Role::Forwarded;
            }
            // The write failed: the listener died between accept and read.
            // Fall through and claim primary rather than dropping the open.
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
            let mut stream = match listener.accept() {
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
            let mut payload = String::new();
            if stream.read_to_string(&mut payload).is_ok() {
                let trimmed = payload.trim();
                // A bare relaunch sends an empty payload. Forward it as None so
                // the primary still opens an (empty) window.
                let forwarded = (!trimmed.is_empty()).then(|| PathBuf::from(trimmed));
                if tx.send(forwarded).is_err() {
                    return; // the app is gone
                }
            }
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
