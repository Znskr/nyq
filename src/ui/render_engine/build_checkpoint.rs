#![allow(non_snake_case)]
use std::ffi::c_void;
const WORK_QUOTA: usize = 4096;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputPending;
pub struct BuildCheckpoint {
    pub context: *mut c_void,
    pub input_pending: fn(*mut c_void) -> bool,
    pub remaining: usize,
}
impl BuildCheckpoint {
    pub fn init(context: *mut c_void, input_pending: fn(*mut c_void) -> bool) -> Self {
        Self {
            context,
            input_pending,
            remaining: WORK_QUOTA,
        }
    }
    fn tick(&mut self) -> Result<(), InputPending> {
        self.consume(1)
    }
    fn consume(&mut self, units: usize) -> Result<(), InputPending> {
        if units < self.remaining {
            self.remaining -= units;
            return Ok(());
        }
        let overflow = units - self.remaining;
        let used = overflow % WORK_QUOTA;
        self.remaining = if used == 0 {
            WORK_QUOTA
        } else {
            WORK_QUOTA - used
        };
        self.poll()
    }
    fn poll(&mut self) -> Result<(), InputPending> {
        if (self.input_pending)(self.context) {
            Err(InputPending)
        } else {
            Ok(())
        }
    }
}
pub fn tick(checkpoint: Option<&mut BuildCheckpoint>) -> Result<(), InputPending> {
    match checkpoint {
        Some(c) => c.tick(),
        None => Ok(()),
    }
}
pub fn consume(checkpoint: Option<&mut BuildCheckpoint>, units: usize) -> Result<(), InputPending> {
    match checkpoint {
        Some(c) => c.consume(units),
        None => Ok(()),
    }
}
pub fn poll(checkpoint: Option<&mut BuildCheckpoint>) -> Result<(), InputPending> {
    match checkpoint {
        Some(c) => c.poll(),
        None => Ok(()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static POLLS: AtomicUsize = AtomicUsize::new(0);
    fn pending(_: *mut c_void) -> bool {
        POLLS.fetch_add(1, Ordering::SeqCst);
        false
    }
    #[test]
    fn quota_polling() {
        POLLS.store(0, Ordering::SeqCst);
        let mut c = BuildCheckpoint::init(std::ptr::null_mut(), pending);
        consume(Some(&mut c), WORK_QUOTA - 1).unwrap();
        assert_eq!(POLLS.load(Ordering::SeqCst), 0);
        tick(Some(&mut c)).unwrap();
        assert_eq!(POLLS.load(Ordering::SeqCst), 1);
        consume(Some(&mut c), WORK_QUOTA * 2 + 7).unwrap();
        assert_eq!(POLLS.load(Ordering::SeqCst), 2);
        consume(Some(&mut c), WORK_QUOTA - 8).unwrap();
        assert_eq!(POLLS.load(Ordering::SeqCst), 2);
        tick(Some(&mut c)).unwrap();
        assert_eq!(POLLS.load(Ordering::SeqCst), 3);
        consume(None, WORK_QUOTA).unwrap();
    }
}
