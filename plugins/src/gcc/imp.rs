///! Implements https://datatracker.ietf.org/doc/html/draft-ietf-rmcat-gcc-02
///!
///! This element implements the pacing as describe in the spec by running its
///! own streaming thread on its srcpad. It implements the mathematic as closely
///! to the specs as possible and sets the #rtpgccbwe:estimated-bitrate property
///! each time a new estimate is produced. User should connect to the
///! `rtpgccbwe::notify::estimated-bitrate` signal to make the encoders target
///! that new estimated bitrate (the overall target bitrate of the potentially
///! multiple encore should match that target bitrate, the application is
///! responsible for determining what bitrate to give to each encode)
use chrono::Duration;
use gst::{
    glib::{self},
    prelude::*,
    subclass::prelude::*,
};
use once_cell::sync::Lazy;
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    fmt::Debug,
    mem,
    sync::Mutex,
    time,
};

type Bitrate = u32;

const DEFAULT_MIN_BITRATE: Bitrate = 1000;
const DEFAULT_ESTIMATED_BITRATE: Bitrate = 2_048_000;
const DEFAULT_MAX_BITRATE: Bitrate = 8_192_000;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "gcc",
        gst::DebugColorFlags::empty(),
        Some("Google Congestion Controller based bandwidth estimator"),
    )
});

// Table1. Time limit in milliseconds  between packet bursts which  identifies a group
static BURST_TIME: Lazy<Duration> = Lazy::new(|| Duration::milliseconds(5));

// Table1. Coefficient used for the measured noise variance
//  [0.1,0.001]
const CHI: f64 = 0.01;
const ONE_MINUS_CHI: f64 = 1. - CHI;

// Table1. State noise covariance matrix
const Q: f64 = 0.001;

// Table1. Initial value for the adaptive threshold
static INITIAL_DEL_VAR_TH: Lazy<Duration> = Lazy::new(|| Duration::microseconds(12500));

// Table1. Initial value of the system error covariance
const INITIAL_ERROR_COVARIANCE: f64 = 0.1;

// Table1. Time required to trigger an overuse signal
static OVERUSE_TIME_TH: Lazy<Duration> = Lazy::new(|| Duration::milliseconds(10));

// from 5.5 "beta is typically chosen to be in the interval [0.8, 0.95], 0.85 is the RECOMMENDED value."
const BETA: f64 = 0.85;

// From "5.5 Rate control" It is RECOMMENDED to measure this average and
// standard deviation with an exponential moving average with the smoothing
// factor 0.5 (NOTE: the spec mentions 0.95 here but in the equations it is 0.5
// and other implementations use 0.5), as it is expected that this average
// covers multiple occasions at which we are in the Decrease state.
const MOVING_AVERAGE_SMOOTHING_FACTOR: f64 = 0.5;

// `N(i)` is the number of packets received the past T seconds and `L(j)` is
// the payload size of packet j.  A window between 0.5 and 1 second is
// RECOMMENDED.
static PACKETS_RECEIVED_WINDOW: Lazy<Duration> = Lazy::new(|| Duration::milliseconds(1000)); // ms

// from "5.4 Over-use detector" ->
// Moreover, del_var_th(i) SHOULD NOT be updated if this condition
// holds:
//
// ```
// |m(i)| - del_var_th(i) > 15
// ```
static MAX_M_MINUS_DEL_VAR_TH: Lazy<Duration> = Lazy::new(|| Duration::milliseconds(15));

// from 5.4 "It is also RECOMMENDED to clamp del_var_th(i) to the range [6, 600]"
static MIN_THRESHOLD: Lazy<Duration> = Lazy::new(|| Duration::milliseconds(6));
static MAX_THRESHOLD: Lazy<Duration> = Lazy::new(|| Duration::milliseconds(600));

// From 5.5 ""Close" is defined as three standard deviations around this average"
const STANDARD_DEVIATION_CLOSE_NUM: f64 = 3.;

// Minimal duration between 2 updates on the lost based rate controller
static LOSS_UPDATE_INTERVAL: Lazy<time::Duration> = Lazy::new(|| time::Duration::from_millis(200));
static LOSS_DECREASE_THRESHOLD: f64 = 0.1;
static LOSS_INCREASE_THRESHOLD: f64 = 0.02;
static LOSS_INCREASE_FACTOR: f64 = 1.05;

// Minimal duration between 2 updates on the lost based rate controller
static DELAY_UPDATE_INTERVAL: Lazy<time::Duration> = Lazy::new(|| time::Duration::from_millis(100));

static ROUND_TRIP_TIME_WINDOW_SIZE: usize = 100;

fn ts2dur(t: gst::ClockTime) -> Duration {
    Duration::nanoseconds(t.nseconds() as i64)
}

fn dur2ts(t: Duration) -> gst::ClockTime {
    gst::ClockTime::from_nseconds(t.num_nanoseconds().unwrap() as u64)
}

#[derive(Debug)]
enum BandwidthEstimationOp {
    /// Don't update target bitrate
    Hold,
    /// Decrease target bitrate
    Decrease(String /* reason */),
    Increase(String /* reason */),
}

#[derive(Debug, Clone, Copy)]
enum ControllerType {
    // Running the "delay-based controller"
    Delay,
    // Running the "loss based controller"
    Loss,
}

#[derive(Debug, Clone, Copy)]
struct Packet {
    departure: Duration,
    arrival: Duration,
    size: usize,
    seqnum: u64,
}

fn human_kbits<T: Into<f64>>(bits: T) -> String {
    format!("{:.2}kb", (bits.into() / 1_000.))
}

impl Packet {
    fn from_structure(structure: &gst::StructureRef) -> Option<Self> {
        let lost = structure.get::<bool>("lost").unwrap();
        let departure = match structure.get::<gst::ClockTime>("local-ts") {
            Err(e) => {
                gst::fixme!(
                    CAT,
                    "Got packet feedback without local-ts: {:?} - what does that mean?",
                    e
                );
                return None;
            }
            Ok(ts) => ts,
        };

        let seqnum = structure.get::<u32>("seqnum").unwrap() as u64;
        if lost {
            return Some(Packet {
                arrival: Duration::zero(),
                departure: ts2dur(departure),
                size: structure.get::<u32>("size").unwrap() as usize,
                seqnum,
            });
        }

        let arrival = structure.get::<gst::ClockTime>("remote-ts").unwrap();

        Some(Packet {
            arrival: ts2dur(arrival),
            departure: ts2dur(departure),
            size: structure.get::<u32>("size").unwrap() as usize,
            seqnum,
        })
    }
}

#[derive(Clone)]
struct PacketGroup {
    packets: Vec<Packet>,
    departure: Duration,       // ms
    arrival: Option<Duration>, // ms
}

impl Default for PacketGroup {
    fn default() -> Self {
        Self {
            packets: Default::default(),
            departure: Duration::zero(),
            arrival: None,
        }
    }
}

fn pdur(d: &Duration) -> String {
    let stdd = time::Duration::from_nanos(d.num_nanoseconds().unwrap().abs() as u64);

    format!("{}{stdd:?}", if d.lt(&Duration::zero()) { "-" } else { "" })
}

impl PacketGroup {
    fn add(&mut self, packet: Packet) {
        if self.departure.is_zero() {
            self.departure = packet.departure;
        }

        self.arrival = Some(
            self.arrival
                .map_or_else(|| packet.arrival, |v| Duration::max(v, packet.arrival)),
        );
        self.packets.push(packet);
    }

    /// Returns the delta between self.arrival_time and @prev_group.arrival_time in ms
    // t(i) - t(i-1)
    fn inter_arrival_time(&self, prev_group: &Self) -> Duration {
        // Should never be called if we haven't gotten feedback for all
        // contained packets
        self.arrival.unwrap() - prev_group.arrival.unwrap()
    }

    fn inter_arrival_time_pkt(&self, next_pkt: &Packet) -> Duration {
        // Should never be called if we haven't gotten feedback for all
        // contained packets
        next_pkt.arrival - self.arrival.unwrap()
    }

    /// Returns the delta between self.departure_time and @prev_group.departure_time in ms
    // T(i) - T(i-1)
    fn inter_departure_time(&self, prev_group: &Self) -> Duration {
        // Should never be called if we haven't gotten feedback for all
        // contained packets
        self.departure - prev_group.departure
    }

    fn inter_departure_time_pkt(&self, next_pkt: &Packet) -> Duration {
        // Should never be called if we haven't gotten feedback for all
        // contained packets
        next_pkt.departure - self.departure
    }

    /// Returns the delta between intern arrival time and inter departure time in ms
    fn inter_delay_variation(&self, prev_group: &Self) -> Duration {
        // Should never be called if we haven't gotten feedback for all
        // contained packets
        self.inter_arrival_time(prev_group) - self.inter_departure_time(prev_group)
    }

    fn inter_delay_variation_pkt(&self, next_pkt: &Packet) -> Duration {
        // Should never be called if we haven't gotten feedback for all
        // contained packets
        self.inter_arrival_time_pkt(next_pkt) - self.inter_departure_time_pkt(next_pkt)
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum NetworkUsage {
    Normal,
    Over,
    Under,
}

struct Detector {
    group: PacketGroup,              // Packet group that is being filled
    prev_group: Option<PacketGroup>, // Group that is ready to be used once "group" is filled
    measure: Duration,               // Delay variation measure

    last_received_packets: BTreeMap<u64, Packet>, // Order by seqnums, front is the newest, back is the oldest

    // Last loss update
    last_loss_update: Option<time::Instant>,
    // Moving average of the packet loss
    loss_average: f64,

    // Kalman filter fields
    gain: f64,
    measurement_uncertainty: f64, // var_v_hat(i-1)
    estimate_error: f64,          // e(i-1)
    estimate: Duration,           // m_hat(i-1)

    // Threshold fields
    threshold: Duration,
    last_threshold_update: Option<time::Instant>,
    num_deltas: i64,

    // Overuse related fields
    increasing_counter: u32,
    last_overuse_estimate: Duration,
    last_use_detector_update: time::Instant,
    increasing_duration: Duration,

    // round-trip-time estimations
    rtts: VecDeque<Duration>,
    clock: gst::Clock,

    // Current network usage state
    usage: NetworkUsage,

    twcc_extended_seqnum: u64,
}

// Monitors packet loss and network overuse through because of delay
impl Debug for Detector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Network Usage: {:?}. Effective bitrate: {}ps - Measure: {} Estimate: {} threshold {} - overuse_estimate {}",
            self.usage,
            human_kbits(self.effective_bitrate()),
            pdur(&self.measure),
            pdur(&self.estimate),
            pdur(&self.threshold),
            pdur(&self.last_overuse_estimate),
        )
    }
}

impl Detector {
    fn new() -> Self {
        Detector {
            group: Default::default(),
            prev_group: Default::default(),
            measure: Duration::zero(),

            /* Smallish value to hold PACKETS_RECEIVED_WINDOW packets */
            last_received_packets: BTreeMap::new(),

            last_loss_update: None,
            loss_average: 0.,

            gain: 0.,
            measurement_uncertainty: 0.,
            estimate_error: INITIAL_ERROR_COVARIANCE,
            estimate: Duration::zero(),

            threshold: *INITIAL_DEL_VAR_TH,
            last_threshold_update: None,
            num_deltas: 0,

            last_use_detector_update: time::Instant::now(),
            increasing_counter: 0,
            last_overuse_estimate: Duration::zero(),
            increasing_duration: Duration::zero(),

            rtts: Default::default(),
            clock: gst::SystemClock::obtain(),

            usage: NetworkUsage::Normal,

            twcc_extended_seqnum: 0,
        }
    }

    fn loss_ratio(&self) -> f64 {
        self.loss_average
    }

    fn update_last_received_packets(&mut self, packet: Packet) {
        self.last_received_packets.insert(packet.seqnum, packet);
        self.evict_old_received_packets();
    }

    fn evict_old_received_packets(&mut self) {
        let last_arrival = self
            .last_received_packets
            .values()
            .next_back()
            .unwrap()
            .arrival;

        while last_arrival - self.oldest_packet_in_window_ts() > *PACKETS_RECEIVED_WINDOW {
            let oldest_seqnum = self.last_received_packets.iter().next().unwrap().0.clone();
            self.last_received_packets.remove(&oldest_seqnum);
        }
    }

    /// Returns the effective received bitrate during the last PACKETS_RECEIVED_WINDOW
    fn effective_bitrate(&self) -> Bitrate {
        if self.last_received_packets.is_empty() {
            return 0;
        }

        let duration = self
            .last_received_packets
            .iter()
            .next_back()
            .unwrap()
            .1
            .arrival
            - self.last_received_packets.iter().next().unwrap().1.arrival;
        let bits = self
            .last_received_packets
            .iter()
            .map(|(_seqnum, p)| p.size as f64)
            .sum::<f64>()
            * 8.;

        (bits
            / (duration.num_nanoseconds().unwrap() as f64
                / gst::ClockTime::SECOND.nseconds() as f64)) as Bitrate
    }

    fn oldest_packet_in_window_ts(&self) -> Duration {
        self.last_received_packets.iter().next().unwrap().1.arrival
    }

    fn update_rtts(&mut self, packets: &Vec<Packet>) {
        let mut rtt = Duration::nanoseconds(i64::MAX);
        let now = ts2dur(self.clock.time().unwrap());
        for packet in packets {
            rtt = (now - packet.departure).min(rtt);
        }

        self.rtts.push_back(rtt);
        if self.rtts.len() > ROUND_TRIP_TIME_WINDOW_SIZE {
            self.rtts.pop_front();
        }
    }

    fn rtt(&self) -> Duration {
        Duration::nanoseconds(
            (self
                .rtts
                .iter()
                .map(|d| d.num_nanoseconds().unwrap() as f64)
                .sum::<f64>()
                / self.rtts.len() as f64) as i64,
        )
    }

    fn update(&mut self, packets: &mut Vec<Packet>) {
        self.update_rtts(packets);
        let mut lost_packets = 0.;
        let n_packets = packets.len();
        for pkt in packets {
            // We know feedbacks packets will arrive "soon" after the packets they are reported for or considered
            // lost so we can make the assumption that
            let mut seqnum = pkt.seqnum + (self.twcc_extended_seqnum & !(0xffff as u64));

            if seqnum < self.twcc_extended_seqnum {
                let diff = self.twcc_extended_seqnum.overflowing_sub(seqnum).0;

                if diff > i16::MAX as u64 {
                    seqnum += 1 << 16;
                }
            } else {
                let diff = seqnum.overflowing_sub(self.twcc_extended_seqnum).0;

                if diff > i16::MAX as u64 {
                    if seqnum < 1 << 16 {
                        eprintln!("Cannot unwrap, any wrapping took place yet. Returning 0 without updating extended timestamp.");
                    } else {
                        seqnum -= 1 << 16;
                    }
                }
            }

            self.twcc_extended_seqnum = u64::max(seqnum, self.twcc_extended_seqnum);

            pkt.seqnum = seqnum;

            if pkt.arrival.is_zero() {
                lost_packets += 1.;
                continue;
            }

            self.update_last_received_packets(*pkt);

            if self.group.arrival.is_none() {
                self.group.add(*pkt);

                continue;
            }

            if pkt.arrival < self.group.arrival.unwrap() {
                // ignore out of order arrivals
                continue;
            }

            if pkt.departure >= self.group.departure {
                if self.group.inter_departure_time_pkt(pkt) < *BURST_TIME {
                    self.group.add(*pkt);
                    continue;
                }

                // 5.2 Pre-filtering
                //
                // A Packet which has an inter-arrival time less than burst_time and
                // an inter-group delay variation d(i) less than 0 is considered
                // being part of the current group of packets.
                if self.group.inter_arrival_time_pkt(pkt) < *BURST_TIME
                    && self.group.inter_delay_variation_pkt(pkt) < Duration::zero()
                {
                    self.group.add(*pkt);
                    continue;
                }

                let group = mem::take(&mut self.group);
                gst::trace!(
                    CAT,
                    "Packet group done: {:?}",
                    gst::ClockTime::from_nseconds(group.departure.num_nanoseconds().unwrap() as u64)
                );
                if let Some(prev_group) = mem::replace(&mut self.prev_group, Some(group.clone())) {
                    // 5.3 Arrival-time filter
                    self.kalman_estimate(&prev_group, &group);
                    // 5.4 Over-use detector
                    self.overuse_filter();
                }
            } else {
                gst::debug!(
                    CAT,
                    "Ignoring packet departed at {:?} as we got feedback too late",
                    gst::ClockTime::from_nseconds(pkt.departure.num_nanoseconds().unwrap() as u64)
                );
            }
        }

        self.compute_loss_average(lost_packets as f64 / n_packets as f64);
    }

    fn compute_loss_average(&mut self, loss_fraction: f64) {
        let now = time::Instant::now();

        if let Some(ref last_update) = self.last_loss_update {
            self.loss_average = loss_fraction
                + (-Duration::from_std(now - *last_update)
                    .unwrap()
                    .num_milliseconds() as f64)
                    .exp()
                    * (self.loss_average - loss_fraction);
        }

        self.last_loss_update = Some(now);
    }

    fn kalman_estimate(&mut self, prev_group: &PacketGroup, group: &PacketGroup) {
        self.measure = group.inter_delay_variation(prev_group);

        let z = self.measure - self.estimate;
        let zms = z.num_microseconds().unwrap() as f64 / 1000.0;

        // This doesn't exactly follows the spec as we should compute and
        // use f_max here, no implementation we have found actually uses it.
        let alpha = ONE_MINUS_CHI.powf(30.0 / (1000. * 5. * 1_000_000.));
        let root = self.measurement_uncertainty.sqrt();
        let root3 = 3. * root;

        if zms > root3 {
            self.measurement_uncertainty =
                (alpha * self.measurement_uncertainty + (1. - alpha) * root3.powf(2.)).max(1.);
        } else {
            self.measurement_uncertainty =
                (alpha * self.measurement_uncertainty + (1. - alpha) * zms.powf(2.)).max(1.);
        }

        let estimate_uncertainty = self.estimate_error + Q;
        self.gain = estimate_uncertainty / (estimate_uncertainty + self.measurement_uncertainty);
        self.estimate =
            self.estimate + Duration::nanoseconds((self.gain * zms * 1_000_000.) as i64);
        self.estimate_error = (1. - self.gain) * estimate_uncertainty;
    }

    fn compare_threshold(&mut self) -> (NetworkUsage, Duration) {
        // FIXME: It is unclear where that factor is coming from but all
        // implementations we found have it (libwebrtc, pion, jitsi...), and the
        // algorithm does not work without it.
        const MAX_DELTAS: i64 = 60;

        self.num_deltas += 1;
        if self.num_deltas < 2 {
            return (NetworkUsage::Normal, self.estimate);
        }

        let t = Duration::nanoseconds(
            self.estimate.num_nanoseconds().unwrap() * i64::min(self.num_deltas, MAX_DELTAS),
        );
        let usage = if t > self.threshold {
            NetworkUsage::Over
        } else if t.num_nanoseconds().unwrap() < -self.threshold.num_nanoseconds().unwrap() {
            NetworkUsage::Under
        } else {
            NetworkUsage::Normal
        };

        self.update_threshold(&t);

        (usage, t)
    }

    fn update_threshold(&mut self, estimate: &Duration) {
        const K_U: f64 = 0.01; // Table1. Coefficient for the adaptive threshold
        const K_D: f64 = 0.00018; // Table1. Coefficient for the adaptive threshold
        const MAX_TIME_DELTA: time::Duration = time::Duration::from_millis(100);

        let now = time::Instant::now();
        if self.last_threshold_update.is_none() {
            self.last_threshold_update = Some(now);
        }

        let abs_estimate = Duration::nanoseconds(estimate.num_nanoseconds().unwrap().abs());
        if abs_estimate > self.threshold + *MAX_M_MINUS_DEL_VAR_TH {
            self.last_threshold_update = Some(now);
            return;
        }

        let k = if abs_estimate < self.threshold {
            K_D
        } else {
            K_U
        };
        let time_delta =
            Duration::from_std((now - self.last_threshold_update.unwrap()).min(MAX_TIME_DELTA))
                .unwrap();
        let d = abs_estimate - self.threshold;
        let add = k * d.num_milliseconds() as f64 * time_delta.num_milliseconds() as f64;

        self.threshold = self.threshold + Duration::nanoseconds((add * 100. * 1_000.) as i64);
        self.threshold = self.threshold.clamp(*MIN_THRESHOLD, *MAX_THRESHOLD);
        self.last_threshold_update = Some(now);
    }

    fn overuse_filter(&mut self) {
        let (th_usage, estimate) = self.compare_threshold();

        let now = time::Instant::now();
        let delta = Duration::from_std(now - self.last_use_detector_update).unwrap();
        self.last_use_detector_update = now;
        gst::log!(
            CAT,
            "{:?} - self.estimate {} - estimate: {} - th: {}",
            th_usage,
            pdur(&self.estimate),
            pdur(&estimate),
            pdur(&self.threshold)
        );
        match th_usage {
            NetworkUsage::Over => {
                self.increasing_duration = self.increasing_duration + delta;
                self.increasing_counter += 1;

                if self.increasing_duration > *OVERUSE_TIME_TH
                    && self.increasing_counter > 1
                    && estimate > self.last_overuse_estimate
                {
                    self.usage = NetworkUsage::Over;
                }
            }
            NetworkUsage::Under | NetworkUsage::Normal => {
                self.increasing_duration = Duration::zero();
                self.increasing_counter = 0;

                self.usage = th_usage;
            }
        }
        self.last_overuse_estimate = estimate;
    }
}

#[derive(Default, Debug)]
struct ExponentialMovingAverage {
    average: Option<f64>,
    variance: f64,
    standard_dev: f64,
}

impl ExponentialMovingAverage {
    fn update<T: Into<f64>>(&mut self, value: T) {
        if let Some(avg) = self.average {
            let avg_diff = value.into() - avg;

            self.variance = (1. - MOVING_AVERAGE_SMOOTHING_FACTOR)
                * (self.variance + MOVING_AVERAGE_SMOOTHING_FACTOR * avg_diff * avg_diff);
            self.standard_dev = self.variance.sqrt();

            self.average = Some(avg + (MOVING_AVERAGE_SMOOTHING_FACTOR * avg_diff));
        } else {
            self.average = Some(value.into());
        }
    }

    fn estimate_is_close(&self, value: Bitrate) -> bool {
        self.average.map_or(false, |avg| {
            ((avg - STANDARD_DEVIATION_CLOSE_NUM * self.standard_dev)
                ..(avg + STANDARD_DEVIATION_CLOSE_NUM * self.standard_dev))
                .contains(&(value as f64))
        })
    }
}

struct State {
    /// Note: The target bitrate applied is the min of
    /// target_bitrate_on_delay and target_bitrate_on_loss
    estimated_bitrate: Bitrate,

    /// Bitrate target based on delay factor for all video streams.
    /// Hasn't been tested with multiple video streams, but
    /// current design is simply to divide bitrate equally.
    target_bitrate_on_delay: Bitrate,

    /// Used in additive mode to track last control time, influences
    /// calculation of added value according to gcc section 5.5
    last_increase_on_delay: Option<time::Instant>,
    last_decrease_on_delay: time::Instant,

    /// Bitrate target based on loss for all video streams.
    target_bitrate_on_loss: Bitrate,

    last_increase_on_loss: time::Instant,
    last_decrease_on_loss: time::Instant,

    /// Exponential moving average, updated when bitrate is
    /// decreased
    ema: ExponentialMovingAverage,

    last_control_op: BandwidthEstimationOp,

    min_bitrate: Bitrate,
    max_bitrate: Bitrate,

    detector: Detector,

    clock_entry: Option<gst::SingleShotClockId>,

    // Implemented like a leaky bucket
    buffers: VecDeque<gst::Buffer>,
    // Number of bits remaining from previous burst
    budget_offset: i64,

    flow_return: Result<gst::FlowSuccess, gst::FlowError>,
    last_push: time::Instant,
}

impl Default for State {
    fn default() -> Self {
        Self {
            target_bitrate_on_delay: DEFAULT_ESTIMATED_BITRATE,
            target_bitrate_on_loss: DEFAULT_ESTIMATED_BITRATE,
            last_increase_on_loss: time::Instant::now(),
            last_decrease_on_loss: time::Instant::now(),
            ema: Default::default(),
            last_increase_on_delay: None,
            last_decrease_on_delay: time::Instant::now(),
            min_bitrate: DEFAULT_MIN_BITRATE,
            max_bitrate: DEFAULT_MAX_BITRATE,
            detector: Detector::new(),
            buffers: Default::default(),
            estimated_bitrate: DEFAULT_ESTIMATED_BITRATE,
            last_control_op: BandwidthEstimationOp::Increase("Initial increase".into()),
            flow_return: Err(gst::FlowError::Flushing),
            clock_entry: None,
            last_push: time::Instant::now(),
            budget_offset: 0,
        }
    }
}

impl State {
    // 4. sending engine implementing a "leaky bucket"
    fn create_buffer_list(&mut self, bwe: &super::BandwidthEstimator) -> gst::BufferList {
        let now = time::Instant::now();
        let elapsed = Duration::from_std(now - self.last_push).unwrap();
        let mut budget = (elapsed.num_nanoseconds().unwrap())
            .mul_div_round(
                self.estimated_bitrate as i64,
                gst::ClockTime::SECOND.nseconds() as i64,
            )
            .unwrap()
            + self.budget_offset;
        let total_budget = budget;
        let mut remaining = self.buffers.iter().map(|b| b.size() as f64).sum::<f64>() * 8.;
        let total_size = remaining;

        let mut list = gst::BufferList::new();
        let mutlist = list.get_mut().unwrap();

        // Leak the bucket so it can hold at most 30ms of data
        let maximum_remaining_bits = 30. * self.estimated_bitrate as f64 / 1000.;
        let mut leaked = false;
        while (budget > 0 || remaining > maximum_remaining_bits) && !self.buffers.is_empty() {
            let buf = self.buffers.pop_back().unwrap();
            let n_bits = buf.size() * 8;

            leaked = budget <= 0 && remaining > maximum_remaining_bits;
            mutlist.add(buf);
            budget -= n_bits as i64;
            remaining -= n_bits as f64;
        }

        gst::trace!(
            CAT,
            obj: bwe,
            "{} bitrate: {}ps budget: {}/{} sending: {} Remaining: {}/{}",
            pdur(&elapsed),
            human_kbits(self.estimated_bitrate),
            human_kbits(budget as f64),
            human_kbits(total_budget as f64),
            human_kbits(list.calculate_size() as f64 * 8.),
            human_kbits(remaining),
            human_kbits(total_size)
        );

        self.last_push = now;
        self.budget_offset = if !leaked { budget } else { 0 };

        list
    }

    fn compute_increased_rate(&mut self, bwe: &super::BandwidthEstimator) -> Option<Bitrate> {
        let now = time::Instant::now();
        let target_bitrate = self.target_bitrate_on_delay as f64;
        let effective_bitrate = self.detector.effective_bitrate();
        let time_since_last_update_ms = match self.last_increase_on_delay {
            None => 0.,
            Some(prev) => {
                if now - prev < *DELAY_UPDATE_INTERVAL {
                    return None;
                }

                (now - prev).as_millis() as f64
            }
        };

        if effective_bitrate as f64 - target_bitrate as f64 > 5. * target_bitrate / 100. {
            gst::info!(
                CAT,
                "Effective rate {} >> target bitrate {} - we should avoid that \
                 as much as possible fine tuning the encoder",
                human_kbits(effective_bitrate),
                human_kbits(target_bitrate)
            );
        }

        self.last_increase_on_delay = Some(now);
        if self.ema.estimate_is_close(effective_bitrate) {
            let bits_per_frame = target_bitrate / 30.;
            let packets_per_frame = f64::ceil(bits_per_frame / (1200. * 8.));
            let avg_packet_size_bits = bits_per_frame / packets_per_frame;

            let rtt_ms = self.detector.rtt().num_milliseconds() as f64;
            let response_time_ms = 100. + rtt_ms;
            let alpha = 0.5 * f64::min(time_since_last_update_ms / response_time_ms, 1.0);
            let threshold_on_effective_bitrate = 1.5 * effective_bitrate as f64;
            let increase = f64::max(
                1000.0f64,
                f64::min(
                    alpha * avg_packet_size_bits,
                    // Stuffing should ensure that the effective bitrate is not
                    // < target bitrate, still, make sure to always increase
                    // the bitrate by a minimum amount of 160.bits
                    f64::max(
                        threshold_on_effective_bitrate - self.target_bitrate_on_delay as f64,
                        160.,
                    ),
                ),
            );

            /* Additive increase */
            self.last_control_op =
                BandwidthEstimationOp::Increase(format!("Additive ({})", human_kbits(increase)));
            Some((self.target_bitrate_on_delay as f64 + increase) as Bitrate)
        } else {
            let eta = 1.08_f64.powf(f64::min(time_since_last_update_ms / 1000., 1.0));
            let rate = eta * self.target_bitrate_on_delay as f64;

            self.ema = Default::default();

            assert!(
                rate >= self.target_bitrate_on_delay as f64,
                "Increase: {} - {}",
                rate,
                eta
            );

            // Maximum increase to 1.5 * received rate
            let received_max = 1.5 * effective_bitrate as f64;

            if rate > received_max && received_max > self.target_bitrate_on_delay as f64 {
                gst::log!(
                    CAT,
                    obj: bwe,
                    "Increasing == received_max rate: {}ps",
                    human_kbits(received_max)
                );

                self.last_control_op = BandwidthEstimationOp::Increase(format!(
                    "Using 1.5*effective_rate({})",
                    human_kbits(effective_bitrate)
                ));
                Some(received_max as Bitrate)
            } else if rate < self.target_bitrate_on_delay as f64 {
                gst::log!(
                    CAT,
                    obj: bwe,
                    "Rate < target, returning {}ps",
                    human_kbits(self.target_bitrate_on_delay)
                );

                None
            } else {
                gst::log!(
                    CAT,
                    obj: bwe,
                    "Increase mult {eta}x{}ps={}ps",
                    human_kbits(self.target_bitrate_on_delay),
                    human_kbits(rate)
                );

                self.last_control_op =
                    BandwidthEstimationOp::Increase(format!("Multiplicative x{eta}"));
                Some(rate as Bitrate)
            }
        }
    }

    fn set_bitrate(
        &mut self,
        bwe: &super::BandwidthEstimator,
        bitrate: Bitrate,
        controller_type: ControllerType,
    ) -> bool {
        let prev_bitrate = Bitrate::min(self.target_bitrate_on_delay, self.target_bitrate_on_loss);

        match controller_type {
            ControllerType::Loss => {
                self.target_bitrate_on_loss = bitrate.clamp(self.min_bitrate, self.max_bitrate)
            }

            ControllerType::Delay => {
                self.target_bitrate_on_delay = bitrate.clamp(self.min_bitrate, self.max_bitrate)
            }
        }

        let target_bitrate =
            Bitrate::min(self.target_bitrate_on_delay, self.target_bitrate_on_loss)
                .clamp(self.min_bitrate, self.max_bitrate);

        if target_bitrate == prev_bitrate {
            return false;
        }

        gst::info!(
            CAT,
            obj: bwe,
            "{controller_type:?}: {}ps => {}ps ({:?}) - effective bitrate: {}",
            human_kbits(prev_bitrate),
            human_kbits(target_bitrate),
            self.last_control_op,
            human_kbits(self.detector.effective_bitrate()),
        );

        self.estimated_bitrate = target_bitrate;

        true
    }

    fn loss_control(&mut self, bwe: &super::BandwidthEstimator) -> bool {
        let loss_ratio = self.detector.loss_ratio();
        let now = time::Instant::now();

        if loss_ratio > LOSS_DECREASE_THRESHOLD
            && (now - self.last_decrease_on_loss) > *LOSS_UPDATE_INTERVAL
        {
            let factor = 1. - (0.5 * loss_ratio);

            self.last_control_op =
                BandwidthEstimationOp::Decrease(format!("High loss detected ({loss_ratio:2}"));
            self.last_decrease_on_loss = now;

            self.set_bitrate(
                bwe,
                (self.target_bitrate_on_loss as f64 * factor) as Bitrate,
                ControllerType::Loss,
            )
        } else if loss_ratio < LOSS_INCREASE_THRESHOLD
            && (now - self.last_increase_on_loss) > *LOSS_UPDATE_INTERVAL
        {
            self.last_control_op = BandwidthEstimationOp::Increase("Low loss".into());
            self.last_increase_on_loss = now;

            self.set_bitrate(
                bwe,
                (self.target_bitrate_on_loss as f64 * LOSS_INCREASE_FACTOR) as Bitrate,
                ControllerType::Loss,
            )
        } else {
            false
        }
    }

    fn delay_control(&mut self, bwe: &super::BandwidthEstimator) -> bool {
        match self.detector.usage {
            NetworkUsage::Normal => match self.last_control_op {
                BandwidthEstimationOp::Increase(..) | BandwidthEstimationOp::Hold => {
                    if let Some(bitrate) = self.compute_increased_rate(bwe) {
                        return self.set_bitrate(bwe, bitrate, ControllerType::Delay);
                    }
                }
                _ => (),
            },
            NetworkUsage::Over => {
                let now = time::Instant::now();
                if now - self.last_decrease_on_delay > *DELAY_UPDATE_INTERVAL {
                    let effective_bitrate = self.detector.effective_bitrate();
                    let target =
                        (self.estimated_bitrate as f64 * 0.95).min(BETA * effective_bitrate as f64);
                    self.last_control_op = BandwidthEstimationOp::Decrease(format!(
                        "Over use detected {:#?}",
                        self.detector
                    ));
                    self.ema.update(effective_bitrate);
                    self.last_decrease_on_delay = now;

                    return self.set_bitrate(bwe, target as Bitrate, ControllerType::Delay);
                }
            }
            NetworkUsage::Under => {
                if let BandwidthEstimationOp::Increase(..) = self.last_control_op {
                    if let Some(bitrate) = self.compute_increased_rate(bwe) {
                        return self.set_bitrate(bwe, bitrate, ControllerType::Delay);
                    }
                }
            }
        }

        self.last_control_op = BandwidthEstimationOp::Hold;

        false
    }
}

pub struct BandwidthEstimator {
    state: Mutex<State>,

    srcpad: gst::Pad,
    sinkpad: gst::Pad,
}

impl BandwidthEstimator {
    fn push_list(&self, list: gst::BufferList) -> Result<gst::FlowSuccess, gst::FlowError> {
        let res = self.srcpad.push_list(list);

        self.state.lock().unwrap().flow_return = res;

        res
    }

    fn start_task(&self, bwe: &super::BandwidthEstimator) -> Result<(), glib::BoolError> {
        let weak_bwe = bwe.downgrade();
        let weak_pad = self.srcpad.downgrade();
        let clock = gst::SystemClock::obtain();

        bwe.imp().state.lock().unwrap().clock_entry =
            Some(clock.new_single_shot_id(clock.time().unwrap() + dur2ts(*BURST_TIME)));

        self.srcpad.start_task(move || {
            let pause = || {
                if let Some(pad) = weak_pad.upgrade() {
                    let _ = pad.pause_task();
                }
            };
            let bwe = weak_bwe
                .upgrade()
                .expect("bwe destroyed while its srcpad task is still running?");

            let lock_state = || bwe.imp().state.lock().unwrap();

            let clock_entry = match lock_state().clock_entry.take() {
                Some(id) => id,
                _ => {
                    gst::info!(CAT, "Pausing task as our clock entry is not set anymore");
                    return pause();
                }
            };

            if let (Err(err), _) = clock_entry.wait() {
                match err {
                    gst::ClockError::Early => (),
                    _ => {
                        gst::error!(CAT, "Got error {err:?} on the clock, pausing task");

                        lock_state().flow_return = Err(gst::FlowError::Flushing);

                        return pause();
                    }
                }
            }
            let list = {
                let mut state = lock_state();
                clock
                    .single_shot_id_reinit(
                        &clock_entry,
                        clock.time().unwrap() + dur2ts(*BURST_TIME),
                    )
                    .unwrap();
                state.clock_entry = Some(clock_entry);
                state.create_buffer_list(&bwe)
            };

            if !list.is_empty() {
                if let Err(err) = bwe.imp().push_list(list) {
                    gst::error!(CAT, obj: &bwe, "pause task, reason: {err:?}");
                    return pause();
                }
            }
        })?;

        Ok(())
    }

    fn src_activatemode(
        &self,
        _pad: &gst::Pad,
        bwe: &super::BandwidthEstimator,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if let gst::PadMode::Push = mode {
            if active {
                self.state.lock().unwrap().flow_return = Ok(gst::FlowSuccess::Ok);
                self.start_task(bwe)?;
            } else {
                let mut state = self.state.lock().unwrap();
                state.flow_return = Err(gst::FlowError::Flushing);
                drop(state);

                self.srcpad.stop_task()?;
            }

            Ok(())
        } else {
            Err(gst::LoggableError::new(
                *CAT,
                glib::bool_error!("Unsupported pad mode {mode:?}"),
            ))
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for BandwidthEstimator {
    const NAME: &'static str = "GstRTPGCCBwE";
    type Type = super::BandwidthEstimator;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|_pad, parent, mut buffer| {
                BandwidthEstimator::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |this, _element| {
                        let mut state = this.state.lock().unwrap();
                        let mutbuf = buffer.make_mut();
                        mutbuf.set_pts(None);
                        mutbuf.set_dts(None);
                        state.buffers.push_front(buffer);

                        state.flow_return
                    },
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::PROXY_ALLOCATION)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .event_function(|pad, parent, event| {
                BandwidthEstimator::catch_panic_pad_function(
                    parent,
                    || false,
                    |this, bwe| {
                        if let Some(structure) = event.structure() {
                            if structure.name() == "RTPTWCCPackets" {
                                let varray = structure.get::<glib::ValueArray>("packets").unwrap();
                                let mut packets = varray
                                    .iter()
                                    .filter_map(|s| {
                                        Packet::from_structure(&s.get::<gst::Structure>().unwrap())
                                    })
                                    .collect::<Vec<Packet>>();

                                let bitrate_changed = {
                                    let mut state = this.state.lock().unwrap();

                                    state.detector.update(&mut packets);
                                    if !state.delay_control(bwe) {
                                        state.loss_control(bwe)
                                    } else {
                                        true
                                    }
                                };

                                if bitrate_changed {
                                    bwe.notify("estimated-bitrate")
                                }
                            }
                        }

                        pad.event_default(Some(bwe), event)
                    },
                )
            })
            .activatemode_function(|pad, parent, mode, active| {
                BandwidthEstimator::catch_panic_pad_function(
                    parent,
                    || {
                        Err(gst::loggable_error!(
                            CAT,
                            "Panic activating src pad with mode"
                        ))
                    },
                    |this, bwe| this.src_activatemode(pad, bwe, mode, active),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::PROXY_ALLOCATION)
            .build();

        Self {
            state: Default::default(),
            srcpad,
            sinkpad,
        }
    }
}

impl ObjectImpl for BandwidthEstimator {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                /*
                 *  gcc:estimated-bitrate:
                 *
                 * Currently computed network bitrate, should be used
                 * to set encoders bitrate.
                 */
                glib::ParamSpecUInt::new(
                    "estimated-bitrate",
                    "Estimated Bitrate",
                    "Currently estimated bitrate. Can be set before starting
                     the element to configure the starting bitrate, in which case the
                     encoder should also use it as target bitrate",
                    1,
                    u32::MAX as u32,
                    DEFAULT_MIN_BITRATE as u32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt::new(
                    "min-bitrate",
                    "Minimal Bitrate",
                    "Minimal bitrate to use (in bit/sec) when computing it through the bandwidth estimation algorithm",
                    1,
                    u32::MAX as u32,
                    DEFAULT_MIN_BITRATE,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt::new(
                    "max-bitrate",
                    "Maximal Bitrate",
                    "Maximal bitrate to use (in bit/sec) when computing it through the bandwidth estimation algorithm",
                    1,
                    u32::MAX as u32,
                    DEFAULT_MAX_BITRATE,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "min-bitrate" => {
                let mut state = self.state.lock().unwrap();
                state.min_bitrate = value.get::<u32>().expect("type checked upstream");
            }
            "max-bitrate" => {
                let mut state = self.state.lock().unwrap();
                state.max_bitrate = value.get::<u32>().expect("type checked upstream");
            }
            "estimated-bitrate" => {
                let mut state = self.state.lock().unwrap();
                let bitrate = value.get::<u32>().expect("type checked upstream");
                state.target_bitrate_on_delay = bitrate;
                state.target_bitrate_on_loss = bitrate;
                state.estimated_bitrate = bitrate;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "min-bitrate" => {
                let state = self.state.lock().unwrap();
                state.min_bitrate.to_value()
            }
            "max-bitrate" => {
                let state = self.state.lock().unwrap();
                state.max_bitrate.to_value()
            }
            "estimated-bitrate" => {
                let state = self.state.lock().unwrap();
                state.estimated_bitrate.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for BandwidthEstimator {}

impl ElementImpl for BandwidthEstimator {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Google Congestion Control bandwidth estimator",
                "Network/WebRTC/RTP/Filter",
                "Estimates current network bandwidth using the Google Congestion Control algorithm \
                 notifying about it through the 'bitrate' property",
                "Thibault Saunier <tsaunier@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder_full()
                .structure(gst::Structure::builder("application/x-rtp").build())
                .build();

            let sinkpad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let srcpad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sinkpad_template, srcpad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}
