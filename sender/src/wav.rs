// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! A streaming WAV reader for Music Assistant's stream server. The stream has
//! no known length, so the RIFF and data sizes are ignored (MA writes them as
//! 0 or 0xFFFFFFFF) and the data runs until the connection ends.

use std::io::{self, Read};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    S16,
    S24,
    S32,
    F32,
}

impl SampleFormat {
    pub fn bytes(self) -> usize {
        match self {
            SampleFormat::S16 => 2,
            SampleFormat::S24 => 3,
            SampleFormat::S32 | SampleFormat::F32 => 4,
        }
    }

    /// Decodes little-endian samples into floats in [-1, 1).
    pub fn decode(self, bytes: &[u8], out: &mut Vec<f32>) {
        let n = self.bytes();
        out.extend(bytes.chunks_exact(n).map(|b| match self {
            SampleFormat::S16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            SampleFormat::S24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            SampleFormat::S32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
            SampleFormat::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        }));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavFormat {
    pub sample_rate: u32,
    pub channels: usize,
    pub format: SampleFormat,
}

impl WavFormat {
    pub fn frame_bytes(&self) -> usize {
        self.channels * self.format.bytes()
    }
}

pub struct WavReader<R> {
    inner: R,
    pub format: WavFormat,
    carry: Vec<u8>,
}

fn read_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn read_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl<R: Read> WavReader<R> {
    /// Reads the header up to the start of the sample data.
    pub fn new(mut inner: R) -> io::Result<Self> {
        let mut riff = [0u8; 12];
        inner.read_exact(&mut riff)?;
        if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
            return Err(bad("not a RIFF/WAVE stream"));
        }
        let mut format = None;
        loop {
            let mut head = [0u8; 8];
            inner.read_exact(&mut head)?;
            let size = read_u32(&head[4..8]) as usize;
            match &head[0..4] {
                b"fmt " => {
                    if !(16..=4096).contains(&size) {
                        return Err(bad("bad fmt chunk"));
                    }
                    let mut f = vec![0u8; size + (size & 1)];
                    inner.read_exact(&mut f)?;
                    let mut tag = read_u16(&f[0..2]);
                    let channels = read_u16(&f[2..4]) as usize;
                    let sample_rate = read_u32(&f[4..8]);
                    let bits = read_u16(&f[14..16]);
                    if tag == 0xFFFE && size >= 40 {
                        // WAVE_FORMAT_EXTENSIBLE: the subformat GUID starts with the real tag.
                        tag = read_u16(&f[24..26]);
                    }
                    let sf = match (tag, bits) {
                        (1, 16) => SampleFormat::S16,
                        (1, 24) => SampleFormat::S24,
                        (1, 32) => SampleFormat::S32,
                        (3, 32) => SampleFormat::F32,
                        _ => return Err(bad(&format!("unsupported WAV format {tag}/{bits} bit"))),
                    };
                    if channels == 0 || channels > 8 || sample_rate == 0 {
                        return Err(bad("bad channel count or sample rate"));
                    }
                    format = Some(WavFormat { sample_rate, channels, format: sf });
                }
                b"data" => {
                    let format = format.ok_or_else(|| bad("data before fmt"))?;
                    return Ok(WavReader { inner, format, carry: Vec::new() });
                }
                _ => {
                    // LIST, fact and the like: skip.
                    let skip = size + (size & 1);
                    io::copy(&mut (&mut inner).take(skip as u64), &mut io::sink())?;
                }
            }
        }
    }

    /// Appends up to `max_frames` interleaved frames to `out` as floats.
    /// Returns the frames read; 0 at the end of the stream.
    pub fn read_frames(&mut self, max_frames: usize, out: &mut Vec<f32>) -> io::Result<usize> {
        let fb = self.format.frame_bytes();
        let mut buf = std::mem::take(&mut self.carry);
        let want = max_frames * fb;
        let start = buf.len();
        buf.resize(want.max(start + 1), 0);
        let mut filled = start;
        while filled < fb || (filled == start && filled < want) {
            let n = self.inner.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        let frames = filled / fb;
        self.format.format.decode(&buf[..frames * fb], out);
        self.carry = buf[frames * fb..filled].to_vec();
        Ok(frames)
    }
}

#[cfg(test)]
pub fn make_wav(sample_rate: u32, channels: u16, bits: u16, float: bool, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&u32::MAX.to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"LIST");
    v.extend_from_slice(&4u32.to_le_bytes());
    v.extend_from_slice(b"INFO");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&(if float { 3u16 } else { 1u16 }).to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&sample_rate.to_le_bytes());
    let block = channels as u32 * bits as u32 / 8;
    v.extend_from_slice(&(sample_rate * block).to_le_bytes());
    v.extend_from_slice(&(block as u16).to_le_bytes());
    v.extend_from_slice(&bits.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&u32::MAX.to_le_bytes());
    v.extend_from_slice(data);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that hands out one byte at a time, like a slow socket.
    struct Trickle(Vec<u8>, usize);
    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.1 >= self.0.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[self.1];
            self.1 += 1;
            Ok(1)
        }
    }

    #[test]
    fn reads_s24_stereo_across_partial_reads() {
        let mut data = Vec::new();
        for s in [0x400000i32, -0x400000, 0x000001, -1] {
            data.extend_from_slice(&s.to_le_bytes()[..3]);
        }
        let wav = make_wav(48000, 2, 24, false, &data);
        let mut r = WavReader::new(Trickle(wav, 0)).unwrap();
        assert_eq!(r.format, WavFormat { sample_rate: 48000, channels: 2, format: SampleFormat::S24 });
        let mut out = Vec::new();
        let mut total = 0;
        loop {
            let n = r.read_frames(1, &mut out).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, 2);
        assert_eq!(out, vec![0.5, -0.5, 1.0 / 8_388_608.0, -1.0 / 8_388_608.0]);
    }

    #[test]
    fn reads_s16_and_f32() {
        let wav = make_wav(44100, 1, 16, false, &(-16384i16).to_le_bytes());
        let mut r = WavReader::new(&wav[..]).unwrap();
        let mut out = Vec::new();
        assert_eq!(r.read_frames(16, &mut out).unwrap(), 1);
        assert_eq!(out, vec![-0.5]);

        let wav = make_wav(96000, 1, 32, true, &0.25f32.to_le_bytes());
        let mut r = WavReader::new(&wav[..]).unwrap();
        out.clear();
        r.read_frames(16, &mut out).unwrap();
        assert_eq!(out, vec![0.25]);
        assert_eq!(r.format.sample_rate, 96000);
    }

    #[test]
    fn refuses_what_it_cannot_play() {
        assert!(WavReader::new(&b"RIFF\0\0\0\0WAVX"[..]).is_err());
        let wav = make_wav(48000, 2, 8, false, &[]);
        assert!(WavReader::new(&wav[..]).is_err());
    }
}
