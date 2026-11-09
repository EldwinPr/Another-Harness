//! Stopping a turn that is in progress (issue 18).
//!
//! A [`Cancel`] is a flag that many owners share: the key watcher sets it when Esc or Ctrl-C is
//! pressed while the model works, and the places that wait (the request to the router, a running
//! `bash` command, the loop between tool calls) look at it and give up.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The shared flag. Cloning gives another handle to the same flag.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks everything waiting on this flag to stop.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Clears the flag; a new message starts with it clear.
    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Whether the bytes read from the keyboard ask for a cancel: Ctrl-C, or an Esc that stands alone.
/// An Esc followed by more bytes is the start of a key sequence (arrow keys, Alt-x) and is not one.
pub fn asks_to_cancel(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x03 => return true,
            0x1b => match bytes.get(i + 1) {
                None => return true,
                // `ESC [` or `ESC O`: skip the sequence up to its final byte.
                Some(b'[' | b'O') => {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                }
                // Alt-x: the pair is one key.
                Some(_) => i += 1,
            },
            _ => {}
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_shares_the_flag_and_reset_clears_it() {
        let a = Cancel::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
        b.reset();
        assert!(!a.is_cancelled());
    }

    #[test]
    fn a_lone_escape_and_ctrl_c_cancel() {
        assert!(asks_to_cancel(b"\x1b"));
        assert!(asks_to_cancel(b"\x03"));
        assert!(asks_to_cancel(b"abc\x03"));
        assert!(asks_to_cancel(b"x\x1b"));
    }

    #[test]
    fn arrow_keys_other_keys_and_alt_combinations_do_not() {
        assert!(!asks_to_cancel(b""));
        assert!(!asks_to_cancel(b"hello\r"));
        assert!(!asks_to_cancel(b"\x1b[A"));
        assert!(!asks_to_cancel(b"\x1b[1;5C"));
        assert!(!asks_to_cancel(b"\x1bOP"));
        assert!(!asks_to_cancel(b"\x1bb"));
        assert!(!asks_to_cancel(b"\x1b[A\x1b[B"));
    }

    #[test]
    fn a_sequence_followed_by_a_lone_escape_still_cancels() {
        assert!(asks_to_cancel(b"\x1b[A\x1b"));
    }
}
