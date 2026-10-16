//! Audio engine: one decoder thread (symphonia AAC/MP4) feeding a ~500 ms
//! lock-free ring buffer that the cpal output callback drains.
//!
//! The callback never waits on anything: the decoder writes into the ring and
//! the callback reads from it without locks (`rtrb`). With the earlier mutex,
//! the callback had to output a period of silence whenever the decoder happened
//! to hold the lock — a short cut every few seconds.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use rtrb::{Consumer, Producer, RingBuffer};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use crate::Res;

/// Decoded audio kept ahead of the device: rides out scheduling hiccups.
const BUFFER_SECS: f64 = 0.5;
/// How long the decoder waits for the callback to empty the ring on seek/load.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(300);

pub enum Event {
    Finished,
    Error(String),
}

enum Cmd {
    Load(Box<dyn MediaSource>),
    Seek(f64),
    Stop,
}

struct Shared {
    paused: AtomicBool,
    volume: AtomicU32,
    /// Set by the decoder to have the callback discard everything buffered
    /// (only the reading side of the ring can); the callback clears it when done.
    flush: AtomicBool,
    /// Frames sent to the device since the last load/seek.
    played: AtomicU64,
    /// Seconds offset of the last load/seek, as f64 bits.
    base: AtomicU64,
    /// Length of the loaded track in seconds, as f64 bits (0 = unknown).
    duration: AtomicU64,
    /// A track is loaded and hasn't finished decoding (so an empty buffer is a glitch).
    active: AtomicBool,
    /// Diagnostics: samples missing because the buffer ran dry mid-track, and
    /// packets the decoder couldn't decode.
    underrun_samples: AtomicU64,
    decode_errors: AtomicU64,
}

/// Read by the glitch probe test; kept in the app for diagnosing playback.
#[allow(dead_code)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Glitches {
    pub underrun_samples: u64,
    pub decode_errors: u64,
}

pub struct Player {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    out_rate: u32,
}

impl Player {
    pub fn new(on_event: impl Fn(Event) + Send + 'static) -> Res<Self> {
        let shared = Arc::new(Shared {
            paused: AtomicBool::new(false),
            volume: AtomicU32::new(0.8f32.to_bits()),
            flush: AtomicBool::new(false),
            played: AtomicU64::new(0),
            base: AtomicU64::new(0),
            duration: AtomicU64::new(0),
            active: AtomicBool::new(false),
            underrun_samples: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
        });

        // cpal streams are !Send on some hosts, so the stream lives on its own
        // thread; the ring's writing end comes back to the decoder.
        let (fmt_tx, fmt_rx) = mpsc::channel::<Res<(u32, usize, Producer<f32>)>>();
        let out_shared = shared.clone();
        std::thread::Builder::new().name("audio-out".into()).spawn(move || match open_output(out_shared) {
            Ok((stream, rate, ch, producer)) => {
                let _ = fmt_tx.send(Ok((rate, ch, producer)));
                let _stream = stream;
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                let _ = fmt_tx.send(Err(e));
            }
        })?;
        let (out_rate, out_ch, producer) = fmt_rx.recv()??;

        let (tx, rx) = mpsc::channel();
        let dec_shared = shared.clone();
        std::thread::Builder::new()
            .name("audio-decode".into())
            .spawn(move || decode_loop(rx, dec_shared, producer, out_rate, out_ch, on_event))?;

        Ok(Self { tx, shared, out_rate })
    }

    pub fn load(&self, source: Box<dyn MediaSource>) {
        self.shared.paused.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Load(source));
    }
    pub fn seek(&self, secs: f64) {
        let _ = self.tx.send(Cmd::Seek(secs.max(0.0)));
    }
    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }
    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }
    pub fn set_volume(&self, v: f32) {
        self.shared.volume.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    pub fn position(&self) -> f64 {
        f64::from_bits(self.shared.base.load(Ordering::Relaxed))
            + self.shared.played.load(Ordering::Relaxed) as f64 / self.out_rate as f64
    }
    pub fn duration(&self) -> f64 {
        f64::from_bits(self.shared.duration.load(Ordering::Relaxed))
    }
    #[allow(dead_code)]
    pub fn glitches(&self) -> Glitches {
        Glitches {
            underrun_samples: self.shared.underrun_samples.load(Ordering::Relaxed),
            decode_errors: self.shared.decode_errors.load(Ordering::Relaxed),
        }
    }
}

fn open_output(shared: Arc<Shared>) -> Res<(cpal::Stream, u32, usize, Producer<f32>)> {
    let device = cpal::default_host().default_output_device().ok_or("No audio output device")?;
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let (rate, ch) = (config.sample_rate, config.channels as usize);
    let capacity = ((rate as f64 * BUFFER_SECS) as usize).max(1024) * ch;
    let (producer, consumer) = RingBuffer::<f32>::new(capacity);
    let stream = match format {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, config, shared, consumer)?,
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, config, shared, consumer)?,
        cpal::SampleFormat::I32 => build_stream::<i32>(&device, config, shared, consumer)?,
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, config, shared, consumer)?,
        other => return Err(format!("Unsupported output format {other}").into()),
    };
    stream.play()?;
    Ok((stream, rate, ch, producer))
}

fn build_stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: Arc<Shared>,
    mut ring: Consumer<f32>,
) -> Res<cpal::Stream> {
    let ch = config.channels as usize;
    let stream = device.build_output_stream(
        config,
        move |out: &mut [T], _| {
            let silence = T::from_sample(0.0f32);
            if shared.flush.load(Ordering::Acquire) {
                if let Ok(stale) = ring.read_chunk(ring.slots()) {
                    stale.commit_all();
                }
                shared.flush.store(false, Ordering::Release);
            }
            if shared.paused.load(Ordering::Relaxed) {
                out.fill(silence);
                return;
            }
            let v = f32::from_bits(shared.volume.load(Ordering::Relaxed));
            let gain = v * v; // perceptual curve
            // Whole frames only, so channels never shift.
            let n = ring.slots().min(out.len()) / ch * ch;
            if let Ok(chunk) = ring.read_chunk(n) {
                let (a, b) = chunk.as_slices();
                for (o, x) in out.iter_mut().zip(a.iter().chain(b)) {
                    *o = T::from_sample(x * gain);
                }
                chunk.commit_all();
            }
            out[n..].fill(silence);
            if n < out.len() && shared.active.load(Ordering::Relaxed) {
                shared.underrun_samples.fetch_add((out.len() - n) as u64, Ordering::Relaxed);
            }
            shared.played.fetch_add((n / ch) as u64, Ordering::Relaxed);
        },
        |e| eprintln!("audio stream error: {e}"),
        None,
    )?;
    Ok(stream)
}

struct Track {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    resampler: Option<Resampler>,
    duration_secs: f64,
}

fn open_track(source: Box<dyn MediaSource>) -> Res<Track> {
    let mss = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    let format = symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())?;
    let track = format.default_track(TrackType::Audio).ok_or("No audio track")?;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or("Not an audio track")?;
    let decoder = symphonia::default::get_codecs().make_audio_decoder(params, &AudioDecoderOptions::default())?;
    let track_id = track.id;
    let duration_secs = match (track.duration, track.time_base) {
        (Some(d), Some(tb)) => d.get() as f64 * f64::from(tb),
        _ => 0.0,
    };
    Ok(Track { format, decoder, track_id, resampler: None, duration_secs })
}

/// Have the callback discard everything buffered, then restart the position at `base`.
fn reset_output(shared: &Shared, base: f64) {
    shared.flush.store(true, Ordering::Release);
    let start = Instant::now();
    while shared.flush.load(Ordering::Acquire) && start.elapsed() < FLUSH_TIMEOUT {
        std::thread::sleep(Duration::from_millis(1));
    }
    shared.played.store(0, Ordering::Relaxed);
    shared.base.store(base.to_bits(), Ordering::Relaxed);
}

fn decode_loop(
    rx: Receiver<Cmd>,
    shared: Arc<Shared>,
    mut ring: Producer<f32>,
    out_rate: u32,
    out_ch: usize,
    on_event: impl Fn(Event),
) {
    let capacity = ring.buffer().capacity();
    let mut track: Option<Track> = None;
    let mut draining = false;
    let mut scratch: Vec<f32> = Vec::new();
    let mut converted: Vec<f32> = Vec::new();
    // Decoded samples not yet in the ring (it was full).
    let mut pending: Vec<f32> = Vec::new();
    let mut pending_pos = 0usize;

    loop {
        let busy = track.is_some() || pending_pos < pending.len();
        let cmd = if busy {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            }
        } else if draining {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        };

        if cmd.is_some() {
            pending.clear();
            pending_pos = 0;
        }
        match cmd {
            Some(Cmd::Load(source)) => {
                draining = false;
                shared.active.store(false, Ordering::Relaxed);
                reset_output(&shared, 0.0);
                track = match open_track(source) {
                    Ok(t) => {
                        shared.duration.store(t.duration_secs.to_bits(), Ordering::Relaxed);
                        shared.active.store(true, Ordering::Relaxed);
                        Some(t)
                    }
                    Err(e) => {
                        on_event(Event::Error(e.to_string()));
                        None
                    }
                };
            }
            Some(Cmd::Seek(secs)) => {
                if let Some(t) = track.as_mut() {
                    let time = Time::try_new(secs.trunc() as i64, (secs.fract() * 1e9) as u32).unwrap_or(Time::ZERO);
                    if t.format.seek(SeekMode::Coarse, SeekTo::Time { time, track_id: Some(t.track_id) }).is_ok() {
                        t.decoder.reset();
                        if let Some(r) = t.resampler.as_mut() {
                            r.reset();
                        }
                        reset_output(&shared, secs);
                    }
                }
            }
            Some(Cmd::Stop) => {
                shared.active.store(false, Ordering::Relaxed);
                track = None;
                draining = false;
                reset_output(&shared, 0.0);
            }
            None => {}
        }

        // Move decoded samples into the ring as space frees up.
        if pending_pos < pending.len() {
            let n = ring.slots().min(pending.len() - pending_pos);
            if n > 0 {
                if let Ok(chunk) = ring.write_chunk_uninit(n) {
                    chunk.fill_from_iter(pending[pending_pos..pending_pos + n].iter().copied());
                    pending_pos += n;
                }
            }
            if pending_pos < pending.len() {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            pending.clear();
            pending_pos = 0;
        }

        if draining {
            if ring.slots() == capacity {
                draining = false;
                on_event(Event::Finished);
            }
            continue;
        }

        let Some(t) = track.as_mut() else { continue };
        let packet = match t.format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) | Err(_) => {
                shared.active.store(false, Ordering::Relaxed);
                track = None;
                draining = true;
                continue;
            }
        };
        if packet.track_id != t.track_id {
            continue;
        }
        let Ok(audio) = t.decoder.decode(&packet) else {
            shared.decode_errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let in_rate = audio.spec().rate();
        let in_ch = audio.spec().channels().count().max(1);
        scratch.resize(audio.samples_interleaved(), 0.0);
        audio.copy_to_slice_interleaved(&mut scratch[..]);

        remix(&scratch, in_ch, out_ch, &mut converted);
        if in_rate == out_rate {
            pending.extend_from_slice(&converted);
        } else {
            let resampler = t.resampler.get_or_insert_with(|| Resampler::new(in_rate, out_rate, out_ch));
            resampler.process(&converted, &mut pending);
        }
    }
}

/// Map interleaved `in_ch` audio to `out_ch` channels (mono↔stereo, extra channels silent).
fn remix(input: &[f32], in_ch: usize, out_ch: usize, out: &mut Vec<f32>) {
    out.clear();
    if in_ch == out_ch {
        out.extend_from_slice(input);
        return;
    }
    for frame in input.chunks_exact(in_ch) {
        for c in 0..out_ch {
            let s = if out_ch == 1 {
                frame.iter().sum::<f32>() / in_ch as f32
            } else if c < 2 {
                frame[c.min(in_ch - 1)]
            } else {
                0.0
            };
            out.push(s);
        }
    }
}

/// Streaming linear-interpolation resampler. Cheap and good enough for
/// 44.1k↔48k playback of 128 kbps AAC.
struct Resampler {
    step: f64,
    pos: f64,
    prev: Vec<f32>,
    ch: usize,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32, ch: usize) -> Self {
        Self { step: in_rate as f64 / out_rate as f64, pos: 0.0, prev: vec![0.0; ch], ch }
    }

    fn reset(&mut self) {
        self.pos = 0.0;
        self.prev.fill(0.0);
    }

    /// Frame index 0 is `prev` (last frame of the previous call), index k+1 is input frame k.
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = input.len() / self.ch;
        if n == 0 {
            return;
        }
        let frame = |j: usize, c: usize| if j == 0 { self.prev[c] } else { input[(j - 1) * self.ch + c] };
        while self.pos + 1.0 <= n as f64 {
            let j = self.pos as usize;
            let t = (self.pos - j as f64) as f32;
            for c in 0..self.ch {
                let a = frame(j, c);
                let b = frame(j + 1, c);
                out.push(a + (b - a) * t);
            }
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.prev.copy_from_slice(&input[(n - 1) * self.ch..n * self.ch]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_and_decode_real_track() {
        let stream = crate::youtube::fetch_audio("kJQP7kiw5Fk", false).expect("fetch");
        let mut t = open_track(Box::new(std::io::Cursor::new(stream.bytes))).expect("open");
        let (mut frames, mut rate) = (0usize, 0u32);
        while let Ok(Some(p)) = t.format.next_packet() {
            if let Ok(a) = t.decoder.decode(&p) {
                rate = a.spec().rate();
                frames += a.frames();
            }
        }
        let secs = frames as f64 / rate as f64;
        assert!(secs > 60.0, "decoded only {secs}s");
        assert!((secs - t.duration_secs).abs() < 1.0, "decoded {secs}s vs container {}s", t.duration_secs);
    }

    #[test]
    fn resampler_ratio() {
        let mut r = Resampler::new(44_100, 48_000, 2);
        let mut out = Vec::new();
        for _ in 0..100 {
            r.process(&vec![0.5; 441 * 2], &mut out);
        }
        let frames = out.len() / 2;
        assert!((frames as i64 - 48_000).abs() <= 2, "{frames}");
    }
}

#[cfg(test)]
mod glitch_probe {
    use super::*;

    /// Real playback (volume 0) with the app's normal background work running,
    /// reporting how often audio drops out and why. Also checks that seeking
    /// lands where asked (the ring is flushed, not played out).
    #[test]
    #[ignore = "plays a track for ~45 s on the default audio device (muted)"]
    fn count_glitches_during_playback() {
        let player = Player::new(|_| {}).expect("audio device");
        player.set_volume(0.0);
        let source = crate::youtube::open_audio("OPf0YbXqDm0", false).expect("open");
        player.load(source);
        // The background work the app does while a song plays.
        let liked = crate::youtube::playlist_tracks("LM").unwrap_or_default();
        let ids: Vec<&str> = liked.iter().skip(5).take(12).map(|t| t.video_id.as_str()).collect();
        crate::youtube::warm(&ids, true);
        let start = Instant::now();
        let mut last = Glitches::default();
        while start.elapsed() < Duration::from_secs(45) {
            std::thread::sleep(Duration::from_secs(5));
            let g = player.glitches();
            println!(
                "{:>4.0}s pos {:>5.1}s | underrun samples +{} | decode errors +{}",
                start.elapsed().as_secs_f64(),
                player.position(),
                g.underrun_samples - last.underrun_samples,
                g.decode_errors - last.decode_errors
            );
            last = g;
        }
        player.seek(120.0);
        std::thread::sleep(Duration::from_secs(2));
        println!("after seek to 120 s: pos {:.1}s", player.position());
        println!("total: {:?}", player.glitches());
    }
}
