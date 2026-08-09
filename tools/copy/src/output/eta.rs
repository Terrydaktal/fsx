use crate::domain::{
    EtaProgressTotals, EtaWorkSummary, EtaWorkload, MediaKind, TransferProgressRates, ETA_BUCKET_S,
    ETA_FILE_BIN_COUNT, ETA_MAX_REGIME_HYPOTHESES, ETA_SAMPLE_RETENTION_S,
};
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Default)]
pub(crate) struct EtaTelemetry {
    pub(crate) write_bytes: Option<u64>,
    pub(crate) write_complete: Option<u64>,
    pub(crate) read_bytes: Option<u64>,
    pub(crate) read_complete: Option<u64>,
    /// True only when all four counters share one transfer-scoped baseline.
    /// Process I/O and diskstats are different accounting domains and must not
    /// be subtracted to manufacture a backlog.
    pub(crate) aligned_pipeline_counters: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EtaActivity {
    WarmingUp,
    Active,
    WorkloadBound,
    PipelineBound,
    TransientStall,
    Stalled,
    Complete,
}

impl EtaActivity {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::WarmingUp => "warming-up",
            Self::Active => "active",
            Self::WorkloadBound => "workload-bound",
            Self::PipelineBound => "pipeline-bound",
            Self::TransientStall => "stalled briefly",
            Self::Stalled => "stalled",
            Self::Complete => "complete",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct EtaEstimate {
    pub(crate) p10_s: Option<f64>,
    pub(crate) p50_s: Option<f64>,
    pub(crate) p90_s: Option<f64>,
    pub(crate) confidence: f64,
    pub(crate) activity: EtaActivity,
}

#[derive(Clone, Copy)]
struct EtaPipelineState {
    write_backlog: u64,
    read_backlog: u64,
    write_rate_bps: Option<f64>,
    read_rate_bps: Option<f64>,
}

impl EtaPipelineState {
    fn eta_s(self, remaining_bytes: u64) -> Option<f64> {
        let write_eta = self
            .write_rate_bps
            .map(|rate| self.write_backlog.saturating_add(remaining_bytes) as f64 / rate.max(1.0));
        let read_eta = self
            .read_rate_bps
            .map(|rate| self.read_backlog.saturating_add(remaining_bytes) as f64 / rate.max(1.0));
        write_eta.into_iter().chain(read_eta).max_by(f64::total_cmp)
    }

    fn sampled_eta_s(self, remaining_bytes: u64, rng: &mut EtaRng, log_sigma: f64) -> Option<f64> {
        let sample_rate = |rate: Option<f64>, rng: &mut EtaRng| {
            rate.map(|rate| (rate.ln() + rng.normal() * log_sigma).exp())
        };
        Self {
            write_rate_bps: sample_rate(self.write_rate_bps, rng),
            read_rate_bps: sample_rate(self.read_rate_bps, rng),
            ..self
        }
        .eta_s(remaining_bytes)
    }
}

#[derive(Clone, Copy)]
struct EtaWorkSample {
    elapsed_s: f64,
    bytes: u64,
    progress: EtaProgressTotals,
}

#[derive(Clone, Copy)]
struct EtaRegimeHypothesis {
    run_length_s: f64,
    mean_log_bps: f64,
    m2_log_bps: f64,
    observations: f64,
    log_probability: f64,
}

impl EtaRegimeHypothesis {
    fn variance(self) -> f64 {
        if self.observations > 1.0 {
            (self.m2_log_bps / (self.observations - 1.0)).max(0.04 * 0.04)
        } else {
            0.40 * 0.40
        }
    }

    fn observe(self, observation: f64, dt_s: f64, log_probability: f64) -> Self {
        let observations = self.observations + 1.0;
        let delta = observation - self.mean_log_bps;
        let mean_log_bps = self.mean_log_bps + delta / observations;
        let m2_log_bps = self.m2_log_bps + delta * (observation - mean_log_bps);
        Self {
            run_length_s: self.run_length_s + dt_s,
            mean_log_bps,
            m2_log_bps,
            observations,
            log_probability,
        }
    }
}

#[derive(Default)]
struct EtaRegimeModel {
    hypotheses: Vec<EtaRegimeHypothesis>,
    observations: u64,
    observed_s: f64,
    zero_progress_s: f64,
}

pub(crate) fn log_sum_exp(values: impl IntoIterator<Item = f64>) -> f64 {
    let values: Vec<f64> = values.into_iter().filter(|v| v.is_finite()).collect();
    let max = values.iter().copied().max_by(f64::total_cmp);
    let Some(max) = max else {
        return f64::NEG_INFINITY;
    };
    max + values
        .iter()
        .map(|value| (*value - max).exp())
        .sum::<f64>()
        .ln()
}

pub(crate) fn student_t_log_likelihood(observation: f64, mean: f64, variance: f64) -> f64 {
    const NU: f64 = 4.0;
    let variance = variance.max(0.04 * 0.04);
    let residual = observation - mean;
    -0.5 * variance.ln() - 0.5 * (NU + 1.0) * (1.0 + residual * residual / (NU * variance)).ln()
}

#[derive(Clone, Copy)]
struct EtaRng {
    state: u64,
}

impl Default for EtaRng {
    fn default() -> Self {
        Self {
            state: 0x9e37_79b9_7f4a_7c15,
        }
    }
}

impl EtaRng {
    fn next_u64(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 7;
        value ^= value >> 9;
        value ^= value << 8;
        self.state = value;
        value
    }

    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64).clamp(1e-12, 1.0 - 1e-12)
    }

    fn normal(&mut self) -> f64 {
        let u1 = self.uniform();
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

impl EtaRegimeModel {
    fn observe(&mut self, rate_bps: f64, dt_s: f64) {
        if !rate_bps.is_finite() || rate_bps <= 0.0 || dt_s <= 0.0 {
            return;
        }
        self.observed_s += dt_s;
        let observation = rate_bps.max(1.0).ln();
        self.observations = self.observations.saturating_add(1);
        if self.hypotheses.is_empty() {
            self.hypotheses.push(EtaRegimeHypothesis {
                run_length_s: dt_s,
                mean_log_bps: observation,
                m2_log_bps: 0.0,
                observations: 1.0,
                log_probability: 0.0,
            });
            return;
        }

        let hazard = (1.0 - (-dt_s / 240.0).exp()).clamp(0.0001, 0.5);
        let mut growth = Vec::with_capacity(self.hypotheses.len());
        let mut change_terms = Vec::with_capacity(self.hypotheses.len());
        for hypothesis in self.hypotheses.iter().copied() {
            let likelihood = student_t_log_likelihood(
                observation,
                hypothesis.mean_log_bps,
                hypothesis.variance(),
            );
            growth.push(hypothesis.observe(
                observation,
                dt_s,
                hypothesis.log_probability + (1.0 - hazard).ln() + likelihood,
            ));
            change_terms.push(hypothesis.log_probability + hazard.ln() + likelihood);
        }

        let change_log_probability = log_sum_exp(change_terms);
        let mut next = Vec::with_capacity(growth.len() + 1);
        next.push(EtaRegimeHypothesis {
            run_length_s: dt_s,
            mean_log_bps: observation,
            m2_log_bps: 0.0,
            observations: 1.0,
            log_probability: change_log_probability,
        });
        next.extend(growth);
        let normalizer = log_sum_exp(next.iter().map(|hypothesis| hypothesis.log_probability));
        for hypothesis in &mut next {
            hypothesis.log_probability -= normalizer;
        }
        next.sort_by(|a, b| b.log_probability.total_cmp(&a.log_probability));
        next.truncate(ETA_MAX_REGIME_HYPOTHESES);
        self.hypotheses = next;
    }

    fn observe_zero(&mut self, dt_s: f64) {
        if dt_s > 0.0 {
            self.observed_s += dt_s;
            self.zero_progress_s += dt_s;
        }
    }

    fn current_rate_bps(&self) -> Option<f64> {
        self.hypotheses
            .first()
            .map(|hypothesis| hypothesis.mean_log_bps.exp())
            .filter(|rate| rate.is_finite() && *rate > 0.0)
    }

    fn sample_rate_bps(&self, rng: &mut EtaRng) -> Option<f64> {
        if self.hypotheses.is_empty() {
            return None;
        }
        let draw = rng.uniform();
        let mut cumulative = 0.0;
        let selected = self
            .hypotheses
            .iter()
            .find(|hypothesis| {
                cumulative += hypothesis.log_probability.exp();
                cumulative >= draw
            })
            .unwrap_or(&self.hypotheses[0]);
        Some(
            (selected.mean_log_bps + rng.normal() * selected.variance().sqrt())
                .exp()
                .max(1.0),
        )
    }

    fn recent_change_probability(&self) -> f64 {
        self.hypotheses
            .iter()
            .filter(|hypothesis| hypothesis.run_length_s <= 2.5)
            .map(|hypothesis| hypothesis.log_probability.exp())
            .sum::<f64>()
            .clamp(0.0, 1.0)
    }

    fn log_rate_sigma(&self) -> f64 {
        self.hypotheses
            .first()
            .map(|hypothesis| hypothesis.variance().sqrt())
            .unwrap_or(1.0)
            .clamp(0.12, 1.5)
    }

    fn zero_probability(&self) -> f64 {
        if self.observed_s <= 0.0 {
            0.0
        } else {
            (self.zero_progress_s / self.observed_s).clamp(0.0, 1.0)
        }
    }
}

const ETA_MIB: f64 = 1024.0 * 1024.0;
const ETA_FALLBACK_FILE_COST_S: f64 = 0.001;
const ETA_FALLBACK_DIR_COST_S: f64 = 0.002;

#[derive(Clone, Copy)]
struct EtaLinearCost {
    theta: [f64; 2],
    covariance: [[f64; 2]; 2],
    noise_var: f64,
    observations: f64,
}

impl Default for EtaLinearCost {
    fn default() -> Self {
        Self {
            theta: [0.0; 2],
            covariance: [[10.0, 0.0], [0.0, 10.0]],
            noise_var: 1.0,
            observations: 0.0,
        }
    }
}

impl EtaLinearCost {
    fn basis(count: u64, bytes: u64) -> [f64; 2] {
        [count as f64, bytes as f64 / ETA_MIB]
    }

    fn predict(self, count: u64, bytes: u64) -> f64 {
        let x = Self::basis(count, bytes);
        (self.theta[0] * x[0] + self.theta[1] * x[1]).max(0.0)
    }

    fn observe(&mut self, count: u64, bytes: u64, seconds: f64) {
        if count == 0 || !seconds.is_finite() || seconds <= 0.0 {
            return;
        }
        let x = Self::basis(count, bytes);
        let px = [
            self.covariance[0][0] * x[0] + self.covariance[0][1] * x[1],
            self.covariance[1][0] * x[0] + self.covariance[1][1] * x[1],
        ];
        let forgetting = 0.985;
        let denominator = forgetting + x[0] * px[0] + x[1] * px[1];
        if !denominator.is_finite() || denominator <= 0.0 {
            return;
        }
        let gain = [px[0] / denominator, px[1] / denominator];
        let prediction = self.predict(count, bytes);
        let error = seconds - prediction;
        self.theta[0] = (self.theta[0] + gain[0] * error).max(0.0);
        self.theta[1] = (self.theta[1] + gain[1] * error).max(0.0);
        for (row, gain_value) in gain.iter().enumerate() {
            for (col, px_value) in px.iter().enumerate() {
                self.covariance[row][col] =
                    (self.covariance[row][col] - gain_value * px_value) / forgetting;
            }
        }
        self.noise_var = (self.noise_var * 0.98 + error * error * 0.02).clamp(1e-6, 3600.0);
        self.observations += 1.0;
    }

    fn sample(self, count: u64, bytes: u64, rng: &mut EtaRng) -> f64 {
        let mean = self.predict(count, bytes);
        if mean <= 0.0 {
            return 0.0;
        }
        let x = Self::basis(count, bytes);
        let px = [
            self.covariance[0][0] * x[0] + self.covariance[0][1] * x[1],
            self.covariance[1][0] * x[0] + self.covariance[1][1] * x[1],
        ];
        let parameter_var = (x[0] * px[0] + x[1] * px[1]).max(0.0);
        let sigma = (self.noise_var * (1.0 + parameter_var)).sqrt().min(60.0);
        (mean + rng.normal() * sigma).max(0.0)
    }
}

#[derive(Clone, Copy)]
struct EtaCostModel {
    file: [EtaLinearCost; ETA_FILE_BIN_COUNT],
    dirs: EtaLinearCost,
    metadata: EtaLinearCost,
}

impl Default for EtaCostModel {
    fn default() -> Self {
        Self {
            file: [EtaLinearCost::default(); ETA_FILE_BIN_COUNT],
            dirs: EtaLinearCost::default(),
            metadata: EtaLinearCost::default(),
        }
    }
}

impl EtaCostModel {
    fn observe(&mut self, dt_s: f64, progress: EtaProgressTotals) {
        if dt_s <= 0.0 || !dt_s.is_finite() {
            return;
        }
        let mut active: Vec<(usize, u64, u64, f64)> = Vec::new();
        for idx in 0..ETA_FILE_BIN_COUNT {
            let count = progress.file_bins[idx];
            if count > 0 {
                active.push((
                    idx,
                    count,
                    progress.file_bytes[idx],
                    self.file[idx].predict(count, progress.file_bytes[idx]),
                ));
            }
        }
        if progress.dirs > 0 {
            active.push((
                ETA_FILE_BIN_COUNT,
                progress.dirs,
                0,
                self.dirs.predict(progress.dirs, 0),
            ));
        }
        if progress.metadata > 0 {
            active.push((
                ETA_FILE_BIN_COUNT + 1,
                progress.metadata,
                0,
                self.metadata.predict(progress.metadata, 0),
            ));
        }
        if active.is_empty() {
            return;
        }
        let weight_sum: f64 = active
            .iter()
            .map(|(_, count, bytes, prediction)| {
                prediction.max(*count as f64 + *bytes as f64 / ETA_MIB)
            })
            .sum();
        for (idx, count, bytes, prediction) in active {
            let weight = prediction.max(count as f64 + bytes as f64 / ETA_MIB);
            let seconds = dt_s * weight / weight_sum.max(f64::EPSILON);
            if idx == ETA_FILE_BIN_COUNT {
                self.dirs.observe(count, bytes, seconds);
            } else if idx == ETA_FILE_BIN_COUNT + 1 {
                self.metadata.observe(count, bytes, seconds);
            } else {
                self.file[idx].observe(count, bytes, seconds);
            }
        }
    }

    fn summary_eta(&self, summary: EtaWorkSummary, capacity_bps: f64) -> (f64, f64) {
        let byte_eta =
            summary.file_bytes.iter().copied().sum::<u64>() as f64 / capacity_bps.max(1.0);
        let mut class_eta = 0.0;
        for idx in 0..ETA_FILE_BIN_COUNT {
            let fallback = summary.file_bins[idx] as f64 * ETA_FALLBACK_FILE_COST_S;
            class_eta += self.file[idx]
                .predict(summary.file_bins[idx], summary.file_bytes[idx])
                .max(fallback);
        }
        class_eta += self
            .dirs
            .predict(summary.dirs, 0)
            .max(summary.dirs as f64 * ETA_FALLBACK_DIR_COST_S);
        class_eta += self
            .metadata
            .predict(summary.metadata, 0)
            .max(summary.metadata as f64 * ETA_FALLBACK_FILE_COST_S);
        (byte_eta.max(class_eta), class_eta)
    }

    fn remaining_eta_s(
        &self,
        workload: &EtaWorkload,
        progress: EtaProgressTotals,
        capacity_bps: f64,
    ) -> (f64, f64) {
        let mut producer_eta = 0.0;
        let mut class_eta = 0.0;
        workload.for_each_remaining_segment(progress, |summary| {
            let (segment_eta, segment_class_eta) = self.summary_eta(summary, capacity_bps);
            producer_eta += segment_eta;
            class_eta += segment_class_eta;
        });
        (producer_eta, class_eta)
    }

    fn sample_summary(&self, summary: EtaWorkSummary, capacity_bps: f64, rng: &mut EtaRng) -> f64 {
        let byte_eta =
            summary.file_bytes.iter().copied().sum::<u64>() as f64 / capacity_bps.max(1.0);
        let mut class_eta = 0.0;
        for idx in 0..ETA_FILE_BIN_COUNT {
            class_eta += self.file[idx]
                .sample(summary.file_bins[idx], summary.file_bytes[idx], rng)
                .max(summary.file_bins[idx] as f64 * ETA_FALLBACK_FILE_COST_S);
        }
        class_eta += self
            .dirs
            .sample(summary.dirs, 0, rng)
            .max(summary.dirs as f64 * ETA_FALLBACK_DIR_COST_S);
        class_eta += self
            .metadata
            .sample(summary.metadata, 0, rng)
            .max(summary.metadata as f64 * ETA_FALLBACK_FILE_COST_S);
        byte_eta.max(class_eta)
    }

    fn ordered_pipeline_eta(
        &self,
        workload: &EtaWorkload,
        progress: EtaProgressTotals,
        capacity_bps: f64,
        drain_bps: Option<f64>,
        initial_backlog: u64,
    ) -> f64 {
        let mut producer_elapsed_s = 0.0;
        let mut downstream_queue = initial_backlog as f64;
        workload.for_each_remaining_segment(progress, |summary| {
            let segment_bytes = summary.file_bytes.iter().copied().sum::<u64>() as f64;
            let segment_elapsed_s = self.summary_eta(summary, capacity_bps).0;
            if segment_elapsed_s <= 0.0 || !segment_elapsed_s.is_finite() {
                return;
            }
            if let Some(drain_bps) = drain_bps.filter(|rate| *rate > 0.0) {
                if segment_bytes > 0.0 {
                    let producer_bps = segment_bytes / segment_elapsed_s;
                    downstream_queue += segment_bytes;
                    downstream_queue = (downstream_queue - drain_bps * segment_elapsed_s)
                        .max(0.0)
                        .max(if producer_bps > drain_bps {
                            downstream_queue
                        } else {
                            0.0
                        });
                } else {
                    downstream_queue = (downstream_queue - drain_bps * segment_elapsed_s).max(0.0);
                }
            }
            producer_elapsed_s += segment_elapsed_s;
        });
        producer_elapsed_s
            + drain_bps
                .filter(|rate| *rate > 0.0)
                .map(|rate| downstream_queue / rate)
                .unwrap_or(0.0)
    }

    fn confidence(&self) -> f64 {
        let weight: f64 = self
            .file
            .iter()
            .map(|model| model.observations)
            .sum::<f64>()
            + self.dirs.observations
            + self.metadata.observations;
        (1.0 - (-weight / 500.0).exp()).clamp(0.0, 1.0)
    }
}

fn media_key(media: MediaKind) -> &'static str {
    match media {
        MediaKind::Hdd => "hdd",
        MediaKind::Nvme => "nvme",
        MediaKind::Other => "other",
    }
}

fn eta_prior_path() -> Option<PathBuf> {
    if env::var_os("COPY_RS_DISABLE_ETA_PRIORS").is_some() {
        return None;
    }
    if let Some(state_home) = env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(state_home).join("copy-rs/eta-priors.v2"));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".local/state/copy-rs/eta-priors.v2"))
}

fn flatten_cost(model: EtaLinearCost, values: &mut Vec<String>) {
    values.extend(
        [
            model.theta[0],
            model.theta[1],
            model.covariance[0][0],
            model.covariance[0][1],
            model.covariance[1][0],
            model.covariance[1][1],
            model.noise_var,
            model.observations,
        ]
        .into_iter()
        .map(|value| format!("{value:.17}")),
    );
}

fn parse_cost(values: &mut impl Iterator<Item = f64>) -> Option<EtaLinearCost> {
    Some(EtaLinearCost {
        theta: [values.next()?, values.next()?],
        covariance: [
            [values.next()?, values.next()?],
            [values.next()?, values.next()?],
        ],
        noise_var: values.next()?.clamp(1e-6, 3600.0),
        observations: values.next()?.max(0.0),
    })
}

fn load_eta_prior(profile_key: u64, media: MediaKind) -> Option<(EtaCostModel, Option<f64>, f64)> {
    let path = eta_prior_path()?;
    let prefix = format!("v2|{profile_key}|{}|", media_key(media));
    let contents = fs::read_to_string(path).ok()?;
    let line = contents.lines().find(|line| line.starts_with(&prefix))?;
    let mut values = line[prefix.len()..]
        .split('|')
        .filter_map(|value| value.parse::<f64>().ok());
    let sequence_bps = values.next().filter(|value| *value > 0.0);
    let sequence_weight = values.next()?.max(0.0);
    let mut file = [EtaLinearCost::default(); ETA_FILE_BIN_COUNT];
    for model in &mut file {
        *model = parse_cost(&mut values)?;
    }
    let dirs = parse_cost(&mut values)?;
    let metadata = parse_cost(&mut values)?;
    Some((
        EtaCostModel {
            file,
            dirs,
            metadata,
        },
        sequence_bps,
        sequence_weight,
    ))
}

fn save_eta_prior(
    profile_key: u64,
    media: MediaKind,
    model: EtaCostModel,
    sequence_bps: Option<f64>,
    sequence_weight: f64,
) {
    static PRIOR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let Some(path) = eta_prior_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let lock = PRIOR_LOCK.get_or_init(|| Mutex::new(()));
    let Ok(_guard) = lock.lock() else {
        return;
    };
    let prefix = format!("v2|{profile_key}|{}|", media_key(media));
    let mut lines: Vec<String> = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.starts_with(&prefix))
        .map(str::to_owned)
        .collect();
    let mut values = vec![format!("{:.17}", sequence_bps.unwrap_or(0.0).max(0.0))];
    values.push(format!("{sequence_weight:.17}"));
    for file in model.file {
        flatten_cost(file, &mut values);
    }
    flatten_cost(model.dirs, &mut values);
    flatten_cost(model.metadata, &mut values);
    lines.push(format!("{prefix}{}", values.join("|")));
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let temp = parent.join(format!("eta-priors.v2.tmp.{}", std::process::id()));
    if fs::write(&temp, format!("{}\n", lines.join("\n"))).is_ok() {
        let _ = fs::rename(temp, path);
    }
}

#[derive(Default)]
pub(crate) struct TransferEtaEstimator {
    samples: VecDeque<(f64, u64)>,
    display_finish_s: Option<f64>,
    last_elapsed_s: Option<f64>,
    last_done: Option<u64>,
    last_model_sample: Option<EtaWorkSample>,
    regime_start_s: f64,
    change_since_s: Option<(f64, f64)>,
    zero_progress_s: f64,
    regime_model: EtaRegimeModel,
    cost_model: EtaCostModel,
    sequential_bps: Option<f64>,
    sequential_weight: f64,
    last_regime_change_s: Option<f64>,
    last_estimate: Option<EtaEstimate>,
    rng: EtaRng,
    prior_loaded: bool,
    prior_media: Option<MediaKind>,
    prior_profile_key: u64,
}

pub(crate) fn rate_over_window(
    samples: &VecDeque<(f64, u64)>,
    start_s: f64,
    now_s: f64,
    window_s: f64,
) -> Option<f64> {
    let cutoff_s = (now_s - window_s).max(start_s);
    let mut first: Option<(f64, u64)> = None;
    for &(t, bytes) in samples.iter().filter(|(t, _)| *t >= start_s) {
        if t <= cutoff_s {
            first = Some((t, bytes));
        } else if first.is_none() {
            first = Some((t, bytes));
            break;
        } else {
            break;
        }
    }
    let last = samples.iter().rev().find(|(t, _)| *t >= start_s).copied()?;
    let first = first?;
    let dt = last.0 - first.0;
    if dt <= 1e-6 {
        return None;
    }
    Some(last.1.saturating_sub(first.1) as f64 / dt)
}

pub(crate) fn median_rate(rates: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    let mut values: Vec<f64> = rates
        .into_iter()
        .flatten()
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[values.len() / 2])
}

impl TransferEtaEstimator {
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update(
        &mut self,
        done: Option<u64>,
        total: u64,
        instant_bps: Option<f64>,
        elapsed_s: f64,
        finalize_line: bool,
        workload: Option<EtaWorkload>,
        progress: Option<EtaProgressTotals>,
    ) -> Option<f64> {
        self.update_with_telemetry(
            done,
            total,
            instant_bps,
            elapsed_s,
            finalize_line,
            workload,
            progress,
            EtaTelemetry::default(),
            TransferProgressRates::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_with_telemetry(
        &mut self,
        done: Option<u64>,
        total: u64,
        instant_bps: Option<f64>,
        elapsed_s: f64,
        finalize_line: bool,
        workload: Option<EtaWorkload>,
        progress: Option<EtaProgressTotals>,
        telemetry: EtaTelemetry,
        rates: TransferProgressRates,
    ) -> Option<f64> {
        let done = done?;
        if total == 0 {
            return None;
        }
        let done_clamped = done.min(total);
        let remain = total.saturating_sub(done_clamped);
        if remain == 0 {
            self.last_estimate = Some(EtaEstimate {
                p10_s: Some(0.0),
                p50_s: Some(0.0),
                p90_s: Some(0.0),
                confidence: 1.0,
                activity: EtaActivity::Complete,
            });
            if finalize_line {
                if !cfg!(test) {
                    if let Some(media) = self.prior_media {
                        save_eta_prior(
                            self.prior_profile_key,
                            media,
                            self.cost_model,
                            self.sequential_bps,
                            self.sequential_weight,
                        );
                    }
                }
                *self = Self::default();
            }
            return Some(0.0);
        }

        if self
            .last_elapsed_s
            .map(|previous| elapsed_s < previous)
            .unwrap_or(false)
        {
            *self = Self::default();
        }
        if !self.prior_loaded {
            if let Some(workload) = workload.as_ref() {
                self.prior_loaded = true;
                self.prior_media = Some(workload.media);
                self.prior_profile_key = workload.profile_key;
                if !cfg!(test) {
                    if let Some((cost_model, sequential_bps, sequential_weight)) =
                        load_eta_prior(workload.profile_key, workload.media)
                    {
                        self.cost_model = cost_model;
                        self.sequential_bps = sequential_bps;
                        self.sequential_weight = sequential_weight;
                    }
                }
            }
        }
        let dt = self
            .last_elapsed_s
            .map(|previous| (elapsed_s - previous).max(0.0))
            .unwrap_or(0.0);
        if self.last_done == Some(done_clamped) {
            self.zero_progress_s += dt;
        } else {
            self.zero_progress_s = 0.0;
        }
        self.last_elapsed_s = Some(elapsed_s);
        self.last_done = Some(done_clamped);

        self.samples.push_back((elapsed_s, done_clamped));
        while self
            .samples
            .front()
            .map(|(t, _)| elapsed_s - *t > ETA_SAMPLE_RETENTION_S)
            .unwrap_or(false)
        {
            self.samples.pop_front();
        }
        let current_work_sample = EtaWorkSample {
            elapsed_s,
            bytes: done_clamped,
            progress: progress.unwrap_or_default(),
        };
        if let Some(previous) = self.last_model_sample {
            let model_dt = current_work_sample.elapsed_s - previous.elapsed_s;
            if model_dt >= ETA_BUCKET_S {
                let delta_bytes = current_work_sample.bytes.saturating_sub(previous.bytes);
                let delta_progress = current_work_sample.progress.delta(previous.progress);
                if delta_bytes > 0 {
                    let rate = delta_bytes as f64 / model_dt;
                    self.regime_model.observe(rate, model_dt);
                    let files = delta_progress.file_count();
                    let average_file_size = if files > 0 { delta_bytes / files } else { 0 };
                    let sequential_sample = if workload.is_some() && progress.is_some() {
                        delta_bytes >= 4 * 1024 * 1024
                            && (files == 0 || average_file_size >= 1024 * 1024)
                    } else {
                        true
                    };
                    self.update_capacity(rate, sequential_sample, elapsed_s, model_dt);
                    if workload.is_some() && progress.is_some() {
                        self.cost_model.observe(model_dt, delta_progress);
                    }
                } else {
                    self.regime_model.observe_zero(model_dt);
                    if workload.is_some()
                        && progress.is_some()
                        && (delta_progress.file_count() > 0 || delta_progress.dirs > 0)
                    {
                        self.cost_model.observe(model_dt, delta_progress);
                    }
                }
                self.last_model_sample = Some(current_work_sample);
            }
        } else {
            self.last_model_sample = Some(current_work_sample);
        }

        if self.zero_progress_s > 0.0 {
            if let Some(finish) = &mut self.display_finish_s {
                *finish += dt;
                if self.zero_progress_s >= 5.0 {
                    self.last_estimate = Some(EtaEstimate {
                        p10_s: None,
                        p50_s: None,
                        p90_s: None,
                        confidence: 0.0,
                        activity: EtaActivity::Stalled,
                    });
                    return None;
                }
                let eta = (*finish - elapsed_s).max(0.0).round();
                self.last_estimate = Some(EtaEstimate {
                    p10_s: Some(eta),
                    p50_s: Some(eta),
                    p90_s: Some(eta),
                    confidence: 0.25,
                    activity: EtaActivity::TransientStall,
                });
                return Some(eta);
            }
        }

        const WARMUP_S: f64 = 5.0;
        let sample_span_s = match (self.samples.front(), self.samples.back()) {
            (Some((first_t, _)), Some((last_t, _))) => (last_t - first_t).max(0.0),
            _ => 0.0,
        };
        let recent_rate = median_rate([
            rate_over_window(&self.samples, self.regime_start_s, elapsed_s, 1.2),
            rate_over_window(&self.samples, self.regime_start_s, elapsed_s, 2.5),
            rate_over_window(&self.samples, self.regime_start_s, elapsed_s, 5.0),
        ])
        .or_else(|| instant_bps.filter(|rate| rate.is_finite() && *rate > 0.0));

        if let Some(rate) = recent_rate {
            if self.regime_model.hypotheses.is_empty() {
                self.regime_model
                    .observe(rate, sample_span_s.max(ETA_BUCKET_S));
            }
        }

        if self.zero_progress_s >= 5.0 {
            self.last_estimate = Some(EtaEstimate {
                p10_s: None,
                p50_s: None,
                p90_s: None,
                confidence: 0.0,
                activity: EtaActivity::Stalled,
            });
            return None;
        }
        if self.zero_progress_s >= 1.5 {
            if let Some(finish) = &mut self.display_finish_s {
                *finish += dt;
                let eta = (*finish - elapsed_s).max(0.0).round();
                self.last_estimate = Some(EtaEstimate {
                    p10_s: Some(eta),
                    p50_s: Some(eta),
                    p90_s: Some(eta),
                    confidence: 0.25,
                    activity: EtaActivity::TransientStall,
                });
                return Some(eta);
            }
            return None;
        }

        let current_rate = recent_rate
            .or_else(|| self.regime_model.current_rate_bps())
            .filter(|rate| rate.is_finite() && *rate > 0.0);
        let rate = match current_rate {
            Some(rate) => rate,
            _ if sample_span_s < WARMUP_S => {
                self.last_estimate = Some(EtaEstimate {
                    p10_s: None,
                    p50_s: None,
                    p90_s: None,
                    confidence: 0.0,
                    activity: EtaActivity::WarmingUp,
                });
                return None;
            }
            _ => return None,
        };

        let capacity_bps = self.sequential_bps.unwrap_or(rate).max(1.0);
        let byte_eta_s = remain as f64 / capacity_bps;
        let (producer_eta_s, class_eta_s) = workload
            .as_ref()
            .zip(progress)
            .map(|(workload, progress)| {
                self.cost_model
                    .remaining_eta_s(workload, progress, capacity_bps)
            })
            .unwrap_or((byte_eta_s, 0.0));
        let workload_model_active = workload.is_some()
            && progress.is_some()
            && (self.cost_model.confidence() > 0.05 || class_eta_s > 0.0);
        let producer_eta_s = if workload_model_active {
            producer_eta_s
        } else {
            producer_eta_s.max(remain as f64 / rate)
        };
        let pipeline_state = self.pipeline_state(telemetry, rates, elapsed_s);
        let pipeline_stage_eta_s = pipeline_state.and_then(|pipeline| pipeline.eta_s(remain));
        let ordered_pipeline_eta_s = workload.as_ref().zip(progress).zip(pipeline_state).map(
            |((workload, progress), pipeline)| {
                self.cost_model.ordered_pipeline_eta(
                    workload,
                    progress,
                    capacity_bps,
                    pipeline.write_rate_bps,
                    pipeline.write_backlog,
                )
            },
        );
        let pipeline_eta_s = pipeline_stage_eta_s
            .into_iter()
            .chain(ordered_pipeline_eta_s)
            .max_by(f64::total_cmp);
        let model_eta_s = pipeline_eta_s
            .map(|pipeline| producer_eta_s.max(pipeline))
            .unwrap_or(producer_eta_s);
        let model_eta_s = if model_eta_s.is_finite() && model_eta_s > 0.0 {
            model_eta_s
        } else {
            return None;
        };
        let workload_confidence = workload
            .as_ref()
            .zip(progress)
            .map(|_| self.cost_model.confidence())
            .unwrap_or(0.0);
        let pipeline_confidence = if pipeline_eta_s.is_some() { 0.45 } else { 0.0 };
        let confidence = (((1.0 - (-(self.regime_model.observations as f64) / 20.0).exp()) * 0.65
            + workload_confidence * 0.25
            + pipeline_confidence * 0.10)
            * (1.0 - 0.5 * self.regime_model.zero_probability()))
        .clamp(0.0, 1.0);
        let remaining_summary = workload
            .as_ref()
            .zip(progress)
            .map(|(workload, progress)| workload.remaining_ordered(progress));
        let mut forecast_samples = Vec::with_capacity(128);
        for _ in 0..128 {
            let sampled_regime_rate = self.regime_model.sample_rate_bps(&mut self.rng);
            let sampled_capacity = self
                .sequential_bps
                .or(sampled_regime_rate)
                .unwrap_or(rate)
                .max(1.0)
                * (self.rng.normal() * self.regime_model.log_rate_sigma() * 0.35).exp();
            let sampled_producer = remaining_summary
                .map(|summary| {
                    self.cost_model
                        .sample_summary(summary, sampled_capacity, &mut self.rng)
                })
                .unwrap_or(remain as f64 / sampled_capacity);
            let sampled_pipeline = pipeline_state.and_then(|pipeline| {
                pipeline.sampled_eta_s(remain, &mut self.rng, self.regime_model.log_rate_sigma())
            });
            forecast_samples.push(
                sampled_producer
                    .max(sampled_pipeline.unwrap_or(0.0))
                    .max(0.0),
            );
        }
        forecast_samples.sort_by(f64::total_cmp);
        let p10_s = forecast_samples
            .get(forecast_samples.len() * 10 / 100)
            .copied()
            .unwrap_or(model_eta_s);
        let p50_model_s = forecast_samples
            .get(forecast_samples.len() * 50 / 100)
            .copied()
            .unwrap_or(model_eta_s);
        let p90_s = forecast_samples
            .get(forecast_samples.len() * 90 / 100)
            .copied()
            .unwrap_or(model_eta_s)
            .max(p50_model_s);
        let model_eta_s = p50_model_s.max(0.0);
        let workload_bound = class_eta_s > byte_eta_s * 0.25;
        let pipeline_bound = pipeline_eta_s
            .map(|pipeline| pipeline > producer_eta_s * 1.25)
            .unwrap_or(false);
        let activity = if pipeline_bound {
            EtaActivity::PipelineBound
        } else if workload_bound {
            EtaActivity::WorkloadBound
        } else {
            EtaActivity::Active
        };
        let model_finish_s = elapsed_s + model_eta_s;
        let regime_changed = self.regime_model.recent_change_probability() > 0.65
            || self
                .last_regime_change_s
                .map(|changed| elapsed_s - changed <= 2.0)
                .unwrap_or(false);
        if self.display_finish_s.is_none() || regime_changed {
            self.display_finish_s = Some(model_finish_s);
        } else if let Some(finish) = &mut self.display_finish_s {
            let smoothing_s = 2.0 + 4.0 * (1.0 - confidence);
            let alpha = if dt > 0.0 {
                1.0 - (-dt / smoothing_s).exp()
            } else {
                0.0
            };
            *finish += alpha * (model_finish_s - *finish);
        }

        let displayed_s = self
            .display_finish_s
            .map(|finish| (finish - elapsed_s).max(0.0).round());
        self.last_estimate = Some(EtaEstimate {
            p10_s: Some(p10_s),
            p50_s: Some(p50_model_s),
            p90_s: Some(p90_s),
            confidence,
            activity,
        });
        if finalize_line {
            let result = displayed_s;
            if !cfg!(test) {
                if let Some(media) = self.prior_media {
                    save_eta_prior(
                        self.prior_profile_key,
                        media,
                        self.cost_model,
                        self.sequential_bps,
                        self.sequential_weight,
                    );
                }
            }
            *self = Self::default();
            return result;
        }
        displayed_s
    }

    fn update_capacity(&mut self, rate: f64, sequential_sample: bool, elapsed_s: f64, dt_s: f64) {
        if !sequential_sample || !rate.is_finite() || rate <= 0.0 {
            return;
        }
        let previous = self.sequential_bps;
        let ratio = previous.map(|old| rate / old.max(1.0));
        let candidate_s = ratio.and_then(|ratio| {
            if !(0.125..=8.0).contains(&ratio) {
                Some(1.2)
            } else if !(0.60..=1.67).contains(&ratio) {
                Some(2.5)
            } else {
                None
            }
        });
        let mut regime_changed = false;
        if let Some(required_s) = candidate_s {
            if self.change_since_s.is_none() {
                self.change_since_s = Some(((elapsed_s - dt_s).max(0.0), required_s));
            }
            if self
                .change_since_s
                .map(|(started, required)| elapsed_s - started >= required)
                .unwrap_or(false)
            {
                self.regime_start_s = self
                    .change_since_s
                    .map(|(started, _)| started)
                    .unwrap_or((elapsed_s - dt_s).max(0.0));
                self.change_since_s = None;
                regime_changed = true;
            }
        } else {
            self.change_since_s = None;
        }

        if regime_changed {
            // A confirmed capacity change invalidates the old cache-backed
            // rate. Do not let the lifetime estimate contaminate the new one.
            self.sequential_bps = Some(rate);
            self.sequential_weight = dt_s;
            self.regime_model = EtaRegimeModel::default();
            self.regime_model.observe(rate, dt_s);
            self.last_regime_change_s = Some(elapsed_s);
        } else {
            let alpha = 0.20;
            self.sequential_bps = Some(match self.sequential_bps {
                Some(previous) => previous * (1.0 - alpha) + rate * alpha,
                None => rate,
            });
            self.sequential_weight += dt_s;
        }
    }

    fn pipeline_state(
        &self,
        telemetry: EtaTelemetry,
        rates: TransferProgressRates,
        elapsed_s: f64,
    ) -> Option<EtaPipelineState> {
        let write_backlog = if telemetry.aligned_pipeline_counters {
            match (telemetry.write_bytes, telemetry.write_complete) {
                (Some(submitted), Some(completed)) => submitted.saturating_sub(completed),
                _ => 0,
            }
        } else {
            0
        };
        let read_backlog = if telemetry.aligned_pipeline_counters {
            match (telemetry.read_bytes, telemetry.read_complete) {
                (Some(submitted), Some(completed)) => submitted.saturating_sub(completed),
                _ => 0,
            }
        } else {
            0
        };
        let write_rate = rates
            .write_complete_bps
            .filter(|rate| *rate > 0.0)
            .or_else(|| {
                if elapsed_s > 1.0 {
                    telemetry
                        .write_complete
                        .map(|completed| completed as f64 / elapsed_s)
                        .filter(|rate| *rate > 0.0)
                } else {
                    None
                }
            });
        let read_rate = rates
            .read_complete_bps
            .filter(|rate| *rate > 0.0)
            .or_else(|| {
                if elapsed_s > 1.0 {
                    telemetry
                        .read_complete
                        .map(|completed| completed as f64 / elapsed_s)
                        .filter(|rate| *rate > 0.0)
                } else {
                    None
                }
            });
        let mut stage = EtaPipelineState {
            write_backlog,
            read_backlog,
            write_rate_bps: write_rate,
            read_rate_bps: read_rate,
        };
        if telemetry.aligned_pipeline_counters {
            let cumulative_write_rate = if elapsed_s > 1.0 {
                telemetry
                    .write_complete
                    .map(|completed| completed as f64 / elapsed_s)
            } else {
                None
            };
            if let Some(rate) = cumulative_write_rate.filter(|rate| *rate > 0.0) {
                stage.write_rate_bps = Some(
                    stage
                        .write_rate_bps
                        .map(|current| current.min(rate))
                        .unwrap_or(rate),
                );
            }
        }
        (stage.write_rate_bps.is_some() || stage.read_rate_bps.is_some()).then_some(stage)
    }

    pub(crate) fn last_estimate(&self) -> Option<EtaEstimate> {
        self.last_estimate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EtaProgressTotals, EtaWorkload, TransferProgressRates};

    #[test]
    fn ordered_workload_tracks_out_of_order_completion() {
        let workload = EtaWorkload::from_file_sizes(
            &[4 * 1024, 8 * 1024, 2 * 1024 * 1024],
            MediaKind::Hdd,
            42,
        );
        let mut progress = EtaProgressTotals::default();
        assert_eq!(
            workload
                .remaining_ordered(progress)
                .file_bins
                .iter()
                .sum::<u64>(),
            3
        );

        workload.mark_operation(1);
        let remaining = workload.remaining_ordered(progress);
        assert_eq!(remaining.file_bins.iter().sum::<u64>(), 2);
        assert_eq!(remaining.file_bytes[0], 4 * 1024);
        assert_eq!(remaining.file_bytes[5], 2 * 1024 * 1024);

        workload.mark_operation(0);
        progress.file_bins[0] = 2;
        progress.file_bytes[0] = 12 * 1024;
        assert_eq!(
            workload
                .remaining_ordered(progress)
                .file_bins
                .iter()
                .sum::<u64>(),
            1
        );
        workload.mark_operation(2);
        assert_eq!(
            workload
                .remaining_ordered(progress)
                .file_bins
                .iter()
                .sum::<u64>(),
            0
        );
    }

    #[test]
    fn eta_estimator_tracks_stable_throughput() {
        let mut estimator = TransferEtaEstimator::default();
        let mib = 1024.0 * 1024.0;
        let rate = 100.0 * mib;
        let total = 20 * 1024 * 1024 * 1024u64;
        let mut eta = None;

        for tick in 1..=60 {
            let elapsed = tick as f64 * 0.2;
            let done = (elapsed * rate) as u64;
            eta = estimator.update(Some(done), total, Some(rate), elapsed, false, None, None);
        }

        let eta = eta.expect("ETA should be available after warmup");
        assert!(eta > 150.0 && eta < 250.0, "unexpected stable ETA: {eta}");
    }

    #[test]
    fn eta_estimator_uses_remaining_file_composition_after_small_file_phase() {
        let mut estimator = TransferEtaEstimator::default();
        let mib = 1024.0 * 1024.0;
        let kib = 1024u64;
        let fast_rate = 100.0 * mib;
        let mut workload = EtaWorkload::default();
        workload.file_bins[0] = 1_000;
        workload.file_bytes[0] = workload.file_bins[0] * 4 * kib;
        workload.file_bins[7] = 10;
        workload.file_bytes[7] = workload.file_bins[7] * 1024 * 1024 * 1024;
        let total = workload.file_bytes.iter().sum::<u64>();
        let mut progress = EtaProgressTotals::default();
        let mut done = 0u64;

        for tick in 1..=50 {
            let elapsed = tick as f64 * 0.2;
            done = (elapsed * fast_rate) as u64;
            let _ = estimator.update(
                Some(done),
                total,
                Some(fast_rate),
                elapsed,
                false,
                Some(workload.clone()),
                Some(progress),
            );
        }

        let mut eta = None;
        for tick in 51..=100 {
            let elapsed = tick as f64 * 0.2;
            done = done.saturating_add(4 * kib * 2);
            progress.file_bins[0] += 2;
            progress.file_bytes[0] += 4 * kib * 2;
            eta = estimator.update(
                Some(done),
                total,
                Some((4 * kib * 2) as f64 / 0.2),
                elapsed,
                false,
                Some(workload.clone()),
                Some(progress),
            );
        }

        let eta = eta.expect("ETA should remain available during small-file phase");
        assert!(
            eta > 100.0 && eta < 1_000.0,
            "unexpected composition ETA: {eta}"
        );
    }

    #[test]
    fn eta_estimator_reacts_to_cache_to_disk_slowdown() {
        let mut estimator = TransferEtaEstimator::default();
        let mib = 1024.0 * 1024.0;
        let fast_rate = 800.0 * mib;
        let slow_rate = 9.726 * mib;
        let total = 100 * 1024 * 1024 * 1024u64;

        for tick in 1..=50 {
            let elapsed = tick as f64 * 0.2;
            let done = (elapsed * fast_rate) as u64;
            let _ = estimator.update(
                Some(done),
                total,
                Some(fast_rate),
                elapsed,
                false,
                None,
                None,
            );
        }

        let mut eta_after_slowdown = None;
        for tick in 51..=80 {
            let elapsed = tick as f64 * 0.2;
            let done = (10.0 * fast_rate + (elapsed - 10.0) * slow_rate) as u64;
            eta_after_slowdown = estimator.update(
                Some(done),
                total,
                Some(slow_rate),
                elapsed,
                false,
                None,
                None,
            );
        }

        let eta = eta_after_slowdown.expect("ETA should remain available after slowdown");
        assert!(eta > 5_000.0, "slowdown was not detected quickly: {eta}s");
    }

    #[test]
    fn eta_estimator_freezes_short_stalls_and_reports_long_stalls() {
        let mut estimator = TransferEtaEstimator::default();
        let rate = 100.0 * 1024.0 * 1024.0;
        let total = 20 * 1024 * 1024 * 1024u64;
        let mut eta_before_stall = None;

        for tick in 1..=60 {
            let elapsed = tick as f64 * 0.2;
            let done = (elapsed * rate) as u64;
            eta_before_stall =
                estimator.update(Some(done), total, Some(rate), elapsed, false, None, None);
        }

        let eta_before_stall = eta_before_stall.expect("ETA should be available before stall");
        let done = (60.0 * 0.2 * rate) as u64;
        let mut eta_after_short_stall = None;
        for tick in 61..=68 {
            let elapsed = tick as f64 * 0.2;
            eta_after_short_stall =
                estimator.update(Some(done), total, Some(0.0), elapsed, false, None, None);
        }
        let eta_after_short_stall =
            eta_after_short_stall.expect("short stall should retain the current ETA");
        assert!(
            (eta_after_short_stall - eta_before_stall).abs() <= 1.0,
            "short stall changed ETA: before={eta_before_stall}, after={eta_after_short_stall}"
        );

        let mut long_stall_eta = Some(0.0);
        for tick in 69..=90 {
            let elapsed = tick as f64 * 0.2;
            long_stall_eta =
                estimator.update(Some(done), total, Some(0.0), elapsed, false, None, None);
        }
        assert!(
            long_stall_eta.is_none(),
            "long stall should report an unavailable ETA"
        );
    }

    #[test]
    fn eta_estimator_exposes_ordered_percentiles_for_mixed_workload() {
        let mut estimator = TransferEtaEstimator::default();
        let kib = 1024u64;
        let mut workload = EtaWorkload::default();
        workload.file_bins[0] = 2_000;
        workload.file_bytes[0] = workload.file_bins[0] * 4 * kib;
        workload.file_bins[7] = 2;
        workload.file_bytes[7] = 2 * 1024 * 1024 * 1024;
        let total = workload.file_bytes.iter().sum::<u64>();
        let mut progress = EtaProgressTotals::default();
        let mut done = 0u64;

        for tick in 1..=80 {
            let elapsed = tick as f64 * 0.2;
            done = done.saturating_add(4 * 4 * kib);
            progress.file_bins[0] += 4;
            progress.file_bytes[0] += 4 * 4 * kib;
            let _ = estimator.update(
                Some(done),
                total,
                Some(16.0 * kib as f64 / 0.2),
                elapsed,
                false,
                Some(workload.clone()),
                Some(progress),
            );
        }

        let estimate = estimator.last_estimate().expect("mixed-workload estimate");
        let p10 = estimate.p10_s.expect("p10");
        let p50 = estimate.p50_s.expect("p50");
        let p90 = estimate.p90_s.expect("p90");
        assert!(p10 <= p50 && p50 <= p90);
        assert!(p90.is_finite() && p90 > 0.0);
        assert!(estimate.confidence > 0.0);
    }

    #[test]
    fn eta_estimator_accounts_for_downstream_pipeline_backlog() {
        let mut estimator = TransferEtaEstimator::default();
        let mib = 1024.0 * 1024.0;
        let total = 10 * 1024 * 1024 * 1024u64;
        let mut estimate = None;

        for tick in 1..=50 {
            let elapsed = tick as f64 * 0.2;
            let done = (elapsed * 100.0 * mib) as u64;
            let write_complete = (elapsed * mib) as u64;
            estimate = estimator.update_with_telemetry(
                Some(done),
                total,
                Some(100.0 * mib),
                elapsed,
                false,
                None,
                None,
                EtaTelemetry {
                    write_bytes: Some(done),
                    write_complete: Some(write_complete),
                    aligned_pipeline_counters: true,
                    ..EtaTelemetry::default()
                },
                TransferProgressRates {
                    write_complete_bps: Some(mib),
                    ..TransferProgressRates::default()
                },
            );
        }

        assert!(estimate.expect("pipeline estimate") > 300.0);
        assert!(estimator
            .last_estimate()
            .map(|value| value.activity == EtaActivity::PipelineBound)
            .unwrap_or(false));
    }
}
