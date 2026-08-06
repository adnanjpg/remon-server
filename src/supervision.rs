//! Panic supervision for the long-lived background loops.
//!
//! The release profile unwinds, so a panicking loop takes only its own task:
//! collection, alerting or persistence stops while the daemon keeps answering.
//! Ending the process hands the restart to whatever supervises the service.

use log::error;
use tokio::task::JoinHandle;

/// Non-zero on purpose: a Windows scheduled task only restarts an action that
/// failed, while systemd restarts on any exit.
const EXIT_TASK_PANIC: i32 = 70;

/// Exit the process if the loop behind `handle` panics.
pub fn supervise(name: &'static str, handle: JoinHandle<()>) {
    tokio::spawn(async move {
        match handle.await {
            Ok(()) => {}
            Err(e) if e.is_panic() => {
                error!("{name} panicked; exiting so the service restarts");
                std::process::exit(EXIT_TASK_PANIC);
            }
            Err(e) => error!("{name} ended unexpectedly: {e}"),
        }
    });
}
