//! Two-tier fan-out.
//!
//! ```text
//!   engine  --1 pull per content id-->  EngineStream
//!                                          |  raw MPEG-TS
//!                    +---------------------+---------------------+
//!                    v                                           v
//!            Encoder (adts)   <-- 1 ffmpeg per format -->  Encoder (fmp4)
//!                    |                                           |
//!            +-------+-------+                           +-------+
//!            v               v                           v
//!        listener        listener                     listener
//! ```
//!
//! Lifetime is refcounted downward: a `Subscription` holds `Arc<Encoder>`, an
//! `Encoder` holds `Arc<EngineStream>`, and the registry holds only `Weak`s. So
//! the last listener leaving an encoder tears down its ffmpeg, and the last
//! encoder leaving a content id closes the engine connection.
//!
//! # Locking rule
//!
//! Never drop an `Arc<Encoder>` while holding `EngineStream::encoders`.
//! `Encoder::drop` takes that same lock to deregister itself, so doing this
//! self-deadlocks. Use [`EngineStream::live_encoders`], which upgrades under the
//! lock and returns after releasing it.
//!
//! Where `encoders` and `replay` are both needed, `encoders` is taken first
//! (see `start_encoder`). The puller never holds them together — it appends to
//! `replay` and releases it before touching `encoders` — so that ordering is
//! the only one in play and no cycle exists.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use crate::engine::{self, log, Session};
use crate::format::{self, OutputFormat, Plan};
use crate::mp4;
use crate::probe::Probe;

/// Read size for the engine pull and for ffmpeg output. A pipe read returns as
/// soon as any data is there, so this is an upper bound, not a batching delay.
const CHUNK: usize = 64 * 1024;
/// Bytes buffered from the engine before probing. Sized to match ffprobe's
/// `-probesize`.
const PREROLL: usize = 256 * 1024;
/// Per-encoder raw-TS backlog. Bounded deliberately small: the response to a
/// full queue is backpressure onto the engine socket, not buffering.
const TS_QUEUE: usize = 64;
/// How long an encoder may fail to accept raw TS before it is torn down.
///
/// Must be generous. An AceStream engine prebuffers tens of megabytes and then
/// releases the lot at link speed, so at stream start the queue fills instantly
/// through no fault of the encoder. Blocking the puller is the correct response:
/// it stops reading, TCP flow control throttles the engine, and the burst drains
/// at the rate the encoders can actually consume.
const TS_STALL: Duration = Duration::from_secs(20);
/// Per-listener output backlog. Overflow evicts the listener; see `fan_out`.
const SUB_QUEUE: usize = 64;
const STATS_INTERVAL: Duration = Duration::from_secs(5);
/// How long a request will wait for a cold stream to connect and probe.
const START_TIMEOUT: Duration = Duration::from_secs(45);
/// How long an fMP4 joiner waits for the first fragment. A fragment closes on a
/// keyframe, so this must comfortably exceed the source GOP.
const INIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Rolling window of raw TS replayed into each newly started encoder.
///
/// An encoder otherwise attaches at an arbitrary mid-GOP point, where ffmpeg
/// has to wait for the next keyframe to recover H.264 SPS/PPS — measured on a
/// real 720p broadcast stream that took 14s in one trial and simply never
/// succeeded in another. Replaying a window wider than one GOP puts a keyframe
/// in ffmpeg's first bytes instead, so startup stops depending on where we
/// happened to join.
///
/// Sized by bytes rather than time because we cannot know the bitrate up front:
/// 4MB is ~6s at 5 Mbps and ~2s even at 16 Mbps, comfortably over a broadcast
/// GOP either way. Deliberately not parsed for TS random-access flags — ffmpeg
/// sets those far more often than it emits keyframes, so they would not mean
/// what the name suggests.
///
/// The cost is freshness: a viewer starts up to one window behind live. For
/// fMP4 that sits alongside an existing fragment-duration floor of the same
/// order.
const REPLAY_BYTES: usize = 4 * 1024 * 1024;

fn now_pid() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("proxy-{nanos:x}")
}

#[derive(Debug, Clone)]
enum Start {
    Pending,
    Ready,
    Failed(String),
}

pub struct Config {
    pub engine_host: String,
    pub frag_duration_ms: format::FragDurationMs,
}

// ---------------------------------------------------------------------------
// Tier 1: one engine pull per content id
// ---------------------------------------------------------------------------

pub struct EngineStream {
    pub content_id: String,
    pub started: Instant,
    pub bytes_in: AtomicU64,
    session: OnceLock<Session>,
    probe: OnceLock<Probe>,
    /// A clone of the pull socket, kept solely so teardown can interrupt a
    /// puller blocked in a 30s read instead of leaving the engine connection
    /// open until it times out.
    socket: OnceLock<std::net::TcpStream>,
    stats: Mutex<Option<(Instant, serde_json::Value)>>,
    /// Trailing raw TS, replayed into each new encoder. See [`REPLAY_BYTES`].
    replay: Mutex<VecDeque<u8>>,
    /// Sender side lives here alone: dropping the entry closes the channel,
    /// which is how a wedged encoder gets torn down.
    encoders: Mutex<HashMap<OutputFormat, (Weak<Encoder>, SyncSender<Bytes>)>>,
    /// Serialises get-or-create so two simultaneous requests for the same
    /// format cannot both spawn an ffmpeg. Held only by would-be creators, so
    /// the puller is never blocked behind a process spawn.
    creating: Mutex<()>,
    start: (Mutex<Start>, Condvar),
    config: Arc<Config>,
}

impl EngineStream {
    pub fn probe(&self) -> Option<&Probe> {
        self.probe.get()
    }

    pub fn stats(&self) -> Option<serde_json::Value> {
        self.stats.lock().unwrap().as_ref().map(|(_, v)| v.clone())
    }

    fn stat_url(&self) -> Option<String> {
        self.session.get().and_then(|s| s.stat_url.clone())
    }

    fn remember(&self, chunk: &[u8]) {
        let mut replay = self.replay.lock().unwrap();
        replay.extend(chunk.iter().copied());
        if replay.len() > REPLAY_BYTES {
            let excess = replay.len() - REPLAY_BYTES;
            replay.drain(..excess);
        }
    }

    /// The replay window, trimmed to start on a TS packet boundary.
    ///
    /// Handing ffmpeg a partial packet only costs it a resync, but starting
    /// clean keeps the "could not find codec parameters" failure mode off the
    /// table for the sake of a few bytes.
    fn replay_window(&self) -> Vec<u8> {
        let replay = self.replay.lock().unwrap();
        let bytes: Vec<u8> = replay.iter().copied().collect();
        drop(replay);

        // A sync byte is only convincing if the next packet has one too.
        const TS_PACKET: usize = 188;
        let start = (0..bytes.len().min(TS_PACKET)).find(|&i| {
            bytes[i] == 0x47
                && bytes
                    .get(i + TS_PACKET)
                    .is_none_or(|&b| b == 0x47)
        });
        match start {
            Some(i) => bytes[i..].to_vec(),
            None => bytes,
        }
    }

    /// Upgrade every live encoder, releasing the lock before the returned
    /// `Arc`s can drop. See the locking rule in the module docs.
    pub fn live_encoders(&self) -> Vec<Arc<Encoder>> {
        let map = self.encoders.lock().unwrap();
        let live: Vec<_> = map.values().filter_map(|(w, _)| w.upgrade()).collect();
        drop(map);
        live
    }

    fn wait_ready(&self) -> Result<(), String> {
        let (lock, cv) = &self.start;
        let mut state = lock.lock().unwrap();
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            match &*state {
                Start::Ready => return Ok(()),
                Start::Failed(e) => return Err(e.clone()),
                Start::Pending => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("timed out waiting for the engine stream to start".into());
            }
            let (s, _) = cv.wait_timeout(state, left).unwrap();
            state = s;
        }
    }

    fn publish(&self, outcome: Result<(), String>) {
        let (lock, cv) = &self.start;
        let mut state = lock.lock().unwrap();
        *state = match outcome {
            Ok(()) => Start::Ready,
            Err(e) => Start::Failed(e),
        };
        cv.notify_all();
    }
}

impl Drop for EngineStream {
    fn drop(&mut self) {
        // Unblock the puller immediately rather than waiting out its read
        // timeout; it will see the closed socket, stop the engine session and
        // exit.
        if let Some(s) = self.socket.get() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        log::info(&format!("{}: engine stream closed", self.content_id));
    }
}

/// The puller: one thread per content id, holding the sole engine connection.
fn pull(stream: Weak<EngineStream>, content_id: String, config: Arc<Config>) {
    let pid = now_pid();
    let session = match engine::open_session(&config.engine_host, &content_id, &pid) {
        Ok(s) => s,
        Err(e) => {
            if let Some(s) = stream.upgrade() {
                s.publish(Err(e));
            }
            return;
        }
    };
    // Kept on the thread so teardown can stop the engine session even as the
    // EngineStream is being dropped out from under us.
    let command_url = session.command_url.clone();
    let stop_engine = || {
        if let Some(url) = &command_url {
            engine::send_stop(url);
        }
    };

    let mut resp = match engine::get(&session.playback_url, engine::STALL_TIMEOUT) {
        Ok(r) if r.status == 200 => r,
        Ok(r) => {
            if let Some(s) = stream.upgrade() {
                s.publish(Err(format!("engine returned http {}", r.status)));
            }
            stop_engine();
            return;
        }
        Err(e) => {
            if let Some(s) = stream.upgrade() {
                s.publish(Err(format!("could not open engine stream: {e}")));
            }
            stop_engine();
            return;
        }
    };

    // Register the socket before the first blocking read, so a teardown during
    // startup interrupts the puller rather than waiting out the stall timeout.
    match stream.upgrade() {
        Some(s) => {
            if let Ok(sock) = resp.socket.try_clone() {
                let _ = s.socket.set(sock);
            }
        }
        None => {
            stop_engine();
            return;
        }
    }

    // Buffer a preroll and probe it. This is the only probe: every format and
    // every later listener reuses the result, so it costs one engine read
    // window per content id rather than one per request.
    let mut preroll = vec![0u8; PREROLL];
    let mut filled = 0usize;
    while filled < PREROLL {
        match resp.body.read(&mut preroll[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => {
                if let Some(s) = stream.upgrade() {
                    s.publish(Err(format!("engine stream failed during preroll: {e}")));
                }
                stop_engine();
                return;
            }
        }
    }
    preroll.truncate(filled);

    let probed = match crate::probe::run(&preroll) {
        Ok(p) => p,
        Err(e) => {
            if let Some(s) = stream.upgrade() {
                s.publish(Err(format!("could not probe stream: {e}")));
            }
            stop_engine();
            return;
        }
    };

    {
        let Some(s) = stream.upgrade() else {
            stop_engine();
            return;
        };
        let _ = s.session.set(session);
        log::info(&format!(
            "{}: engine stream up, video={} audio={}",
            content_id,
            probed.video.as_deref().unwrap_or("none"),
            probed.audio.as_deref().unwrap_or("none"),
        ));
        let _ = s.probe.set(probed);
        s.publish(Ok(()));
    }

    // The preroll is deliberately not replayed to encoders: it is already stale
    // by the time the first one starts, and injecting old timestamps upsets the
    // MP4 muxer. TS resynchronises on the next PAT/PMT, typically within 100ms.
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match resp.body.read(&mut buf) {
            Ok(0) => {
                log::info(&format!("{content_id}: engine closed the stream"));
                break;
            }
            Ok(n) => n,
            Err(e) => {
                // A read timeout here is the stall guard that -rw_timeout used
                // to provide inside ffmpeg. But teardown also breaks this read,
                // by design — EngineStream::drop shuts the socket down so the
                // puller stops promptly — and that is not a failure worth
                // reporting. If the stream is already gone, this is that.
                if stream.upgrade().is_some() {
                    log::warn(&format!("{content_id}: engine read failed: {e}"));
                }
                break;
            }
        };

        // Liveness is the Arc, never the encoder map: the map is legitimately
        // empty between `wait_ready` returning and the first encoder
        // registering, and exiting on that would kill every cold start.
        let Some(s) = stream.upgrade() else { break };
        s.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
        s.remember(&buf[..n]);
        let chunk = Bytes::copy_from_slice(&buf[..n]);

        // Snapshot the senders so the lock is not held across a blocking send.
        // Never upgrade the Weak<Encoder> here either: dropping the last Arc
        // under this lock would re-enter Encoder::drop and deadlock.
        let targets: Vec<(OutputFormat, SyncSender<Bytes>)> = {
            let map = s.encoders.lock().unwrap();
            map.iter().map(|(f, (_, tx))| (*f, tx.clone())).collect()
        };

        let mut wedged = Vec::new();
        for (fmt, tx) in &targets {
            if let Err(reason) = send_backpressured(tx, chunk.clone()) {
                wedged.push((*fmt, reason));
            }
        }

        if !wedged.is_empty() {
            let mut map = s.encoders.lock().unwrap();
            for (fmt, reason) in wedged {
                if map.remove(&fmt).is_some() && reason != "gone" {
                    log::error(&format!("{content_id}/{fmt}: encoder {reason}; torn down"));
                }
            }
        }
    }

    stop_engine();
}

/// Hand a chunk to one encoder, waiting for room rather than giving up.
///
/// This is the opposite policy from [`Encoder::fan_out`], deliberately. Evicting
/// a *listener* costs that listener alone and it can reconnect; dropping an
/// *encoder* kills every listener on it and restarts ffmpeg. So here the puller
/// blocks — which is also the only way to signal the engine to slow down — and
/// only gives up after [`TS_STALL`].
fn send_backpressured(tx: &SyncSender<Bytes>, chunk: Bytes) -> Result<(), &'static str> {
    let deadline = Instant::now() + TS_STALL;
    let mut chunk = chunk;
    loop {
        match tx.try_send(chunk) {
            Ok(()) => return Ok(()),
            // ffmpeg exited and its feeder went with it.
            Err(TrySendError::Disconnected(_)) => return Err("gone"),
            Err(TrySendError::Full(returned)) => {
                if Instant::now() >= deadline {
                    return Err("could not keep up with the engine");
                }
                chunk = returned;
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tier 2: one ffmpeg per (content id, output format)
// ---------------------------------------------------------------------------

struct Listener {
    id: u64,
    tx: SyncSender<Bytes>,
    /// fMP4 only: true until this listener has been aligned to a `moof`.
    awaiting_fragment: bool,
    peer: String,
    joined: Instant,
    bytes: Arc<AtomicU64>,
}

pub struct Encoder {
    pub content_id: String,
    pub fmt: OutputFormat,
    pub started: Instant,
    pub bytes_out: AtomicU64,
    pub video: format::TrackMode,
    pub audio: format::TrackMode,
    /// Cached fMP4 init segment, replayed to every late joiner. Joiners block
    /// on `init_ready` until the first `moof` has been written.
    init: Mutex<Option<Bytes>>,
    init_ready: Condvar,
    /// Set if the fMP4 byte stream did not parse; late joiners are refused
    /// rather than served something they cannot decode.
    unjoinable: AtomicBool,
    /// Set when ffmpeg's output ends, so joiners stop waiting on an init
    /// segment that will never arrive.
    finished: AtomicBool,
    listeners: Mutex<Vec<Listener>>,
    next_id: AtomicU64,
    child: Mutex<Option<Child>>,
    engine: Arc<EngineStream>,
}

impl Encoder {
    #[cfg(test)]
    fn listener_count(&self) -> usize {
        self.listeners.lock().unwrap().len()
    }

    pub fn listener_info(&self) -> Vec<(String, u64, u64)> {
        self.listeners
            .lock()
            .unwrap()
            .iter()
            .map(|l| {
                (
                    l.peer.clone(),
                    l.joined.elapsed().as_secs(),
                    l.bytes.load(Ordering::Relaxed),
                )
            })
            .collect()
    }

    /// Takes `Arc<Self>` because the returned `Subscription` must keep the
    /// encoder alive — that refcount is what stops ffmpeg when the last
    /// listener leaves.
    fn subscribe(self: Arc<Self>, peer: String) -> Result<Subscription, String> {
        let init = if self.fmt.needs_init_segment() {
            Some(self.await_init_segment()?)
        } else {
            None
        };

        let (tx, rx) = sync_channel(SUB_QUEUE);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let bytes = Arc::new(AtomicU64::new(0));

        if let Some(init) = init {
            // Must land before any media, so a blocking send is correct: the
            // channel is empty and SUB_QUEUE deep.
            let _ = tx.send(init);
        }

        self.listeners.lock().unwrap().push(Listener {
            id,
            tx,
            awaiting_fragment: self.fmt.needs_init_segment(),
            peer,
            joined: Instant::now(),
            bytes: bytes.clone(),
        });
        Ok(Subscription {
            rx,
            leftover: Bytes::new(),
            bytes,
            id,
            encoder: self,
        })
    }

    fn remove_listener(&self, id: u64) {
        self.listeners.lock().unwrap().retain(|l| l.id != id);
    }

    /// Block until the first `moof` has been written, so an fMP4 joiner gets a
    /// complete init segment.
    ///
    /// The caller holds `Arc<Self>` throughout, which is what makes this
    /// correct: tearing down and retrying would restart ffmpeg each time and
    /// never converge, since the fragment clock restarts with it.
    fn await_init_segment(&self) -> Result<Bytes, String> {
        let mut init = self.init.lock().unwrap();
        let deadline = Instant::now() + INIT_TIMEOUT;
        loop {
            // Checked before the cached segment: once the box structure has
            // broken, fragment boundaries are no longer reliable, so a joiner
            // would take the init segment and then never attach.
            if self.unjoinable.load(Ordering::Relaxed) {
                return Err("stream is not valid fragmented mp4".into());
            }
            if let Some(i) = init.as_ref() {
                return Ok(i.clone());
            }
            if self.finished.load(Ordering::Relaxed) {
                return Err("encoder stopped before producing a fragment".into());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "no fragment within {}s; the source keyframe interval may exceed it",
                    INIT_TIMEOUT.as_secs()
                ));
            }
            let (g, _) = self.init_ready.wait_timeout(init, left).unwrap();
            init = g;
        }
    }

    /// Wake anything waiting on the init segment after a terminal state change.
    fn notify_joiners(&self) {
        let _guard = self.init.lock().unwrap();
        self.init_ready.notify_all();
    }

    /// Deliver one scanned piece to every listener.
    fn fan_out(&self, piece: &mp4::Piece) {
        let data = piece.data();
        let starts_fragment = piece.fragment_start();
        let mut listeners = self.listeners.lock().unwrap();
        listeners.retain_mut(|l| {
            if l.awaiting_fragment {
                if !starts_fragment {
                    // Mid-fragment bytes are useless to a client that has only
                    // had the init segment; hold it until the next moof.
                    return true;
                }
                l.awaiting_fragment = false;
            }
            match l.tx.try_send(data.clone()) {
                Ok(()) => {
                    l.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                    true
                }
                // A shared producer cannot block on its slowest consumer, and
                // for fMP4 a dropped chunk is unrecoverable. Evict instead;
                // players reconnect.
                Err(TrySendError::Full(_)) => {
                    log::warn(&format!(
                        "{}/{}: {} fell behind; disconnecting",
                        self.content_id, self.fmt, l.peer
                    ));
                    false
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // Deregister so the puller stops feeding this format and /status stops
        // reporting it. Safe here: nothing holds `encoders` while dropping an
        // Arc<Encoder> (see the module locking rule).
        //
        // Remove only our own entry: a replacement encoder may already have
        // taken this slot. Compared by pointer rather than by upgrading, since
        // dropping an upgraded Arc under this lock is exactly what the locking
        // rule forbids.
        let mut map = self.engine.encoders.lock().unwrap();
        let ours = map
            .get(&self.fmt)
            .is_some_and(|(w, _)| std::ptr::eq(w.as_ptr(), self as *const Encoder));
        if ours {
            map.remove(&self.fmt);
        }
        drop(map);
        log::info(&format!(
            "{}/{}: encoder stopped after {}s, {} bytes",
            self.content_id,
            self.fmt,
            self.started.elapsed().as_secs(),
            self.bytes_out.load(Ordering::Relaxed)
        ));
    }
}

// ---------------------------------------------------------------------------
// Tier 3: one listener per HTTP request
// ---------------------------------------------------------------------------

/// A listener's handle on an encoder. Dropping it deregisters the listener and,
/// if it was the last one, tears down the ffmpeg process.
pub struct Subscription {
    rx: Receiver<Bytes>,
    leftover: Bytes,
    bytes: Arc<AtomicU64>,
    id: u64,
    encoder: Arc<Encoder>,
}

impl Read for Subscription {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.leftover.is_empty() {
            match self.rx.recv() {
                Ok(b) => self.leftover = b,
                // Encoder gone: a clean end of response.
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.leftover.len());
        buf[..n].copy_from_slice(&self.leftover[..n]);
        self.leftover = self.leftover.slice(n..);
        Ok(n)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.encoder.remove_listener(self.id);
    }
}

impl Subscription {
    #[cfg(test)]
    fn bytes_sent(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// A handle on the byte counter that outlives the subscription being moved
    /// into a response body, so the request thread can still log a total.
    pub fn counter(&self) -> Arc<AtomicU64> {
        self.bytes.clone()
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct Registry {
    streams: Mutex<HashMap<String, Weak<EngineStream>>>,
    config: Arc<Config>,
}

impl Registry {
    pub fn new(config: Config) -> Arc<Self> {
        let reg = Arc::new(Registry {
            streams: Mutex::new(HashMap::new()),
            config: Arc::new(config),
        });
        spawn_stats_poller(Arc::downgrade(&reg));
        reg
    }

    pub fn live_streams(&self) -> Vec<Arc<EngineStream>> {
        let map = self.streams.lock().unwrap();
        let live: Vec<_> = map.values().filter_map(|w| w.upgrade()).collect();
        drop(map);
        live
    }

    /// Get or start the engine pull for `content_id`, blocking until it has
    /// connected and probed.
    fn engine_stream(&self, content_id: &str) -> Result<Arc<EngineStream>, String> {
        let (stream, fresh) = {
            let mut map = self.streams.lock().unwrap();
            match map.get(content_id).and_then(|w| w.upgrade()) {
                Some(s) => (s, false),
                None => {
                    let s = Arc::new(EngineStream {
                        content_id: content_id.to_owned(),
                        started: Instant::now(),
                        bytes_in: AtomicU64::new(0),
                        session: OnceLock::new(),
                        probe: OnceLock::new(),
                        socket: OnceLock::new(),
                        stats: Mutex::new(None),
                        replay: Mutex::new(VecDeque::new()),
                        encoders: Mutex::new(HashMap::new()),
                        creating: Mutex::new(()),
                        start: (Mutex::new(Start::Pending), Condvar::new()),
                        config: self.config.clone(),
                    });
                    map.insert(content_id.to_owned(), Arc::downgrade(&s));
                    (s, true)
                }
            }
        };

        if fresh {
            let weak = Arc::downgrade(&stream);
            let id = content_id.to_owned();
            let cfg = self.config.clone();
            thread::Builder::new()
                .name(format!("pull-{}", &content_id[..8.min(content_id.len())]))
                .spawn(move || pull(weak, id, cfg))
                .map_err(|e| format!("could not start puller: {e}"))?;
        }

        // Concurrent requests for the same cold id all wait here on one pull.
        stream.wait_ready()?;
        Ok(stream)
    }

    /// Subscribe to `content_id` in `fmt`, starting the engine pull and the
    /// format's ffmpeg if they are not already running.
    pub fn subscribe(
        &self,
        content_id: &str,
        fmt: OutputFormat,
        peer: &str,
    ) -> Result<Subscription, String> {
        let stream = self.engine_stream(content_id)?;
        // The Arc is held across subscribe, so the encoder cannot be torn down
        // underneath a joiner waiting for its first fragment.
        get_or_start_encoder(&stream, fmt)?.subscribe(peer.to_owned())
    }
}

fn get_or_start_encoder(
    stream: &Arc<EngineStream>,
    fmt: OutputFormat,
) -> Result<Arc<Encoder>, String> {
    // Without this, two requests arriving together both find an empty slot,
    // both spawn ffmpeg, and the second insert orphans the first — its listener
    // then streams from an encoder the puller no longer feeds.
    let _creating = stream.creating.lock().unwrap();
    {
        let map = stream.encoders.lock().unwrap();
        let existing = map.get(&fmt).and_then(|(w, _)| w.upgrade());
        drop(map);
        // A finished encoder is still registered while its last listener drains;
        // start a replacement rather than joining a dead one.
        if let Some(e) = existing {
            if !e.finished.load(Ordering::Relaxed) {
                return Ok(e);
            }
        }
    }

    let probe = stream
        .probe()
        .ok_or_else(|| "engine stream has no probe result".to_string())?;
    let plan = format::plan(fmt, probe.audio.as_deref(), stream.config.frag_duration_ms);
    start_encoder(stream, fmt, plan)
}

fn start_encoder(
    stream: &Arc<EngineStream>,
    fmt: OutputFormat,
    plan: Plan,
) -> Result<Arc<Encoder>, String> {
    let mut child = Command::new("ffmpeg")
        .args(&plan.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Inherited, as the Go service did: with -loglevel error this is nearly
        // silent, and it is the only diagnostic when a stream fails to start.
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("could not start ffmpeg: {e}"))?;

    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");

    let encoder = Arc::new(Encoder {
        content_id: stream.content_id.clone(),
        fmt,
        started: Instant::now(),
        bytes_out: AtomicU64::new(0),
        video: plan.video,
        audio: plan.audio,
        init: Mutex::new(None),
        init_ready: Condvar::new(),
        unjoinable: AtomicBool::new(false),
        finished: AtomicBool::new(false),
        listeners: Mutex::new(Vec::new()),
        next_id: AtomicU64::new(0),
        child: Mutex::new(Some(child)),
        engine: stream.clone(),
    });

    let (tx, rx) = sync_channel::<Bytes>(TS_QUEUE);
    // Snapshot the replay window and register in one step, holding the map lock
    // across both. The puller appends to `replay` before sending to registered
    // encoders, so taking these separately could duplicate a chunk (in the
    // window and again live) or drop one.
    let prime = {
        let mut map = stream.encoders.lock().unwrap();
        let prime = stream.replay_window();
        map.insert(fmt, (Arc::downgrade(&encoder), tx));
        prime
    };

    // stdin and stdout MUST be serviced by different threads: ffmpeg blocks
    // writing output once its stdout pipe fills, so a single thread doing both
    // would deadlock the moment either pipe backs up.
    let name = format!("{}-{}", &stream.content_id[..8.min(stream.content_id.len())], fmt);
    let primed = prime.len();
    thread::Builder::new()
        .name(format!("feed-{name}"))
        .spawn(move || feed_ffmpeg(rx, stdin, prime))
        .map_err(|e| format!("could not start feeder: {e}"))?;

    let weak = Arc::downgrade(&encoder);
    thread::Builder::new()
        .name(format!("emit-{name}"))
        .spawn(move || drain_ffmpeg(weak, stdout, fmt))
        .map_err(|e| format!("could not start drainer: {e}"))?;

    log::info(&format!(
        "{}/{}: encoder started (video {}, audio {}, {} bytes replayed)",
        stream.content_id, fmt, plan.video, plan.audio, primed
    ));
    Ok(encoder)
}

/// Pump raw TS into ffmpeg's stdin. Exits when the puller drops the sender,
/// closing stdin so ffmpeg sees EOF and shuts down cleanly.
///
/// `prime` is the replay window, written before any live data so ffmpeg starts
/// from a keyframe rather than wherever this encoder happened to attach.
fn feed_ffmpeg(rx: Receiver<Bytes>, mut stdin: std::process::ChildStdin, prime: Vec<u8>) {
    if !prime.is_empty() && stdin.write_all(&prime).is_err() {
        return;
    }
    while let Ok(chunk) = rx.recv() {
        if stdin.write_all(&chunk).is_err() {
            break; // ffmpeg exited; the drainer reports why
        }
    }
}

/// Read ffmpeg's output, track fMP4 structure, and fan out to listeners.
fn drain_ffmpeg(encoder: Weak<Encoder>, mut stdout: std::process::ChildStdout, fmt: OutputFormat) {
    let mut scanner = fmt.needs_init_segment().then(mp4::Scanner::new);
    let mut buf = vec![0u8; CHUNK];

    loop {
        let n = match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        // Upgrade per chunk so the last listener leaving ends this thread.
        let Some(enc) = encoder.upgrade() else { break };
        enc.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
        let chunk = Bytes::copy_from_slice(&buf[..n]);

        match scanner.as_mut() {
            None => enc.fan_out(&mp4::Piece::Media {
                data: chunk,
                fragment_start: true,
            }),
            Some(s) => {
                for piece in s.push(chunk) {
                    enc.fan_out(&piece);
                }
                if s.failed() && !enc.unjoinable.load(Ordering::Relaxed) {
                    log::error(&format!(
                        "{}/{}: output is not valid fragmented mp4; late joiners refused",
                        enc.content_id, fmt
                    ));
                    enc.unjoinable.store(true, Ordering::Relaxed);
                    enc.notify_joiners();
                } else {
                    let mut init = enc.init.lock().unwrap();
                    if init.is_none() {
                        if let Some(segment) = s.init_segment() {
                            *init = Some(segment);
                            enc.init_ready.notify_all();
                        }
                    }
                }
            }
        }
    }

    // Release anyone still waiting for a first fragment that will not come.
    if let Some(enc) = encoder.upgrade() {
        enc.finished.store(true, Ordering::Relaxed);
        enc.notify_joiners();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::TrackMode;

    fn test_engine() -> Arc<EngineStream> {
        Arc::new(EngineStream {
            content_id: "a".repeat(40),
            started: Instant::now(),
            bytes_in: AtomicU64::new(0),
            session: OnceLock::new(),
            probe: OnceLock::new(),
            socket: OnceLock::new(),
            stats: Mutex::new(None),
            replay: Mutex::new(VecDeque::new()),
            encoders: Mutex::new(HashMap::new()),
            creating: Mutex::new(()),
            start: (Mutex::new(Start::Ready), Condvar::new()),
            config: Arc::new(Config {
                engine_host: "127.0.0.1:6878".into(),
                frag_duration_ms: None,
            }),
        })
    }

    /// An encoder with no ffmpeg behind it, for exercising the fan-out policy.
    fn test_encoder(fmt: OutputFormat) -> Arc<Encoder> {
        Arc::new(Encoder {
            content_id: "a".repeat(40),
            fmt,
            started: Instant::now(),
            bytes_out: AtomicU64::new(0),
            video: TrackMode::Copy,
            audio: TrackMode::Copy,
            init: Mutex::new(None),
            init_ready: Condvar::new(),
            unjoinable: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            listeners: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
            child: Mutex::new(None),
            engine: test_engine(),
        })
    }

    fn media(len: usize, fragment_start: bool) -> mp4::Piece {
        mp4::Piece::Media {
            data: Bytes::from(vec![0xAB; len]),
            fragment_start,
        }
    }

    /// Build `count` TS packets whose payload byte identifies the packet.
    fn ts_packets(count: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(count * 188);
        for i in 0..count {
            v.push(0x47);
            v.extend_from_slice(&[0x01, 0x00, 0x10]);
            v.extend(std::iter::repeat_n((i & 0xFF) as u8, 184));
        }
        v
    }

    #[test]
    fn replay_window_keeps_the_most_recent_bytes() {
        let s = test_engine();
        let packets = REPLAY_BYTES / 188 + 500;
        let data = ts_packets(packets);
        // Feed in chunks, as the puller does.
        for chunk in data.chunks(64 * 1024) {
            s.remember(chunk);
        }

        let window = s.replay_window();
        assert!(window.len() <= REPLAY_BYTES);
        assert!(window.len() > REPLAY_BYTES - 188 * 2);
        // The tail must be the newest data, not the oldest.
        assert_eq!(&window[window.len() - 184..], &data[data.len() - 184..]);
    }

    #[test]
    fn replay_window_starts_on_a_packet_boundary() {
        let s = test_engine();
        // Overflow by a partial packet so the window starts mid-packet.
        let data = ts_packets(REPLAY_BYTES / 188 + 3);
        s.remember(&data[57..]);
        let window = s.replay_window();
        assert_eq!(window.first(), Some(&0x47), "must begin at a sync byte");
        assert_eq!(window[188], 0x47, "and the next packet must line up");
    }

    #[test]
    fn replay_window_is_empty_before_any_data() {
        assert!(test_engine().replay_window().is_empty());
    }

    #[test]
    fn replay_window_survives_a_stream_with_no_sync_bytes() {
        let s = test_engine();
        // Garbage in, garbage out — but no panic, and nothing silently dropped.
        s.remember(&vec![0x00; 1000]);
        assert_eq!(s.replay_window().len(), 1000);
    }

    #[test]
    fn audio_listeners_start_receiving_immediately() {
        let enc = test_encoder(OutputFormat::Adts);
        let sub = enc.clone().subscribe("peer".into()).unwrap();
        // ADTS is self-synchronising, so there is nothing to wait for.
        enc.fan_out(&media(10, false));
        assert_eq!(sub.bytes_sent(), 10);
    }

    #[test]
    fn fmp4_listeners_wait_for_a_fragment_boundary() {
        let enc = test_encoder(OutputFormat::Fmp4);
        *enc.init.lock().unwrap() = Some(Bytes::from_static(b"ftypmoov"));
        let sub = enc.clone().subscribe("peer".into()).unwrap();

        // Mid-fragment bytes are useless to a client holding only the init
        // segment, so they must be withheld.
        enc.fan_out(&media(10, false));
        assert_eq!(sub.bytes_sent(), 0, "mid-fragment data must be held back");

        enc.fan_out(&media(20, true));
        assert_eq!(sub.bytes_sent(), 20, "must attach at the moof");

        // Once attached, everything flows.
        enc.fan_out(&media(5, false));
        assert_eq!(sub.bytes_sent(), 25);
    }

    #[test]
    fn fmp4_subscribe_blocks_until_the_first_fragment_arrives() {
        let enc = test_encoder(OutputFormat::Fmp4);
        let writer = enc.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            *writer.init.lock().unwrap() = Some(Bytes::from_static(b"ftypmoov"));
            writer.init_ready.notify_all();
        });

        // Waiting is the whole point: tearing down and retrying would restart
        // ffmpeg and never converge.
        let began = Instant::now();
        assert!(enc.subscribe("peer".into()).is_ok());
        assert!(began.elapsed() >= Duration::from_millis(150));
    }

    #[test]
    fn fmp4_subscribe_fails_fast_once_the_encoder_has_finished() {
        let enc = test_encoder(OutputFormat::Fmp4);
        enc.finished.store(true, Ordering::Relaxed);
        let began = Instant::now();
        assert!(enc.subscribe("peer".into()).is_err());
        // Must not sit out the full INIT_TIMEOUT waiting on a dead encoder.
        assert!(began.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn fmp4_subscribe_is_refused_when_the_stream_did_not_parse() {
        let enc = test_encoder(OutputFormat::Fmp4);
        // Init was cached before the structure broke. Serving it anyway would
        // leave the joiner waiting forever for a boundary that never comes.
        *enc.init.lock().unwrap() = Some(Bytes::from_static(b"ftypmoov"));
        enc.unjoinable.store(true, Ordering::Relaxed);
        let began = Instant::now();
        assert!(enc.subscribe("peer".into()).is_err());
        assert!(began.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_slow_listener_is_evicted_rather_than_blocking_the_others() {
        let enc = test_encoder(OutputFormat::Adts);
        let slow = enc.clone().subscribe("slow".into()).unwrap();
        let _fast = enc.clone().subscribe("fast".into()).unwrap();
        assert_eq!(enc.listener_count(), 2);

        // Neither reads, so both back up. A shared producer must not block on
        // its slowest consumer.
        for _ in 0..(SUB_QUEUE + 8) {
            enc.fan_out(&media(64, true));
        }
        assert_eq!(enc.listener_count(), 0, "both should have been evicted");
        drop(slow);
    }

    #[test]
    fn dropping_a_subscription_deregisters_its_listener() {
        let enc = test_encoder(OutputFormat::Adts);
        let a = enc.clone().subscribe("a".into()).unwrap();
        let b = enc.clone().subscribe("b".into()).unwrap();
        assert_eq!(enc.listener_count(), 2);
        drop(a);
        assert_eq!(enc.listener_count(), 1);
        drop(b);
        assert_eq!(enc.listener_count(), 0);
    }

    #[test]
    fn subscription_read_hands_back_partial_chunks() {
        let enc = test_encoder(OutputFormat::Adts);
        let mut sub = enc.clone().subscribe("peer".into()).unwrap();
        enc.fan_out(&media(10, true));

        // tiny_http reads with whatever buffer it likes; a chunk larger than
        // the buffer must be handed out across successive reads.
        let mut buf = [0u8; 3];
        let mut total = 0;
        for _ in 0..4 {
            let n = sub.read(&mut buf).unwrap();
            total += n;
            if total == 10 {
                break;
            }
        }
        assert_eq!(total, 10);
    }

    #[test]
    fn subscription_read_ends_cleanly_when_the_encoder_stops() {
        let enc = test_encoder(OutputFormat::Adts);
        let mut sub = enc.clone().subscribe("peer".into()).unwrap();
        // Dropping every listener handle closes the channel.
        enc.listeners.lock().unwrap().clear();
        let mut buf = [0u8; 8];
        assert_eq!(sub.read(&mut buf).unwrap(), 0, "must EOF, not error");
    }

    #[test]
    fn fmp4_listeners_receive_the_init_segment_first() {
        let enc = test_encoder(OutputFormat::Fmp4);
        *enc.init.lock().unwrap() = Some(Bytes::from_static(b"ftypmoov"));
        let mut sub = enc.clone().subscribe("peer".into()).unwrap();
        enc.fan_out(&media(4, true));

        let mut buf = [0u8; 8];
        assert_eq!(sub.read(&mut buf).unwrap(), 8);
        assert_eq!(&buf, b"ftypmoov", "init segment must precede all media");
    }

    #[test]
    fn encoder_deregisters_itself_from_the_engine_on_drop() {
        let engine = test_engine();
        let enc = Arc::new(Encoder {
            content_id: engine.content_id.clone(),
            fmt: OutputFormat::Adts,
            started: Instant::now(),
            bytes_out: AtomicU64::new(0),
            video: TrackMode::Dropped,
            audio: TrackMode::Copy,
            init: Mutex::new(None),
            init_ready: Condvar::new(),
            unjoinable: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            listeners: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
            child: Mutex::new(None),
            engine: engine.clone(),
        });
        let (tx, _rx) = sync_channel::<Bytes>(TS_QUEUE);
        engine
            .encoders
            .lock()
            .unwrap()
            .insert(OutputFormat::Adts, (Arc::downgrade(&enc), tx));
        assert_eq!(engine.live_encoders().len(), 1);

        drop(enc);
        // Deadlocks if Drop is reached while `encoders` is held; see the
        // locking rule in the module docs.
        assert!(engine.encoders.lock().unwrap().is_empty());
    }
}

/// One thread polls every live stream's engine statistics, rather than one per
/// stream.
fn spawn_stats_poller(reg: Weak<Registry>) {
    thread::Builder::new()
        .name("engine-stats".into())
        .spawn(move || loop {
            thread::sleep(STATS_INTERVAL);
            let Some(reg) = reg.upgrade() else { return };
            let streams = reg.live_streams();
            drop(reg);
            for s in streams {
                let Some(url) = s.stat_url() else { continue };
                match engine::fetch_stats(&url) {
                    Ok(v) => *s.stats.lock().unwrap() = Some((Instant::now(), v)),
                    Err(e) => log::warn(&format!("{}: stat poll failed: {e}", s.content_id)),
                }
            }
        })
        .expect("could not start the stats poller");
}
