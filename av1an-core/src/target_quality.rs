use std::{
    cmp,
    cmp::Ordering,
    collections::HashSet,
    convert::TryInto,
    path::PathBuf,
    thread::available_parallelism,
};

use clap::ValueEnum;
use ffmpeg::format::Pixel;
use serde::{Deserialize, Serialize};
use splines::{Interpolation, Key, Spline};
use tracing::{debug, error};

use crate::{
    broker::EncoderCrash,
    chunk::Chunk,
    progress_bar::update_mp_msg,
    settings::ProbingStats,
    vmaf::{read_weighted_vmaf, VmafScoreMethod},
    Encoder,
    ProbingSpeed,
};

const SCORE_TOLERANCE: f64 = 0.01;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetQuality {
    pub vmaf_res:              String,
    pub vmaf_scaler:           String,
    pub vmaf_filter:           Option<String>,
    pub vmaf_threads:          usize,
    pub model:                 Option<PathBuf>,
    pub probing_rate:          usize,
    pub probing_speed:         Option<u8>,
    pub probes:                u32,
    pub target:                f64,
    pub min_q:                 u32,
    pub max_q:                 u32,
    pub encoder:               Encoder,
    pub pix_format:            Pixel,
    pub temp:                  String,
    pub workers:               usize,
    pub video_params:          Vec<String>,
    pub vspipe_args:           Vec<String>,
    pub probe_slow:            bool,
    pub probing_vmaf_features: Vec<VmafFeature>,
    pub probing_stats:         Option<ProbingStats>,
    pub probing_percent:       Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
pub enum VmafFeature {
    #[value(name = "default")]
    Default,
    #[value(name = "weighted")]
    Weighted,
    #[value(name = "neg")]
    Neg,
    #[value(name = "motionless")]
    Motionless,
}

impl TargetQuality {
    fn per_shot_target_quality(
        &self,
        chunk: &Chunk,
        worker_id: Option<usize>,
    ) -> anyhow::Result<u32> {
        // History of probe results as quantizer-score pairs
        let mut quantizer_score_history: Vec<(u32, f64)> = vec![];

        let update_progress_bar = |last_q: u32| {
            if let Some(worker_id) = worker_id {
                update_mp_msg(
                    worker_id,
                    format!(
                        "Targeting Quality {target} - Testing {last_q}",
                        target = self.target
                    ),
                );
            }
        };

        // Initialize quantizer limits from specified minimum and maximum quantizers
        let mut lower_quantizer_limit = self.min_q;
        let mut upper_quantizer_limit = self.max_q;

        let score_method = if let Some(percent) = self.probing_percent {
            VmafScoreMethod::Percentile(percent)
        } else if let Some(stats) = self.probing_stats {
            match stats {
                ProbingStats::Mean => VmafScoreMethod::Mean,
                ProbingStats::Median => VmafScoreMethod::Median,
                ProbingStats::HarmonicMean => VmafScoreMethod::HarmonicMean,
            }
        } else {
            VmafScoreMethod::Percentile(0.01)
        };

        loop {
            let next_quantizer = predict_quantizer(
                lower_quantizer_limit,
                upper_quantizer_limit,
                &quantizer_score_history,
                self.target,
            );

            if quantizer_score_history
                .iter()
                .any(|(quantizer, _)| *quantizer == next_quantizer)
            {
                // Predicted quantizer has already been probed
                let (last_quantizer, last_score) = quantizer_score_history
                    .iter()
                    .find(|(quantizer, _)| *quantizer == next_quantizer)
                    .unwrap();
                log_probes(
                    &mut quantizer_score_history.clone(),
                    self.target,
                    chunk.frames() as u32,
                    self.probing_rate as u32,
                    self.probing_speed,
                    &chunk.name(),
                    *last_quantizer,
                    *last_score,
                    SkipProbingReason::None,
                );
                break;
            }

            update_progress_bar(next_quantizer);

            let probe_path = self.vmaf_probe(chunk, next_quantizer as usize)?;
            let score = read_weighted_vmaf(&probe_path, score_method)?;
            let score_within_tolerance = within_tolerance(score, self.target);

            quantizer_score_history.push((next_quantizer, score));

            if score_within_tolerance || quantizer_score_history.len() >= self.probes as usize {
                log_probes(
                    &mut quantizer_score_history,
                    self.target,
                    chunk.frames() as u32,
                    self.probing_rate as u32,
                    self.probing_speed,
                    &chunk.name(),
                    next_quantizer,
                    score,
                    if score_within_tolerance {
                        SkipProbingReason::WithinTolerance
                    } else {
                        SkipProbingReason::ProbeLimitReached
                    },
                );
                break;
            }

            if score > self.target {
                lower_quantizer_limit = (next_quantizer + 1).min(upper_quantizer_limit);
            } else {
                upper_quantizer_limit = (next_quantizer - 1).max(lower_quantizer_limit);
            }

            // Ensure quantizer limits are valid
            if lower_quantizer_limit > upper_quantizer_limit {
                log_probes(
                    &mut quantizer_score_history,
                    self.target,
                    chunk.frames() as u32,
                    self.probing_rate as u32,
                    self.probing_speed,
                    &chunk.name(),
                    next_quantizer,
                    score,
                    if score > self.target {
                        SkipProbingReason::QuantizerTooHigh
                    } else {
                        SkipProbingReason::QuantizerTooLow
                    },
                );
                break;
            }
        }

        let history_within_tolerance: Vec<&(u32, f64)> = quantizer_score_history
            .iter()
            .filter(|(_, score)| within_tolerance(*score, self.target))
            .collect();

        let final_quantizer_score = if !history_within_tolerance.is_empty() {
            history_within_tolerance.iter().max_by_key(|(quantizer, _)| *quantizer).unwrap()
        } else {
            quantizer_score_history
                .iter()
                .min_by(|(_, score1), (_, score2)| {
                    let difference1 = (score1 - self.target).abs();
                    let difference2 = (score2 - self.target).abs();
                    difference1.partial_cmp(&difference2).unwrap_or(Ordering::Equal)
                })
                .unwrap()
        };

        Ok(final_quantizer_score.0)
    }

    fn vmaf_probe(&self, chunk: &Chunk, q: usize) -> Result<PathBuf, Box<EncoderCrash>> {
        let vmaf_threads = if self.vmaf_threads == 0 {
            vmaf_auto_threads(self.workers)
        } else {
            self.vmaf_threads
        };

        let cmd = self.encoder.probe_cmd(
            self.temp.clone(),
            chunk.index,
            q,
            self.pix_format,
            self.probing_rate,
            self.probing_speed,
            vmaf_threads,
            self.video_params.clone(),
            self.probe_slow,
        );

        let future = async {
            let mut source = if let [pipe_cmd, args @ ..] = &*chunk.source_cmd {
                tokio::process::Command::new(pipe_cmd)
                    .args(args)
                    .stderr(if cfg!(windows) {
                        std::process::Stdio::null()
                    } else {
                        std::process::Stdio::piped()
                    })
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            } else {
                unreachable!()
            };

            let source_pipe_stdout: std::process::Stdio =
                source.stdout.take().unwrap().try_into().unwrap();

            let mut source_pipe = if let [ffmpeg, args @ ..] = &*cmd.0 {
                tokio::process::Command::new(ffmpeg)
                    .args(args)
                    .stdin(source_pipe_stdout)
                    .stdout(std::process::Stdio::piped())
                    .stderr(if cfg!(windows) {
                        std::process::Stdio::null()
                    } else {
                        std::process::Stdio::piped()
                    })
                    .spawn()
                    .unwrap()
            } else {
                unreachable!()
            };

            let source_pipe_stdout: std::process::Stdio =
                source_pipe.stdout.take().unwrap().try_into().unwrap();

            let enc_pipe = if let [cmd, args @ ..] = &*cmd.1 {
                tokio::process::Command::new(cmd.as_ref())
                    .args(args.iter().map(AsRef::as_ref))
                    .stdin(source_pipe_stdout)
                    .stdout(std::process::Stdio::piped())
                    .stderr(if cfg!(windows) {
                        std::process::Stdio::null()
                    } else {
                        std::process::Stdio::piped()
                    })
                    .spawn()
                    .unwrap()
            } else {
                unreachable!()
            };

            let source_pipe_output = source_pipe.wait_with_output().await.unwrap();

            // TODO: Expand EncoderCrash to handle io errors as well
            let enc_output = enc_pipe.wait_with_output().await.unwrap();

            if !enc_output.status.success() {
                let e = EncoderCrash {
                    exit_status:        enc_output.status,
                    stdout:             enc_output.stdout.into(),
                    stderr:             enc_output.stderr.into(),
                    source_pipe_stderr: source_pipe_output.stderr.into(),
                    ffmpeg_pipe_stderr: None,
                };
                error!("[chunk {}] {}", chunk.index, e);
                return Err(e);
            }

            Ok(())
        };

        let rt = tokio::runtime::Builder::new_current_thread().enable_io().build().unwrap();
        rt.block_on(future)?;

        let extension = match self.encoder {
            crate::encoder::Encoder::x264 => "264",
            crate::encoder::Encoder::x265 => "hevc",
            _ => "ivf",
        };

        let probe_name = std::path::Path::new(&chunk.temp)
            .join("split")
            .join(format!("v_{index:05}_{q}.{extension}", index = chunk.index));
        let fl_path = std::path::Path::new(&chunk.temp)
            .join("split")
            .join(format!("{index}.json", index = chunk.index));

        let features: HashSet<_> = self.probing_vmaf_features.iter().copied().collect();
        let use_weighted = features.contains(&VmafFeature::Weighted);
        let use_neg = features.contains(&VmafFeature::Neg);
        let disable_motion = features.contains(&VmafFeature::Motionless);

        let default_neg_model = PathBuf::from("vmaf_v0.6.1neg.json");
        let model = if use_neg && self.model.is_none() {
            Some(&default_neg_model)
        } else {
            self.model.as_ref()
        };

        if use_weighted {
            crate::vmaf::run_vmaf_weighted(
                &probe_name,
                chunk.source_cmd.as_slice(),
                self.vspipe_args.clone(),
                &fl_path,
                model,
                &self.vmaf_res,
                &self.vmaf_scaler,
                self.probing_rate,
                self.vmaf_filter.as_deref(),
                self.vmaf_threads,
                chunk.frame_rate,
                disable_motion,
            )
            .map_err(|e| {
                Box::new(EncoderCrash {
                    exit_status:        std::process::ExitStatus::default(),
                    source_pipe_stderr: String::new().into(),
                    ffmpeg_pipe_stderr: None,
                    stderr:             format!("VMAF calculation failed: {e}").into(),
                    stdout:             String::new().into(),
                })
            })?;
        } else {
            crate::vmaf::run_vmaf(
                &probe_name,
                chunk.source_cmd.as_slice(),
                self.vspipe_args.clone(),
                &fl_path,
                model,
                &self.vmaf_res,
                &self.vmaf_scaler,
                self.probing_rate,
                self.vmaf_filter.as_deref(),
                self.vmaf_threads,
                chunk.frame_rate,
                disable_motion,
            )?;
        }

        Ok(fl_path)
    }

    #[inline]
    pub fn per_shot_target_quality_routine(
        &self,
        chunk: &mut Chunk,
        worker_id: Option<usize>,
    ) -> anyhow::Result<()> {
        chunk.tq_cq = Some(self.per_shot_target_quality(chunk, worker_id)?);
        Ok(())
    }
}

fn predict_quantizer(
    lower_quantizer_limit: u32,
    upper_quantizer_limit: u32,
    quantizer_score_history: &[(u32, f64)],
    target: f64,
) -> u32 {
    if quantizer_score_history.len() < 2 {
        // Fewer than 2 probes, return the midpoint between the upper and lower
        // quantizer bounds
        return (lower_quantizer_limit + upper_quantizer_limit) / 2;
    }

    // Sort history by quantizer
    let mut sorted_quantizer_score_history = quantizer_score_history.to_vec();
    sorted_quantizer_score_history.sort_by_key(|(quantizer, _)| *quantizer);
    let mut quantizer_score_map: Vec<(u32, f64)> = sorted_quantizer_score_history
        .iter()
        .map(|(quantizer, score)| (*quantizer, *score))
        .collect();
    quantizer_score_map.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
    // Create interpolation keys from score-quantizer pairs
    let (scores, quantizers): (Vec<f64>, Vec<f64>) = quantizer_score_map
        .iter()
        .map(|(quantizer, score)| (*score, *quantizer as f64))
        .unzip();
    let keys = scores
        .iter()
        .zip(quantizers.iter())
        .map(|(score, quantizer)| {
            Key::new(
                *score,
                *quantizer,
                match sorted_quantizer_score_history.len() {
                    0..=1 => unreachable!(),        // Handled in earlier guard
                    2 => Interpolation::Linear,     // 2 probes, use Linear without fitting curve
                    _ => Interpolation::CatmullRom, // 3 or more probes, fit CatmullRom curve
                },
            )
        })
        .collect();

    let spline = Spline::from_vec(keys);
    if let Some(predicted_quantizer) = spline.sample(target) {
        // Ensure predicted quantizer is an integer and within bounds
        (predicted_quantizer.round() as u32).clamp(lower_quantizer_limit, upper_quantizer_limit)
    } else {
        // We expect this to be unreachable but just in case
        // Failed to predict quantizer from Spline interpolation
        error!(
            "Failed to predict quantizer from Spline interpolation with target: {target}
            quantizer_score_history: {quantizer_score_history:?}",
        );
        (lower_quantizer_limit + upper_quantizer_limit) / 2
    }
}

fn within_tolerance(score: f64, target: f64) -> bool {
    (score - target).abs() / target < SCORE_TOLERANCE
}

pub fn vmaf_auto_threads(workers: usize) -> usize {
    const OVER_PROVISION_FACTOR: f64 = 1.25;

    let threads = available_parallelism()
        .expect("Unrecoverable: Failed to get thread count")
        .get();

    cmp::max(
        ((threads / workers) as f64 * OVER_PROVISION_FACTOR) as usize,
        1,
    )
}

#[derive(Copy, Clone)]
pub enum SkipProbingReason {
    QuantizerTooHigh,
    QuantizerTooLow,
    WithinTolerance,
    ProbeLimitReached,
    None,
}

#[allow(clippy::too_many_arguments)]
pub fn log_probes(
    quantizer_score_history: &mut [(u32, f64)],
    target: f64,
    frames: u32,
    probing_rate: u32,
    probing_speed: Option<u8>,
    chunk_name: &str,
    target_quantizer: u32,
    target_score: f64,
    skip: SkipProbingReason,
) {
    // Sort history by quantizer
    quantizer_score_history.sort_by_key(|(quantizer, _)| *quantizer);

    debug!(
        "chunk {name}: Target={target}, P-Rate={rate}, P-Speed={speed:?}, {frame_count} frames
        TQ-Probes: {history:.2?}{suffix}
        Final Q={target_quantizer:.0}, Final Score={target_score:.2}",
        name = chunk_name,
        target = target,
        rate = probing_rate,
        speed = ProbingSpeed::from_repr(probing_speed.unwrap_or(4) as usize),
        frame_count = frames,
        history = quantizer_score_history,
        suffix = match skip {
            SkipProbingReason::None => "",
            SkipProbingReason::QuantizerTooHigh => "Early Skip High Quantizer",
            SkipProbingReason::QuantizerTooLow => " Early Skip Low Quantizer",
            SkipProbingReason::WithinTolerance => " Early Skip Within Tolerance",
            SkipProbingReason::ProbeLimitReached => " Early Skip Probe Limit Reached",
        },
        target_quantizer = target_quantizer,
        target_score = target_score
    );
}

#[inline]
pub const fn adapt_probing_rate(rate: usize) -> usize {
    match rate {
        1..=4 => rate,
        _ => 1,
    }
}
