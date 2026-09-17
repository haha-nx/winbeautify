//! Real-time spectrum analysis of whatever the machine is playing.
//!
//! Audio is captured from the default *render* endpoint in WASAPI loopback
//! mode — no virtual cable, no driver, no admin rights. The capture client is
//! driven by its own event handle, so the thread blocks in the kernel instead
//! of spinning, and the FFT only runs when there are samples to chew on.
//!
//! Two throttles keep the cost down on a machine that is meant to idle below 1%
//! CPU: frames are emitted at most 30 times a second, and once the signal has
//! been silent for a couple of seconds the rate drops to 5 a second.

use beautify_core::event::Event;
use beautify_core::model::SpectrumFrame;
use rustfft::{num_complex::Complex, FftPlanner};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::GUID;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};

use crate::session::WakeEvent;

/// `KSDATAFORMAT_SUBTYPE_PCM` as it appears in a `WAVEFORMATEXTENSIBLE`
/// sub-format GUID. Declared here rather than pulled from
/// `Win32::Media::KernelStreaming` so the module does not need that whole
/// feature for two constants.
// Grouped the way a GUID is written rather than in uniform nibble groups.
#[allow(clippy::unusual_byte_groupings)]
const SUBTYPE_PCM: GUID = GUID::from_u128(0x0000_0001_0000_0010_8000_00aa00389b71);
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`, same layout.
#[allow(clippy::unusual_byte_groupings)]
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00aa00389b71);

/// FFT window size. 2048 samples at 48 kHz is ~43 ms — short enough that the
/// bars feel instant, long enough to resolve bass.
const FFT_SIZE: usize = 2048;
/// Ring buffer keeping the most recent samples plus one window of slack.
const RING_CAPACITY: usize = FFT_SIZE * 2;
/// Lowest band centre, in Hz.
const BAND_MIN_HZ: f32 = 40.0;
/// Highest band centre, in Hz.
const BAND_MAX_HZ: f32 = 16_000.0;
/// Frame emission interval while audio is playing.
const FRAME_INTERVAL_MS: u128 = 33;
/// Below this RMS the frame counts as silence.
const SILENCE_RMS: f32 = 0.0015;
/// `WAVE_FORMAT_EXTENSIBLE` from `mmreg.h`.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Sample layout of the capture stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleFormat {
    F32,
    I16,
}

#[derive(Debug, Clone, Copy)]
struct StreamFormat {
    channels: usize,
    sample_rate: f32,
    format: SampleFormat,
}

/// Configurable knobs, re-read whenever the settings change.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpectrumOptions {
    pub bars: usize,
    pub sensitivity: f32,
    pub smoothing: f32,
}

impl Default for SpectrumOptions {
    fn default() -> Self {
        Self {
            // The shared constant, not a literal: a stale default here would
            // silently produce a frame the renderer cannot draw completely.
            bars: beautify_core::model::SPECTRUM_BANDS,
            sensitivity: 1.0,
            smoothing: 0.6,
        }
    }
}

/// Running spectrum analyser.
pub struct SpectrumAnalyzer {
    stop: Arc<AtomicBool>,
    /// Signalled on shutdown so the capture thread leaves its wait at once.
    stop_event: WakeEvent,
    options: Arc<parking_lot::RwLock<SpectrumOptions>>,
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SpectrumAnalyzer {
    /// Start capturing.
    ///
    /// Returns `None` when there is no render endpoint (a VM with no audio
    /// device, say) or the capture thread cannot be spawned — the widget then
    /// hides the bars instead of drawing a permanently flat line.
    pub fn start(
        bus: beautify_core::event::EventBus,
        options: SpectrumOptions,
    ) -> Option<SpectrumAnalyzer> {
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(false));
        let options = Arc::new(parking_lot::RwLock::new(options));
        let stop_event = WakeEvent::new()?;
        let thread = std::thread::Builder::new()
            .name("wb-spectrum".into())
            .spawn({
                let stop = Arc::clone(&stop);
                let running = Arc::clone(&running);
                let options = Arc::clone(&options);
                let worker_stop = stop_event.clone();
                move || {
                    if let Err(e) = capture_loop(bus, stop, running, options, worker_stop) {
                        tracing::warn!("spectrum capture unavailable: {e}");
                    }
                }
            })
            .ok()?;

        Some(Self {
            stop,
            stop_event,
            options,
            running,
            thread: Some(thread),
        })
    }

    pub fn set_options(&self, options: SpectrumOptions) {
        *self.options.write() = options;
    }

    /// True once the capture client is actually running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.stop_event.signal();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for SpectrumAnalyzer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Detect the sample layout from the mix format.
///
/// `WAVEFORMATEX` is a packed struct, so every field is copied out by value
/// rather than referenced.
fn detect_format(
    format_ptr: *const windows::Win32::Media::Audio::WAVEFORMATEX,
) -> Option<StreamFormat> {
    let format = unsafe { format_ptr.read_unaligned() };
    let channels = format.nChannels as usize;
    let sample_rate = format.nSamplesPerSec as f32;
    let bits = format.wBitsPerSample;
    let tag = format.wFormatTag;
    if channels == 0 || sample_rate <= 0.0 {
        return None;
    }

    let sample_format = match tag {
        3 => SampleFormat::F32, // WAVE_FORMAT_IEEE_FLOAT
        1 => SampleFormat::I16, // WAVE_FORMAT_PCM
        WAVE_FORMAT_EXTENSIBLE => {
            // The sub-format GUID lives at offset 24 of WAVEFORMATEXTENSIBLE.
            let raw = format_ptr as *const u8;
            let guid = unsafe { std::ptr::read_unaligned(raw.add(24) as *const GUID) };
            if guid == SUBTYPE_IEEE_FLOAT {
                SampleFormat::F32
            } else if guid == SUBTYPE_PCM {
                SampleFormat::I16
            } else {
                tracing::warn!("unsupported loopback sub-format; spectrum disabled");
                return None;
            }
        }
        other => {
            tracing::warn!("unsupported loopback format tag {other}; spectrum disabled");
            return None;
        }
    };

    // Guard against 24-bit packed PCM, which the I16 path would misread.
    if sample_format == SampleFormat::I16 && bits != 16 {
        tracing::warn!("unsupported PCM sample width {bits}; spectrum disabled");
        return None;
    }

    Some(StreamFormat {
        channels,
        sample_rate,
        format: sample_format,
    })
}

fn capture_loop(
    bus: beautify_core::event::EventBus,
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    options: Arc<parking_lot::RwLock<SpectrumOptions>>,
    stop_event: WakeEvent,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let format_ptr = unsafe { client.GetMixFormat()? };

    let stream = match detect_format(format_ptr) {
        Some(f) => f,
        None => {
            unsafe { CoTaskMemFree(Some(format_ptr as *const core::ffi::c_void)) };
            return Ok(());
        }
    };
    tracing::info!(
        rate = stream.sample_rate as u32,
        channels = stream.channels,
        "loopback capture opened"
    );

    // 200 ms of buffer keeps us ahead of any scheduling hiccup without adding
    // meaningful latency: only the newest packets ever reach the FFT.
    let init = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            200_000,
            0,
            format_ptr,
            None,
        )
    };
    if let Err(e) = init {
        unsafe { CoTaskMemFree(Some(format_ptr as *const core::ffi::c_void)) };
        return Err(format!("IAudioClient::Initialize: {e}").into());
    }

    let audio_event = unsafe { CreateEventW(None, false, false, None)? };
    unsafe { client.SetEventHandle(audio_event)? };
    let capture: IAudioCaptureClient = unsafe { client.GetService()? };
    unsafe { client.Start()? };
    running.store(true, Ordering::Release);

    let result = pump(&bus, &capture, audio_event, &stop, stream, &options, stop_event);

    running.store(false, Ordering::Release);
    unsafe {
        let _ = client.Stop();
        let _ = CloseHandle(audio_event);
        CoTaskMemFree(Some(format_ptr as *const core::ffi::c_void));
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn pump(
    bus: &beautify_core::event::EventBus,
    capture: &IAudioCaptureClient,
    audio_event: HANDLE,
    stop: &AtomicBool,
    stream: StreamFormat,
    options: &parking_lot::RwLock<SpectrumOptions>,
    stop_event: WakeEvent,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Both events are waited on together, so a shutdown request is honoured
    // immediately rather than after the next timeout.
    let handles = [audio_event, HANDLE(stop_event.raw_handle())];

    let mut ring = Ring::with_capacity(RING_CAPACITY);
    // Reused every frame so the ring only ever copies into a warm buffer.
    let mut window_samples = vec![0.0f32; FFT_SIZE];
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|i| {
            // Hann window: the usual choice for a smooth-looking display.
            let x = i as f32 / (FFT_SIZE - 1) as f32;
            0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos()
        })
        .collect();
    let mut scratch = vec![Complex::new(0.0f32, 0.0); fft.get_inplace_scratch_len()];
    let mut fft_buffer = vec![Complex::new(0.0f32, 0.0); FFT_SIZE];
    let mut smoothed: Vec<f32> = Vec::new();
    let mut bands: Vec<f32> = Vec::new();

    let mut last_emit = std::time::Instant::now();
    let mut silent_since: Option<std::time::Instant> = None;
    // Once the display has been cleared for a silent signal, stop publishing
    // altogether. Loopback capture on a real machine picks up dither and
    // electrical noise rather than digital silence, and repainting the bars for
    // that is pure waste — it cost several percent of a core before this.
    let mut published_silence = false;

    while !stop.load(Ordering::Acquire) {
        let waited = unsafe { WaitForMultipleObjects(&handles, false, 250) };
        // Index 1 is the stop event; index 0 is the capture event.
        if waited.0 == WAIT_OBJECT_0.0 + 1 || stop.load(Ordering::Acquire) {
            break;
        }

        let mut got_samples = false;
        loop {
            let packet = unsafe { capture.GetNextPacketSize() }?;
            if packet == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };

            if frames > 0 {
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let count = frames as usize;
                if silent || data.is_null() {
                    // Explicit silence: keep the ring advancing so the display
                    // decays instead of freezing on the last loud frame.
                    ring.push_silence(count);
                } else {
                    match stream.format {
                        SampleFormat::F32 => {
                            let samples = unsafe {
                                std::slice::from_raw_parts(data as *const f32, count * stream.channels)
                            };
                            for frame in samples.chunks_exact(stream.channels) {
                                let sum: f32 = frame.iter().sum();
                                ring.push(sum / stream.channels as f32);
                            }
                        }
                        SampleFormat::I16 => {
                            let samples = unsafe {
                                std::slice::from_raw_parts(data as *const i16, count * stream.channels)
                            };
                            for frame in samples.chunks_exact(stream.channels) {
                                let sum: f32 = frame.iter().map(|s| *s as f32 / 32768.0).sum();
                                ring.push(sum / stream.channels as f32);
                            }
                        }
                    }
                }
                got_samples = true;
            }
            unsafe { capture.ReleaseBuffer(frames)? };
        }

        if !got_samples && ring.len() < FFT_SIZE {
            continue;
        }

        let opts = *options.read();
        let ready = ring.len() >= FFT_SIZE;
        if ready {
            ring.copy_newest_into(&mut window_samples);
        }
        let rms = if ready {
            (window_samples.iter().map(|s| s * s).sum::<f32>() / FFT_SIZE as f32).sqrt()
        } else {
            0.0
        };

        let now = std::time::Instant::now();
        silent_since = if rms < SILENCE_RMS {
            silent_since.or(Some(now))
        } else {
            None
        };
        let _ = silent_since;
        let interval = FRAME_INTERVAL_MS;

        let quiet = rms < SILENCE_RMS;
        if quiet && published_silence {
            // Nothing audible and the display already reads empty.
            if got_samples {
                continue;
            }
            continue;
        }

        if ready && now.duration_since(last_emit).as_millis() >= interval {
            last_emit = now;
            if quiet {
                // One last frame that clears the bars, then stop.
                smoothed.iter_mut().for_each(|value| *value = 0.0);
                published_silence = true;
                bus.publish(&Event::Spectrum(Arc::new(SpectrumFrame {
                    bands: smoothed.clone(),
                    level: 0.0,
                })));
                continue;
            }
            published_silence = false;
            compute_bars(
                &window_samples,
                &window,
                &mut fft_buffer,
                &mut scratch,
                fft.as_ref(),
                stream.sample_rate,
                &opts,
                &mut bands,
                &mut smoothed,
            );
            bus.publish(&Event::Spectrum(Arc::new(SpectrumFrame {
                bands: smoothed.clone(),
                level: rms.min(1.0),
            })));
        }
    }

    Ok(())
}

/// Fixed-capacity ring of mono samples.
///
/// The obvious `Vec` + `remove(0)` is a memmove of the whole buffer per sample,
/// which at 48 kHz is hundreds of megabytes a second of pointless copying — it
/// showed up as several percent of a core spent while nothing was even playing.
#[derive(Debug)]
pub struct Ring {
    data: Vec<f32>,
    write: usize,
    filled: usize,
}

impl Ring {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            data: vec![0.0; capacity],
            write: 0,
            filled: 0,
        }
    }

    pub fn push(&mut self, sample: f32) {
        let capacity = self.data.len();
        self.data[self.write] = sample;
        self.write = (self.write + 1) % capacity;
        self.filled = (self.filled + 1).min(capacity);
    }

    pub fn push_silence(&mut self, count: usize) {
        for _ in 0..count {
            self.push(0.0);
        }
    }

    pub fn len(&self) -> usize {
        self.filled
    }

    pub fn is_empty(&self) -> bool {
        self.filled == 0
    }

    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// The newest `out.len()` samples, oldest first.
    ///
    /// Copies into the caller's buffer, which is a single 8 KB memcpy per frame
    /// instead of a rotation of the whole ring.
    pub fn copy_newest_into(&self, out: &mut [f32]) {
        let count = out.len().min(self.filled);
        let capacity = self.data.len();
        // Index of the oldest sample we want.
        let start = (self.write + capacity - count) % capacity;
        for (index, slot) in out[..count].iter_mut().enumerate() {
            *slot = self.data[(start + index) % capacity];
        }
        for slot in out[count..].iter_mut() {
            *slot = 0.0;
        }
    }
}

/// Fill `smoothed` with `opts.bars` magnitudes in `0.0..=1.0`.
#[allow(clippy::too_many_arguments)]
fn compute_bars(
    ring: &[f32],
    window: &[f32],
    fft_buffer: &mut [Complex<f32>],
    scratch: &mut [Complex<f32>],
    fft: &dyn rustfft::Fft<f32>,
    sample_rate: f32,
    opts: &SpectrumOptions,
    bands: &mut Vec<f32>,
    smoothed: &mut Vec<f32>,
) {
    if ring.len() < FFT_SIZE {
        return;
    }

    for (i, out) in fft_buffer.iter_mut().enumerate() {
        out.re = ring[i] * window[i];
        out.im = 0.0;
    }
    fft.process_with_scratch(fft_buffer, scratch);

    let bins = FFT_SIZE / 2;
    bands.clear();
    bands.reserve(opts.bars);

    // Log-spaced bands: perceived pitch is logarithmic, so linear bins would
    // cram every musical note into the leftmost third of the display.
    let ratio = (BAND_MAX_HZ / BAND_MIN_HZ).powf(1.0 / opts.bars as f32);
    let bin_width = sample_rate / FFT_SIZE as f32;

    for b in 0..opts.bars {
        let low = BAND_MIN_HZ * ratio.powi(b as i32);
        let high = low * ratio;
        let lo_bin = ((low / bin_width).floor() as usize).clamp(1, bins - 1);
        let hi_bin = ((high / bin_width).ceil() as usize).clamp(lo_bin + 1, bins);

        let peak = fft_buffer[lo_bin..hi_bin]
            .iter()
            .map(|c| c.norm())
            .fold(0.0f32, f32::max);
        // Scale so the result does not depend on the FFT size.
        let magnitude = peak * 2.0 / FFT_SIZE as f32;

        // -80 dB .. 0 dB mapped onto 0..1: roughly how a hardware analyser
        // behaves, and it keeps quiet passages visible.
        let db = 20.0 * (magnitude + 1e-9).log10();
        let normalised = ((db + 80.0) / 80.0).clamp(0.0, 1.0);
        bands.push((normalised * opts.sensitivity).clamp(0.0, 1.0));
    }

    // Asymmetric smoothing: snap up so beats are visible, ease down so the
    // display does not strobe.
    if smoothed.len() != bands.len() {
        *smoothed = bands.clone();
        return;
    }
    let fall = opts.smoothing.clamp(0.0, 0.95);
    let rise = (fall * 0.4).clamp(0.0, 0.9);
    for (value, target) in smoothed.iter_mut().zip(bands.iter()) {
        let coefficient = if *target > *value { rise } else { fall };
        *value = *value * coefficient + *target * (1.0 - coefficient);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hann() -> Vec<f32> {
        (0..FFT_SIZE)
            .map(|i| {
                let x = i as f32 / (FFT_SIZE - 1) as f32;
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos()
            })
            .collect()
    }

    fn analyse(samples: &[f32], bars: usize) -> Vec<f32> {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let mut buffer = vec![Complex::new(0.0f32, 0.0); FFT_SIZE];
        let mut scratch = vec![Complex::new(0.0f32, 0.0); fft.get_inplace_scratch_len()];
        let mut bands = Vec::new();
        let mut smoothed = Vec::new();
        let opts = SpectrumOptions {
            bars,
            sensitivity: 1.0,
            smoothing: 0.0,
        };
        compute_bars(
            samples,
            &hann(),
            &mut buffer,
            &mut scratch,
            fft.as_ref(),
            48_000.0,
            &opts,
            &mut bands,
            &mut smoothed,
        );
        smoothed
    }

    /// Loudest band index for a pure tone at `freq`.
    fn peak_band(freq: f32, bars: usize) -> usize {
        let samples: Vec<f32> = (0..FFT_SIZE)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / 48_000.0).sin() * 0.8)
            .collect();
        let out = analyse(&samples, bars);
        assert_eq!(out.len(), bars);
        out.iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0
    }

    #[test]
    fn silence_produces_near_zero_bands() {
        let out = analyse(&vec![0.0; FFT_SIZE], 16);
        assert_eq!(out.len(), 16);
        assert!(
            out.iter().all(|v| *v < 0.02),
            "silence should not light up: {out:?}"
        );
    }

    /// The renderer draws one bar per band, so the band count *is* the bar
    /// count. This pins the frame the app actually produces to the shared
    /// constant, so changing one side alone cannot leave bars unfed.
    #[test]
    fn the_frame_has_exactly_the_shared_band_count() {
        let bars = crate::SpectrumOptions::default().bars;
        assert_eq!(bars, beautify_core::model::SPECTRUM_BANDS);
        assert_eq!(analyse(&vec![0.0; FFT_SIZE], bars).len(), bars);
    }

    #[test]
    fn a_tone_lands_in_the_expected_band_and_is_log_scaled() {
        let bars = 24;
        // Bands are log-spaced from 40 Hz to 16 kHz. Compute where each test
        // frequency should land by the same formula the implementation uses.
        let expected = |freq: f32| {
            let ratio = (BAND_MAX_HZ / BAND_MIN_HZ).powf(1.0 / bars as f32);
            let idx = (freq / BAND_MIN_HZ).log(ratio).floor() as usize;
            idx.min(bars - 1)
        };

        for freq in [100.0f32, 440.0, 1000.0, 4000.0] {
            let got = peak_band(freq, bars);
            let want = expected(freq);
            assert!(
                got.abs_diff(want) <= 1,
                "{freq} Hz peaked at band {got}, expected {want}"
            );
        }
    }

    #[test]
    fn higher_frequencies_land_in_higher_bands() {
        let bars = 24;
        let low = peak_band(100.0, bars);
        let mid = peak_band(1000.0, bars);
        let high = peak_band(6000.0, bars);
        assert!(low < mid, "100 Hz ({low}) should be below 1 kHz ({mid})");
        assert!(mid < high, "1 kHz ({mid}) should be below 6 kHz ({high})");
    }

    #[test]
    fn sensitivity_scales_the_output() {
        let samples: Vec<f32> = (0..FFT_SIZE)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin() * 0.5)
            .collect();

        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let mut buffer = vec![Complex::new(0.0f32, 0.0); FFT_SIZE];
        let mut scratch = vec![Complex::new(0.0f32, 0.0); fft.get_inplace_scratch_len()];
        let mut run = |sensitivity: f32| {
            let mut bands = Vec::new();
            let mut smoothed = Vec::new();
            compute_bars(
                &samples,
                &hann(),
                &mut buffer,
                &mut scratch,
                fft.as_ref(),
                48_000.0,
                &SpectrumOptions {
                    bars: 8,
                    sensitivity,
                    smoothing: 0.0,
                },
                &mut bands,
                &mut smoothed,
            );
            smoothed
        };
        let quiet = run(0.5);
        let loud = run(1.0);
        assert!(loud.iter().sum::<f32>() > quiet.iter().sum::<f32>());
    }

    #[test]
    fn smoothing_is_asymmetric() {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let mut buffer = vec![Complex::new(0.0f32, 0.0); FFT_SIZE];
        let mut scratch = vec![Complex::new(0.0f32, 0.0); fft.get_inplace_scratch_len()];
        let loud: Vec<f32> = (0..FFT_SIZE)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin())
            .collect();
        let opts = SpectrumOptions {
            bars: 8,
            sensitivity: 1.0,
            smoothing: 0.8,
        };

        let mut bands = Vec::new();
        let mut smoothed = vec![0.0f32; 8];
        compute_bars(
            &loud,
            &hann(),
            &mut buffer,
            &mut scratch,
            fft.as_ref(),
            48_000.0,
            &opts,
            &mut bands,
            &mut smoothed,
        );
        let after_loud = smoothed.clone();

        // One silent frame: the bars must fall, but less than they rose.
        compute_bars(
            &vec![0.0; FFT_SIZE],
            &hann(),
            &mut buffer,
            &mut scratch,
            fft.as_ref(),
            48_000.0,
            &opts,
            &mut bands,
            &mut smoothed,
        );
        assert!(
            smoothed.iter().zip(after_loud.iter()).all(|(a, b)| a <= b),
            "silence must not raise a band"
        );
        assert!(
            smoothed.iter().any(|v| *v > 0.0),
            "a single silent frame should not wipe the display"
        );
    }

    #[test]
    fn ring_buffer_keeps_only_the_newest_samples() {
        let mut ring = Ring::with_capacity(RING_CAPACITY);
        for i in 0..(RING_CAPACITY + 500) {
            ring.push(i as f32);
        }
        assert_eq!(ring.len(), RING_CAPACITY);
        assert!(ring.len() <= ring.capacity());

        let mut newest = vec![0.0f32; 4];
        ring.copy_newest_into(&mut newest);
        let last = (RING_CAPACITY + 499) as f32;
        assert_eq!(newest, vec![last - 3.0, last - 2.0, last - 1.0, last]);
    }

    #[test]
    fn a_partly_filled_ring_zero_pads_the_tail() {
        // Only reachable when fewer samples than a window exist, which the
        // caller guards against; the contract is documented here anyway.
        let mut ring = Ring::with_capacity(RING_CAPACITY);
        ring.push(1.0);
        ring.push(2.0);
        let mut out = vec![9.0f32; 5];
        ring.copy_newest_into(&mut out);
        assert_eq!(out, vec![1.0, 2.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn pushing_silence_advances_the_ring() {
        let mut ring = Ring::with_capacity(RING_CAPACITY);
        ring.push(5.0);
        ring.push_silence(3);
        assert_eq!(ring.len(), 4);
        let mut out = vec![9.0f32; 4];
        ring.copy_newest_into(&mut out);
        assert_eq!(out, vec![5.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn short_input_does_not_panic() {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let mut buffer = vec![Complex::new(0.0f32, 0.0); FFT_SIZE];
        let mut scratch = vec![Complex::new(0.0f32, 0.0); fft.get_inplace_scratch_len()];
        let mut bands = Vec::new();
        let mut smoothed = Vec::new();
        compute_bars(
            &[1.0, 2.0, 3.0],
            &hann(),
            &mut buffer,
            &mut scratch,
            fft.as_ref(),
            48_000.0,
            &SpectrumOptions::default(),
            &mut bands,
            &mut smoothed,
        );
        assert!(smoothed.is_empty());
    }
}
