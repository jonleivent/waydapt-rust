#![forbid(unsafe_code)]

use crate::multithread_exit::{multithread_exit, ExitCode};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::{panicking, sleep};
use std::time::Duration;

// When a client session ends, and we are running in multi-threaded mode (sessions are threads,
// not child processes), and the terminate command-line option was used (it's only allowed in
// multi-threaded mode), we want the waydapt process to exit when the last session ends and
// there are no new sessions before some timeout (the value of the terminate option, in
// seconds).  To do this, we use 2 atomic counters.  CLIENT_SESSION_TOTAL only ever increments -
// it counts the number of client sessions that were started.  CLIENT_SESSION_COUNT both
// incremenets and decrements - it counts the number of currently existing client sessions.  We
// then test these at client session end (determined by SessionTerminator::drop).  If a session
// exits and the CLIENT_SESSION_COUNT goes to 0 (fetch_sub returns the previous value, so when
// that goes to 1), the session queries and caches CLIENT_SESSION_TOTAL, sleeps for the desired
// number of seconds, and then queries CLIENT_SESSION_TOTAL again.  If the two values are the
// same, it uses multithread_exit to end the process and run destructors.

static CLIENT_SESSION_COUNT: AtomicU32 = AtomicU32::new(0);
static CLIENT_SESSION_TOTAL: AtomicU32 = AtomicU32::new(0);

pub(crate) struct SessionTerminator {
    timeout: Duration,
}

impl SessionTerminator {
    pub(crate) fn new(terminate_after: Duration) -> Self {
        CLIENT_SESSION_COUNT.fetch_add(1, Ordering::Acquire);
        CLIENT_SESSION_TOTAL.fetch_add(1, Ordering::Relaxed);
        Self { timeout: terminate_after }
    }
}

impl Drop for SessionTerminator {
    fn drop(&mut self) {
        // fetch_sub returns the value prior to the subtract, so test against 1 intstead of 0:
        if CLIENT_SESSION_COUNT.fetch_sub(1, Ordering::Release) == 1 {
            // We are the last remaining session, but maybe there will be others before timeout...
            let saved_client_session_total = CLIENT_SESSION_TOTAL.load(Ordering::Relaxed);
            // All we care about is if CLIENT_SESSION_TOTAL got incremented while we were
            // sleeping, hence we can use Relaxed because we aren't trying to maintain the
            // order of these two loads with respect to anything except operations on
            // CLIENT_SESSION_TOTAL.
            sleep(self.timeout);
            if saved_client_session_total == CLIENT_SESSION_TOTAL.load(Ordering::Relaxed) {
                // No new sessions since we decided above we were last, so exit.
                let exit_code = if panicking() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
                multithread_exit(exit_code);
            }
        }
    }
}
