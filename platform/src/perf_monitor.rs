//! Main-thread frame monitor feeding the PerfGraph widget (widgets crate).
//!
//! A fixed ring of per-frame samples: the paint-to-paint gap (frame pacing —
//! spikes here are the hiccups the user sees) plus per-channel CPU times.
//! Built-in channels cover the platform (event dispatch, script GC, pass
//! encode, drawable wait); apps register their own channels for anything
//! else they want plotted (physics, script tick, audio…):
//!
//! ```ignore
//! let ch = cx.perf_monitor_channel("physics", 0x6aa9ff);
//! // ...
//! cx.perf_monitor_add(ch, t0.elapsed().as_micros() as u64);
//! ```
//!
//! Collection is off until something (normally the PerfGraph widget) calls
//! `set_enabled(true)`; disabled adds are a single branch.

use crate::makepad_micro_serde::*;
use std::{cell::Cell, rc::Rc};

pub const PERF_MONITOR_HISTORY: usize = 240;
pub const PERF_MONITOR_MAX_CHANNELS: usize = 12;

/// GPU completion handlers run off-thread; they park each presented frame's
/// GPU time here and the next `frame_boundary` folds it into the ring.
static GPU_ACCUM_US: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static GPU_COLLECT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Call from a GPU command-buffer completion handler (any thread) with one
/// presented frame's GPU interval in seconds.
pub fn perf_gpu_frame_completed(seconds: f64) {
    use std::sync::atomic::Ordering;
    if GPU_COLLECT.load(Ordering::Relaxed) && seconds.is_finite() && seconds > 0.0 {
        GPU_ACCUM_US.fetch_add((seconds * 1e6) as u32, Ordering::Relaxed);
    }
}

/// Index of a registered channel; hand out once, add to it every frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PerfChannel(pub usize);

/// Built-in channels, registered in `PerfMonitor::default()`.
pub const PERF_CHANNEL_EVENT: PerfChannel = PerfChannel(0);
/// Splash/script VM execution — embedders add their eval/call time here.
pub const PERF_CHANNEL_SCRIPT: PerfChannel = PerfChannel(1);
pub const PERF_CHANNEL_GC: PerfChannel = PerfChannel(2);
pub const PERF_CHANNEL_DRAW: PerfChannel = PerfChannel(3);
pub const PERF_CHANNEL_DRAWABLE_WAIT: PerfChannel = PerfChannel(4);
/// GPU time of a presented frame (command-buffer start→end, completion
/// handler thread). Runs CONCURRENT with the CPU channels — read it as its
/// own series, not as part of the main-thread total.
pub const PERF_CHANNEL_GPU: PerfChannel = PerfChannel(5);

#[derive(Clone, Copy, Default)]
pub struct PerfMonitorFrame {
    /// Time between this paint and the previous one, milliseconds.
    pub gap_ms: f32,
    /// Per-channel CPU time this frame, microseconds.
    pub channel_us: [u32; PERF_MONITOR_MAX_CHANNELS],
    /// The CPU time the main thread used this frame, zero where the platform doesn't measure it.
    pub thread_cpu_us: u32,
}

struct PerfPresent {
    interval_ms: f32,
    missed_vsyncs: u32,
}

/// The count, mean, median, 95th percentile and max of one series, in milliseconds.
#[derive(Clone, Copy, Debug, Default, SerJson)]
pub struct PerfStats {
    pub count: u32,
    pub avg: f32,
    pub p50: f32,
    pub p95: f32,
    pub max: f32,
}

impl PerfStats {
    fn from_samples(mut samples: Vec<f32>) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        samples.sort_by(f32::total_cmp);
        let round_to_us = |ms: f32| (ms * 1000.0).round() / 1000.0;
        let percentile = |percent: usize| round_to_us(samples[(samples.len() - 1) * percent / 100]);
        Self {
            count: samples.len() as u32,
            avg: round_to_us(samples.iter().sum::<f32>() / samples.len() as f32),
            p50: percentile(50),
            p95: percentile(95),
            max: percentile(100),
        }
    }
}

/// Frame stats over the frames recorded since the last [`PerfMonitor::take_summary`] call.
#[derive(Clone, Copy, Debug, Default, SerJson)]
pub struct PerfSummary {
    /// The time those frames span, which leaves out any idle time after the last one.
    pub seconds: f32,
    /// The time between frames, including any idle time between them.
    pub frame_ms: PerfStats,
    pub thread_cpu_ms: PerfStats,
    pub event_ms: PerfStats,
    pub draw_ms: PerfStats,
    pub wait_ms: PerfStats,
    pub gc_ms: PerfStats,
    pub gpu_ms: PerfStats,
    /// The time between frames reaching the display, for frames we drew back to back.
    pub present_ms: PerfStats,
    pub missed_vsyncs: u32,
}

#[derive(Clone)]
pub struct PerfChannelInfo {
    pub name: String,
    /// 0xRRGGBB plot color hint.
    pub color: u32,
}

pub struct PerfMonitor {
    enabled: bool,
    channels: Vec<PerfChannelInfo>,
    ring: Vec<PerfMonitorFrame>,
    at: usize,
    cur: PerfMonitorFrame,
    last_frame_time: Option<f64>,
    /// Active event-dispatch depth, used to attribute nested channel timing.
    pub(crate) event_depth: Rc<Cell<u32>>,
    /// Time app channels attributed while inside an event dispatch; deducted
    /// from the "event" channel so the stacked plot doesn't double-count.
    event_deduct: u32,
    /// Window repaints seen since enabling (see `frames_painted`).
    frames_painted: u64,
    frames_since_summary: usize,
    presents: Vec<PerfPresent>,
}

impl Default for PerfMonitor {
    fn default() -> Self {
        Self {
            enabled: false,
            channels: vec![
                PerfChannelInfo { name: "event".into(), color: 0x4fd06a },
                PerfChannelInfo { name: "script".into(), color: 0x58b6ff },
                PerfChannelInfo { name: "gc".into(), color: 0xd0c24f },
                PerfChannelInfo { name: "draw".into(), color: 0xff9a4f },
                PerfChannelInfo { name: "wait".into(), color: 0xe05555 },
                PerfChannelInfo { name: "gpu".into(), color: 0xb08cff },
            ],
            ring: Vec::new(),
            at: 0,
            cur: Default::default(),
            last_frame_time: None,
            event_depth: Rc::new(Cell::new(0)),
            event_deduct: 0,
            frames_painted: 0,
            frames_since_summary: 0,
            presents: Vec::new(),
        }
    }
}

impl PerfMonitor {
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        GPU_COLLECT.store(on, std::sync::atomic::Ordering::Relaxed);
        if !on {
            self.last_frame_time = None;
            self.cur = Default::default();
            self.event_deduct = 0;
            self.frames_since_summary = 0;
            self.presents.clear();
            GPU_ACCUM_US.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Number of window repaints (frame boundaries) recorded while enabled.
    /// Lets a scripted driver pace itself to presented frames instead of
    /// queueing pass renders faster than the GPU retires them.
    pub fn frames_painted(&self) -> u64 {
        self.frames_painted
    }

    /// Register (or find by name) an app channel. Indexes are stable for the
    /// life of the process; past MAX_CHANNELS you share the last slot.
    pub fn channel(&mut self, name: &str, color: u32) -> PerfChannel {
        if let Some(index) = self.channels.iter().position(|c| c.name == name) {
            return PerfChannel(index);
        }
        if self.channels.len() >= PERF_MONITOR_MAX_CHANNELS {
            return PerfChannel(PERF_MONITOR_MAX_CHANNELS - 1);
        }
        self.channels.push(PerfChannelInfo { name: name.into(), color });
        PerfChannel(self.channels.len() - 1)
    }

    pub fn channels(&self) -> &[PerfChannelInfo] {
        &self.channels
    }

    pub fn add(&mut self, channel: PerfChannel, us: u64) {
        if !self.enabled {
            return;
        }
        let mut us = us as u32;
        if channel == PERF_CHANNEL_EVENT {
            // "event" is what's left of the dispatch after the app attributed
            // its own channels (script, physics, …) inside it.
            us = us.saturating_sub(self.event_deduct);
            self.event_deduct = 0;
        } else if self.event_depth.get() > 0 {
            self.event_deduct = self.event_deduct.saturating_add(us);
        }
        let slot = &mut self.cur.channel_us[channel.0.min(PERF_MONITOR_MAX_CHANNELS - 1)];
        *slot = slot.saturating_add(us);
    }

    pub fn add_thread_cpu_time(&mut self, us: u64) {
        if self.enabled {
            self.cur.thread_cpu_us = self.cur.thread_cpu_us.saturating_add(us as u32);
        }
    }

    /// Records the time between two frames reaching the display, and how many
    /// vsyncs went by without a new frame in between.
    pub fn add_present(&mut self, interval_ms: f32, missed_vsyncs: u32) {
        if !self.enabled {
            return;
        }
        if self.presents.len() == PERF_MONITOR_HISTORY {
            self.presents.remove(0);
        }
        self.presents.push(PerfPresent { interval_ms, missed_vsyncs });
    }

    /// Close the frame being accumulated and start the next. Called by the
    /// platform at the start of every window repaint.
    pub fn frame_boundary(&mut self, time: f64) {
        if !self.enabled {
            return;
        }
        self.frames_painted += 1;
        if self.ring.is_empty() {
            self.ring.resize(PERF_MONITOR_HISTORY, Default::default());
        }
        // Fold in GPU time completed since the last boundary (one frame late
        // by construction — fine for a monitor).
        let gpu_us = GPU_ACCUM_US.swap(0, std::sync::atomic::Ordering::Relaxed);
        if gpu_us > 0 {
            let slot = &mut self.cur.channel_us[PERF_CHANNEL_GPU.0];
            *slot = slot.saturating_add(gpu_us);
        }
        if let Some(last) = self.last_frame_time {
            self.cur.gap_ms = ((time - last) * 1000.0) as f32;
            self.ring[self.at] = self.cur;
            self.at = (self.at + 1) % PERF_MONITOR_HISTORY;
            self.frames_since_summary += 1;
        }
        self.last_frame_time = Some(time);
        self.cur = Default::default();
    }

    /// Copy the history oldest→newest. Empty until enabled + first frames.
    pub fn read(&self, out: &mut Vec<PerfMonitorFrame>) {
        out.clear();
        if self.ring.is_empty() {
            return;
        }
        for i in 0..PERF_MONITOR_HISTORY {
            out.push(self.ring[(self.at + i) % PERF_MONITOR_HISTORY]);
        }
    }

    /// Returns stats for the frames recorded since the last call, so calling it once a second
    /// gives a per-second summary. It only sees the newest `PERF_MONITOR_HISTORY` frames.
    pub fn take_summary(&mut self) -> PerfSummary {
        let count = std::mem::take(&mut self.frames_since_summary).min(self.ring.len());
        let frames: Vec<PerfMonitorFrame> = (self.ring.len() - count..self.ring.len())
            .map(|i| self.ring[(self.at + i) % PERF_MONITOR_HISTORY])
            .collect();
        let stats = |ms: fn(&PerfMonitorFrame) -> f32| {
            PerfStats::from_samples(frames.iter().map(ms).collect())
        };
        // A zero here means the frame has no sample, like a GPU time that hasn't come back yet.
        let nonzero_stats = |ms: fn(&PerfMonitorFrame) -> f32| {
            PerfStats::from_samples(frames.iter().map(ms).filter(|ms| *ms > 0.0).collect())
        };
        let presents = std::mem::take(&mut self.presents);
        PerfSummary {
            seconds: frames.iter().map(|frame| frame.gap_ms).sum::<f32>() / 1000.0,
            frame_ms: stats(|frame| frame.gap_ms),
            thread_cpu_ms: nonzero_stats(|frame| frame.thread_cpu_us as f32 / 1000.0),
            event_ms: stats(|frame| frame.channel_us[PERF_CHANNEL_EVENT.0] as f32 / 1000.0),
            draw_ms: stats(|frame| frame.channel_us[PERF_CHANNEL_DRAW.0] as f32 / 1000.0),
            wait_ms: stats(|frame| frame.channel_us[PERF_CHANNEL_DRAWABLE_WAIT.0] as f32 / 1000.0),
            gc_ms: stats(|frame| frame.channel_us[PERF_CHANNEL_GC.0] as f32 / 1000.0),
            gpu_ms: nonzero_stats(|frame| frame.channel_us[PERF_CHANNEL_GPU.0] as f32 / 1000.0),
            present_ms: PerfStats::from_samples(presents.iter().map(|present| present.interval_ms).collect()),
            missed_vsyncs: presents.iter().map(|present| present.missed_vsyncs).sum(),
        }
    }
}
