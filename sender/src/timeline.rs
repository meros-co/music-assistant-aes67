// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! An output's audio, indexed by media-clock time.
//!
//! Writers put frames at the media time they must be heard; the stream's
//! transmit thread reads the frames of each packet at that packet's time and
//! clears them behind itself. A Sendspin chunk lands where its timestamp says,
//! an HTTP stream is laid down back to back from its start time, and both are
//! held back when they get more than the buffer's length ahead of playout.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Media time in frames, unwrapped to 64 bits (the RTP timestamp is its low 32).
pub type MediaTime = u64;

struct Inner {
    samples: Vec<f32>,
    /// One flag per frame: written since playout last passed it.
    present: Vec<bool>,
    /// The next frame playout will read; nothing before it can be written.
    read_pos: MediaTime,
    /// Readers have started (read_pos is meaningful).
    reading: bool,
    /// Bumped by clear(): a writer blocked across a clear gives up.
    generation: u64,
    frames_read_present: u64,
    frames_late: u64,
}

pub struct Timeline {
    channels: usize,
    capacity: usize,
    inner: Mutex<Inner>,
    space: Condvar,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WriteResult {
    Written,
    /// Every frame was before playout and was dropped.
    Late,
    /// The timeline was cleared while waiting.
    Cleared,
    /// Timed out waiting for space.
    TimedOut,
}

impl Timeline {
    /// `capacity` frames of `channels` channels; a power of two is not required.
    pub fn new(channels: usize, capacity: usize) -> Self {
        Timeline {
            channels,
            capacity,
            inner: Mutex::new(Inner {
                samples: vec![0.0; channels * capacity],
                present: vec![false; capacity],
                read_pos: 0,
                reading: false,
                generation: 0,
                frames_read_present: 0,
                frames_late: 0,
            }),
            space: Condvar::new(),
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Writes interleaved frames starting at `at`. Frames already behind
    /// playout are dropped (and counted); when the end would be more than the
    /// capacity ahead of playout, waits up to `wait` for playout to catch up.
    pub fn write(&self, at: MediaTime, frames: &[f32], wait: Duration) -> WriteResult {
        let n = frames.len() / self.channels;
        let deadline = std::time::Instant::now() + wait;
        let mut g = self.inner.lock().unwrap();
        let generation = g.generation;
        loop {
            if g.generation != generation {
                return WriteResult::Cleared;
            }
            if !g.reading || at + n as u64 <= g.read_pos + self.capacity as u64 {
                break;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return WriteResult::TimedOut;
            }
            g = self.space.wait_timeout(g, deadline - now).unwrap().0;
        }
        let mut wrote = false;
        for i in 0..n {
            let t = at + i as u64;
            if g.reading && t < g.read_pos {
                g.frames_late += 1;
                continue;
            }
            if g.reading && t >= g.read_pos + self.capacity as u64 {
                break;
            }
            let slot = (t % self.capacity as u64) as usize;
            let src = &frames[i * self.channels..(i + 1) * self.channels];
            g.samples[slot * self.channels..(slot + 1) * self.channels].copy_from_slice(src);
            g.present[slot] = true;
            wrote = true;
        }
        if wrote {
            WriteResult::Written
        } else {
            WriteResult::Late
        }
    }

    /// Reads `out.len() / channels` frames at `at` into `out` (silence where
    /// nothing was written), clears them, and moves playout past them.
    /// Returns how many frames held audio.
    pub fn read(&self, at: MediaTime, out: &mut [f32]) -> usize {
        let n = out.len() / self.channels;
        let mut g = self.inner.lock().unwrap();
        if !g.reading || at > g.read_pos {
            // First read, or playout skipped ahead: whatever lies in between is stale.
            let from = if g.reading { g.read_pos } else { at };
            let skip = (at - from.min(at)).min(self.capacity as u64);
            for i in 0..skip {
                let slot = ((from + i) % self.capacity as u64) as usize;
                g.present[slot] = false;
            }
            g.reading = true;
        }
        let mut present = 0;
        for i in 0..n {
            let t = at + i as u64;
            let dst = &mut out[i * self.channels..(i + 1) * self.channels];
            if t < g.read_pos {
                dst.fill(0.0);
                continue;
            }
            let slot = (t % self.capacity as u64) as usize;
            if g.present[slot] {
                dst.copy_from_slice(&g.samples[slot * self.channels..(slot + 1) * self.channels]);
                g.present[slot] = false;
                present += 1;
            } else {
                dst.fill(0.0);
            }
        }
        g.read_pos = g.read_pos.max(at + n as u64);
        g.frames_read_present += present as u64;
        drop(g);
        self.space.notify_all();
        present
    }

    /// Drops everything buffered and wakes any blocked writer.
    pub fn clear(&self) {
        let mut g = self.inner.lock().unwrap();
        g.present.fill(false);
        g.generation += 1;
        drop(g);
        self.space.notify_all();
    }

    /// Where playout is, once it has started.
    pub fn read_pos(&self) -> Option<MediaTime> {
        let g = self.inner.lock().unwrap();
        g.reading.then_some(g.read_pos)
    }

    /// Frames buffered ahead of playout.
    pub fn buffered(&self) -> usize {
        let g = self.inner.lock().unwrap();
        g.present.iter().filter(|&&p| p).count()
    }

    pub fn stats(&self) -> (u64, u64) {
        let g = self.inner.lock().unwrap();
        (g.frames_read_present, g.frames_late)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const NO_WAIT: Duration = Duration::ZERO;

    #[test]
    fn reads_what_was_written_at_its_time() {
        let t = Timeline::new(2, 16);
        assert_eq!(
            t.write(100, &[1.0, 2.0, 3.0, 4.0], NO_WAIT),
            WriteResult::Written
        );
        let mut out = [9.0; 8];
        assert_eq!(t.read(99, &mut out), 2);
        assert_eq!(out, [0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 0.0, 0.0]);
        // Read once: the frames are gone.
        let mut out = [9.0; 8];
        assert_eq!(t.read(103, &mut out), 0);
    }

    #[test]
    fn drops_late_frames() {
        let t = Timeline::new(1, 16);
        let mut out = [0.0; 4];
        t.read(10, &mut out);
        // Playout is at 14: frames 8..13 are all late.
        assert_eq!(
            t.write(8, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], NO_WAIT),
            WriteResult::Late
        );
        assert_eq!(t.stats().1, 6);
        assert_eq!(
            t.write(12, &[1.0, 2.0, 3.0, 4.0], NO_WAIT),
            WriteResult::Written
        );
        let mut out = [0.0; 4];
        assert_eq!(t.read(14, &mut out), 2);
        assert_eq!(out, [3.0, 4.0, 0.0, 0.0]);
        assert_eq!(t.write(0, &[1.0], NO_WAIT), WriteResult::Late);
    }

    #[test]
    fn writer_waits_for_space_and_clear_releases_it() {
        let t = Arc::new(Timeline::new(1, 8));
        let mut out = [0.0; 1];
        t.read(0, &mut out);
        assert_eq!(t.write(1, &[1.0; 8], NO_WAIT), WriteResult::Written);
        assert_eq!(t.write(9, &[1.0; 4], NO_WAIT), WriteResult::TimedOut);
        let t2 = t.clone();
        let writer = std::thread::spawn(move || t2.write(9, &[2.0; 4], Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        let mut out = [0.0; 4];
        assert_eq!(t.read(1, &mut out), 4);
        assert_eq!(writer.join().unwrap(), WriteResult::Written);
        let t3 = t.clone();
        let blocked = std::thread::spawn(move || t3.write(100, &[3.0; 4], Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        t.clear();
        assert_eq!(blocked.join().unwrap(), WriteResult::Cleared);
        assert_eq!(t.buffered(), 0);
    }

    #[test]
    fn a_skip_ahead_discards_what_was_between() {
        let t = Timeline::new(1, 32);
        let mut out = [0.0; 2];
        t.read(0, &mut out);
        t.write(2, &[1.0; 10], NO_WAIT);
        t.read(8, &mut out);
        assert_eq!(out, [1.0, 1.0]);
        assert_eq!(t.buffered(), 2);
        assert_eq!(t.read_pos(), Some(10));
    }
}
