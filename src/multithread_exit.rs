#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::{mpsc::sync_channel, mpsc::SyncSender, OnceLock};

static TX_TO_MAIN: OnceLock<SyncSender<ExitCode>> = OnceLock::new();

fn sleep_forever() -> ! {
    use std::thread::sleep;
    use std::time::Duration;
    loop {
        sleep(Duration::from_secs(!0)); // infinite sleep, waiting for the process to exit
    }
}

pub(crate) fn multithread_exit(exit_code: ExitCode) -> ! {
    if let Some(tx) = TX_TO_MAIN.get() {
        if tx.try_send(exit_code).is_ok() {
            sleep_forever();
        }
    }
    panic!("multithread_exit failed trying to propagate value: {exit_code:?}");
}

// TBD: this should only be called in the main thread - do we want to test for that?  If so, we
// can use the in_main_thread crate.
pub(crate) fn multithread_exit_handler() -> ExitCode {
    let (tx, rx) = sync_channel(1);
    TX_TO_MAIN.set(tx).unwrap();
    rx.recv().unwrap()
}
