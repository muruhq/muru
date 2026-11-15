//! The signals a worker takes before its server serves (issue 673).
//!
//! The server's own handling of SIGTERM, SIGINT, SIGUSR1 and SIGUSR2
//! starts with tokio's, registered when it starts serving. Between the
//! fork and then the worker imports the application, runs its hooks and
//! starts lifespan, and a signal that arrives then must not be lost, nor
//! kill the worker with the default action. Python's handlers take them
//! first, a stop raising to unwind the start; [`claim`] registers a flag
//! for each right after, chained over Python's, and [`replay`] delivers
//! again whatever arrived before the server's own handling was there.

use std::io;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// The signals claimed, in the order of [`SEEN`].
const SIGNALS: [libc::c_int; 4] = [libc::SIGTERM, libc::SIGINT, libc::SIGUSR1, libc::SIGUSR2];

/// Whether each of [`SIGNALS`] has arrived since it was claimed.
static SEEN: [AtomicBool; 4] = [const { AtomicBool::new(false) }; 4];

static CLAIMED: OnceLock<()> = OnceLock::new();

/// Note each signal as it arrives, on top of the handler in place, which
/// still runs. Once per process.
pub fn claim() -> io::Result<()> {
    if CLAIMED.get().is_some() {
        return Ok(());
    }

    for (index, &signal) in SIGNALS.iter().enumerate() {
        // SAFETY: the action only stores to an atomic, which is
        // async-signal-safe.

        unsafe {
            signal_hook_registry::register(signal, move || {
                SEEN[index].store(true, Ordering::SeqCst);
            })?;
        }
    }

    let _ = CLAIMED.set(());
    Ok(())
}

/// Send this process again each signal that arrived before the server's
/// own handling was registered, so the server acts on it. Called once the
/// server's handlers are in place. A signal that arrived just after them
/// as well is sent twice, which the server takes as once: a stop ends its
/// loop, and a second reopen or reclaim does no harm.
pub fn replay() {
    for (index, &signal) in SIGNALS.iter().enumerate() {
        if SEEN[index].swap(false, Ordering::SeqCst) {
            // SAFETY: kill with this process's own pid and a valid signal.

            unsafe {
                libc::kill(libc::getpid(), signal);
            }
        }
    }
}
