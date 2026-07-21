use anyhow::{anyhow, Context, Result};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::{Handle as SignalHandle, Signals};
use std::fmt;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

static INTERRUPT_COUNT: AtomicUsize = AtomicUsize::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
struct InterruptedError;

impl fmt::Display for InterruptedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "interrupted")
    }
}

impl std::error::Error for InterruptedError {}

pub(crate) struct InterruptGuard {
    handle: SignalHandle,
    thread: Option<JoinHandle<()>>,
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
        INTERRUPT_COUNT.store(0, Ordering::SeqCst);
        ARMED.store(false, Ordering::SeqCst);
    }
}

pub(crate) fn arm_interrupts() -> Result<InterruptGuard> {
    if ARMED.swap(true, Ordering::SeqCst) {
        return Err(anyhow!("interrupt handlers are already armed"));
    }
    INTERRUPT_COUNT.store(0, Ordering::SeqCst);
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);

    let mut signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(signals) => signals,
        Err(err) => {
            ARMED.store(false, Ordering::SeqCst);
            return Err(err).context("failed to register interrupt handlers");
        }
    };
    let handle = signals.handle();
    let thread = std::thread::Builder::new()
        .name("botforge-signal-listener".to_string())
        .spawn(move || {
            for signal in signals.forever() {
                if matches!(signal, SIGINT | SIGTERM) {
                    INTERRUPT_COUNT.fetch_add(1, Ordering::SeqCst);
                }
                if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                    break;
                }
            }
        })
        .context("failed to spawn signal-listener thread")?;

    Ok(InterruptGuard {
        handle,
        thread: Some(thread),
    })
}

pub(crate) fn interrupt_count() -> usize {
    INTERRUPT_COUNT.load(Ordering::SeqCst)
}

pub(crate) fn is_interrupted() -> bool {
    interrupt_count() >= 1
}

pub(crate) fn should_force_exit() -> bool {
    interrupt_count() >= 2
}

pub(crate) fn maybe_hard_exit() {
    if should_force_exit() {
        eprintln!("second interrupt received — exiting immediately");
        std::process::exit(130);
    }
}

pub(crate) fn interrupted_error() -> anyhow::Error {
    anyhow::Error::new(InterruptedError)
}

pub(crate) fn poll_interrupt() -> Result<()> {
    maybe_hard_exit();
    if is_interrupted() {
        return Err(interrupted_error());
    }
    Ok(())
}

pub(crate) fn kill_child(child: &mut Child) {
    let _ = child.kill();
    loop {
        maybe_hard_exit();
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        arm_interrupts, interrupt_count, is_interrupted, poll_interrupt, should_force_exit, ARMED,
        INTERRUPT_COUNT,
    };
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn set_interrupts(value: usize) {
        INTERRUPT_COUNT.store(value, Ordering::SeqCst);
    }

    #[test]
    fn arm_and_drop_reset_interrupt_state() {
        let _lock = TEST_LOCK.lock().unwrap();
        set_interrupts(0);
        ARMED.store(false, Ordering::SeqCst);
        {
            let _guard = arm_interrupts().unwrap();
            assert!(!is_interrupted());
            set_interrupts(1);
            assert!(is_interrupted());
        }
        assert_eq!(interrupt_count(), 0);
        assert!(!is_interrupted());
        assert!(!ARMED.load(Ordering::SeqCst));
    }

    #[test]
    fn poll_interrupt_returns_error_after_first_interrupt() {
        let _lock = TEST_LOCK.lock().unwrap();
        set_interrupts(1);
        let err = poll_interrupt().unwrap_err();
        assert!(format!("{err:#}").contains("interrupted"));
    }

    #[test]
    fn second_interrupt_count_enables_hard_exit_path() {
        let _lock = TEST_LOCK.lock().unwrap();
        set_interrupts(2);
        assert!(should_force_exit());
    }
}
