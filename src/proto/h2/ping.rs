/// HTTP2 Ping usage
///
/// hyper uses HTTP2 pings for two purposes:
///
/// 1. Adaptive flow control using BDP
/// 2. Connection keep-alive
///
/// Both cases are optional.
///
/// # BDP Algorithm
///
/// 1. When receiving a DATA frame, if a BDP ping isn't outstanding:
///   1a. Record current time.
///   1b. Send a BDP ping.
/// 2. Increment the number of received bytes.
/// 3. When the BDP ping ack is received:
///   3a. Record duration from sent time.
///   3b. Merge RTT with a running average.
///   3c. Calculate bdp as bytes/rtt.
///   3d. If bdp is over 2/3 max, set new max to bdp and update windows.

#[cfg(feature = "runtime")]
use std::future::Future;
#[cfg(feature = "runtime")]
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{self, Poll};
use std::time::Duration;
#[cfg(not(feature = "runtime"))]
use std::time::Instant;

use h2::{Ping, PingPong};
#[cfg(feature = "runtime")]
use tokio::time::{Delay, Instant};

type WindowSize = u32;

pub(super) fn disabled() -> Recorder {
    Recorder { shared: None }
}

pub(super) fn channel(ping_pong: PingPong, config: Config) -> (Recorder, Ponger) {
    debug_assert!(
        config.is_enabled(),
        "ping channel requires bdp or keep-alive config",
    );

    let bdp = config.bdp_initial_window.map(|wnd| Bdp {
        bdp: wnd,
        max_bandwidth: 0.0,
        samples: 0,
        rtt: 0.0,
    });

    let bytes = bdp.as_ref().map(|_| 0);

    #[cfg(feature = "runtime")]
    let keep_alive = config.keep_alive_interval.map(|interval| KeepAlive {
        interval,
        timeout: config.keep_alive_timeout,
        while_idle: config.keep_alive_while_idle,
        timer: tokio::time::delay_for(interval),
        state: KeepAliveState::Init,
    });

    #[cfg(feature = "runtime")]
    let last_read_at = keep_alive.as_ref().map(|_| Instant::now());

    let shared = Arc::new(Mutex::new(Shared {
        bytes,
        #[cfg(feature = "runtime")]
        last_read_at,
        ping_pong,
        ping_sent_at: None,
    }));

    (
        Recorder {
            shared: Some(shared.clone()),
        },
        Ponger {
            bdp,
            #[cfg(feature = "runtime")]
            keep_alive,
            shared,
        },
    )
}

#[derive(Clone)]
pub(super) struct Config {
    pub(super) bdp_initial_window: Option<WindowSize>,
    /// If no frames are received in this amount of time, a PING frame is sent.
    #[cfg(feature = "runtime")]
    pub(super) keep_alive_interval: Option<Duration>,
    /// After sending a keepalive PING, the connection will be closed if
    /// a pong is not received in this amount of time.
    #[cfg(feature = "runtime")]
    pub(super) keep_alive_timeout: Duration,
    /// If true, sends pings even when there are no active streams.
    #[cfg(feature = "runtime")]
    pub(super) keep_alive_while_idle: bool,
}

#[derive(Clone)]
pub(crate) struct Recorder {
    shared: Option<Arc<Mutex<Shared>>>,
}

pub(super) struct Ponger {
    bdp: Option<Bdp>,
    #[cfg(feature = "runtime")]
    keep_alive: Option<KeepAlive>,
    shared: Arc<Mutex<Shared>>,
}

struct Shared {
    ping_pong: PingPong,
    ping_sent_at: Option<Instant>,

    // bdp
    /// If `Some`, bdp is enabled, and this tracks how many bytes have been
    /// read during the current sample.
    bytes: Option<usize>,

    // keep-alive
    /// If `Some`, keep-alive is enabled, and the Instant is how long ago
    /// the connection read the last frame.
    #[cfg(feature = "runtime")]
    last_read_at: Option<Instant>,
}

struct Bdp {
    /// Current BDP in bytes
    bdp: u32,
    /// Largest bandwidth we've seen so far.
    max_bandwidth: f64,
    /// Count of samples made (ping sent and received)
    samples: usize,
    /// Round trip time in seconds
    rtt: f64,
}

#[cfg(feature = "runtime")]
struct KeepAlive {
    /// If no frames are received in this amount of time, a PING frame is sent.
    interval: Duration,
    /// After sending a keepalive PING, the connection will be closed if
    /// a pong is not received in this amount of time.
    timeout: Duration,
    /// If true, sends pings even when there are no active streams.
    while_idle: bool,

    state: KeepAliveState,
    timer: Delay,
}

#[cfg(feature = "runtime")]
enum KeepAliveState {
    Init,
    Scheduled,
    PingSent,
}

pub(super) enum Ponged {
    SizeUpdate(WindowSize),
    #[cfg(feature = "runtime")]
    KeepAliveTimedOut,
}

#[cfg(feature = "runtime")]
struct KeepAliveTimedOut;

// ===== impl Config =====

impl Config {
    pub(super) fn is_enabled(&self) -> bool {
        #[cfg(feature = "runtime")]
        {
            self.bdp_initial_window.is_some() || self.keep_alive_interval.is_some()
        }

        #[cfg(not(feature = "runtime"))]
        {
            self.bdp_initial_window.is_some()
        }
    }
}

// ===== impl Recorder =====

impl Recorder {
    pub(crate) fn record_data(&self, len: usize) {
        let shared = if let Some(ref shared) = self.shared {
            shared
        } else {
            return;
        };

        let mut locked = shared.lock().unwrap();

        #[cfg(feature = "runtime")]
        locked.update_last_read_at();

        if let Some(ref mut bytes) = locked.bytes {
            *bytes += len;
        } else {
            // no need to send bdp ping if bdp is disabled
            return;
        }

        if !locked.is_ping_sent() {
            locked.send_ping();
        }
    }

    pub(crate) fn record_non_data(&self) {
        #[cfg(feature = "runtime")]
        {
            let shared = if let Some(ref shared) = self.shared {
                shared
            } else {
                return;
            };

            let mut locked = shared.lock().unwrap();

            locked.update_last_read_at();
        }
    }

    /// If the incoming stream is already closed, convert self into
    /// a disabled reporter.
    pub(super) fn for_stream(self, stream: &h2::RecvStream) -> Self {
        if stream.is_end_stream() {
            disabled()
        } else {
            self
        }
    }
}

// ===== impl Ponger =====

impl Ponger {
    pub(super) fn poll(&mut self, cx: &mut task::Context<'_>) -> Poll<Ponged> {
        let mut locked = self.shared.lock().unwrap();
        #[cfg(feature = "runtime")]
        let is_idle = self.is_idle();

        #[cfg(feature = "runtime")]
        {
            if let Some(ref mut ka) = self.keep_alive {
                ka.schedule(is_idle, &locked);
                ka.maybe_ping(cx, &mut locked);
            }
        }

        if !locked.is_ping_sent() {
            // XXX: this doesn't register a waker...?
            return Poll::Pending;
        }

        let (bytes, rtt) = match locked.ping_pong.poll_pong(cx) {
            Poll::Ready(Ok(_pong)) => {
                let rtt = locked
                    .ping_sent_at
                    .expect("pong received implies ping_sent_at")
                    .elapsed();
                locked.ping_sent_at = None;
                trace!("recv pong");

                #[cfg(feature = "runtime")]
                {
                    if let Some(ref mut ka) = self.keep_alive {
                        locked.update_last_read_at();
                        ka.schedule(is_idle, &locked);
                    }
                }

                if let Some(ref mut bdp) = self.bdp {
                    let bytes = locked.bytes.expect("bdp enabled implies bytes");
                    locked.bytes = Some(0); // reset
                    bdp.samples += 1;
                    trace!("received BDP ack; bytes = {}, rtt = {:?}", bytes, rtt);
                    (bytes, rtt)
                } else {
                    // no bdp, done!
                    return Poll::Pending;
                }
            }
            Poll::Ready(Err(e)) => {
                debug!("pong error: {}", e);
                return Poll::Pending;
            }
            Poll::Pending => {
                #[cfg(feature = "runtime")]
                {
                    if let Some(ref mut ka) = self.keep_alive {
                        if let Err(KeepAliveTimedOut) = ka.maybe_timeout(cx) {
                            self.keep_alive = None;
                            return Poll::Ready(Ponged::KeepAliveTimedOut);
                        }
                    }
                }

                return Poll::Pending;
            }
        };

        drop(locked);

        if let Some(bdp) = self.bdp.as_mut().and_then(|bdp| bdp.calculate(bytes, rtt)) {
            Poll::Ready(Ponged::SizeUpdate(bdp))
        } else {
            // XXX: this doesn't register a waker...?
            Poll::Pending
        }
    }

    #[cfg(feature = "runtime")]
    fn is_idle(&self) -> bool {
        Arc::strong_count(&self.shared) <= 2
    }
}

// ===== impl Shared =====

impl Shared {
    fn send_ping(&mut self) {
        match self.ping_pong.send_ping(Ping::opaque()) {
            Ok(()) => {
                self.ping_sent_at = Some(Instant::now());
                trace!("sent ping");
            }
            Err(err) => {
                debug!("error sending ping: {}", err);
            }
        }
    }

    fn is_ping_sent(&self) -> bool {
        self.ping_sent_at.is_some()
    }

    #[cfg(feature = "runtime")]
    fn update_last_read_at(&mut self) {
        if self.last_read_at.is_some() {
            self.last_read_at = Some(Instant::now());
        }
    }

    #[cfg(feature = "runtime")]
    fn last_read_at(&self) -> Instant {
        self.last_read_at.expect("keep_alive expects last_read_at")
    }
}

// ===== impl Bdp =====

/// Any higher than this likely will be hitting the TCP flow control.
const BDP_LIMIT: usize = 1024 * 1024 * 16;

impl Bdp {
    fn calculate(&mut self, bytes: usize, rtt: Duration) -> Option<WindowSize> {
        // No need to do any math if we're at the limit.
        if self.bdp as usize == BDP_LIMIT {
            return None;
        }

        // average the rtt
        let rtt = seconds(rtt);
        if self.samples < 10 {
            // Average the first 10 samples
            self.rtt += (rtt - self.rtt) / (self.samples as f64);
        } else {
            self.rtt += (rtt - self.rtt) / 0.9;
        }

        // calculate the current bandwidth
        let bw = (bytes as f64) / (self.rtt * 1.5);
        trace!("current bandwidth = {:.1}B/s", bw);

        if bw < self.max_bandwidth {
            // not a faster bandwidth, so don't update
            return None;
        } else {
            self.max_bandwidth = bw;
        }

        // if the current `bytes` sample is at least 2/3 the previous
        // bdp, increase to double the current sample.
        if (bytes as f64) >= (self.bdp as f64) * 0.66 {
            self.bdp = (bytes * 2).min(BDP_LIMIT) as WindowSize;
            trace!("BDP increased to {}", self.bdp);
            Some(self.bdp)
        } else {
            None
        }
    }
}

fn seconds(dur: Duration) -> f64 {
    const NANOS_PER_SEC: f64 = 1_000_000_000.0;
    let secs = dur.as_secs() as f64;
    secs + (dur.subsec_nanos() as f64) / NANOS_PER_SEC
}

// ===== impl KeepAlive =====

#[cfg(feature = "runtime")]
impl KeepAlive {
    fn schedule(&mut self, is_idle: bool, shared: &Shared) {
        match self.state {
            KeepAliveState::Init => {
                if !self.while_idle && is_idle {
                    return;
                }

                self.state = KeepAliveState::Scheduled;
                let interval = shared.last_read_at() + self.interval;
                self.timer.reset(interval);
            }
            KeepAliveState::Scheduled | KeepAliveState::PingSent => (),
        }
    }

    fn maybe_ping(&mut self, cx: &mut task::Context<'_>, shared: &mut Shared) {
        match self.state {
            KeepAliveState::Scheduled => {
                if Pin::new(&mut self.timer).poll(cx).is_pending() {
                    return;
                }
                // check if we've received a frame while we were scheduled
                if shared.last_read_at() + self.interval > self.timer.deadline() {
                    self.state = KeepAliveState::Init;
                    cx.waker().wake_by_ref(); // schedule us again
                    return;
                }
                trace!("keep-alive interval ({:?}) reached", self.interval);
                shared.send_ping();
                self.state = KeepAliveState::PingSent;
                let timeout = Instant::now() + self.timeout;
                self.timer.reset(timeout);
            }
            KeepAliveState::Init | KeepAliveState::PingSent => (),
        }
    }

    fn maybe_timeout(&mut self, cx: &mut task::Context<'_>) -> Result<(), KeepAliveTimedOut> {
        match self.state {
            KeepAliveState::PingSent => {
                if Pin::new(&mut self.timer).poll(cx).is_pending() {
                    return Ok(());
                }
                trace!("keep-alive timeout ({:?}) reached", self.timeout);
                Err(KeepAliveTimedOut)
            }
            KeepAliveState::Init | KeepAliveState::Scheduled => Ok(()),
        }
    }
}