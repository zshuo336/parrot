//! One node ledger, retained through queueing, buffering and actual delivery.
use std::sync::{Arc, Mutex};

use crate::frame::Frame;

#[derive(Debug)]
pub(super) struct Budget {
    limits: Usage,
    used: Mutex<Usage>,
}

#[derive(Debug, Default)]
struct Usage {
    streams: usize,
    frames: usize,
    bytes: usize,
}

#[derive(Debug)]
enum Charge {
    Stream,
    Frame(usize),
}

#[derive(Debug)]
pub(super) struct Permit {
    budget: Arc<Budget>,
    charge: Charge,
}

#[derive(Debug)]
pub(super) struct Pending {
    pub frame: Frame,
    pub permit: Permit,
}

impl Budget {
    pub fn new(streams: usize, frames: usize, bytes: usize) -> Option<Arc<Self>> {
        let max = (usize::MAX >> 3).min(u32::MAX as usize);
        if [streams, frames, bytes]
            .iter()
            .any(|&value| value == 0 || value > max)
        {
            return None;
        }
        Some(Arc::new(Self {
            limits: Usage {
                streams,
                frames,
                bytes,
            },
            used: Mutex::new(Usage::default()),
        }))
    }

    pub fn is_empty(&self) -> bool {
        let used = self.used.lock().unwrap();
        used.streams == 0 && used.frames == 0 && used.bytes == 0
    }

    #[cfg(test)]
    pub fn usage(&self) -> (usize, usize, usize) {
        let used = self.used.lock().unwrap();
        (used.streams, used.frames, used.bytes)
    }

    pub fn stream(self: &Arc<Self>) -> Option<Permit> {
        let mut used = self.used.lock().unwrap();
        if used.streams == self.limits.streams {
            return None;
        }
        used.streams += 1;
        Some(Permit {
            budget: self.clone(),
            charge: Charge::Stream,
        })
    }

    pub fn frame(self: &Arc<Self>, bytes: usize) -> Option<Permit> {
        let mut used = self.used.lock().unwrap();
        if used.frames == self.limits.frames || bytes > self.limits.bytes - used.bytes {
            return None;
        }
        used.frames += 1;
        used.bytes += bytes;
        Some(Permit {
            budget: self.clone(),
            charge: Charge::Frame(bytes),
        })
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap();
        match self.charge {
            Charge::Stream => used.streams -= 1,
            Charge::Frame(bytes) => {
                used.frames -= 1;
                used.bytes -= bytes;
            }
        }
    }
}
