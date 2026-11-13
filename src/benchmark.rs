use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurements {
    pub schema_version: u32,
    pub scenario: String,
    pub backend: String,
    pub minecraft_version: String,
    pub hardware: String,
    pub settings: String,
    pub runs: Vec<Run>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub frame_ms: Vec<f64>,
    pub tick_ms: Vec<f64>,
}

#[derive(Debug, Serialize)]
pub struct Distribution {
    pub samples: usize,
    pub median_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

#[derive(Serialize)]
pub struct RunSummary {
    pub frames: Distribution,
    pub ticks: Distribution,
}

#[derive(Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub scenario: String,
    pub backend: String,
    pub minecraft_version: String,
    pub hardware: String,
    pub settings: String,
    pub runs: Vec<RunSummary>,
    pub sufficient_repetitions: bool,
    pub measurement_source: &'static str,
    pub performance_claim: &'static str,
}

/// Nearest-rank percentiles; retain runs separately to avoid pooling away variance.
pub fn distribution(samples: &[f64]) -> Result<Distribution> {
    if samples.is_empty() || samples.iter().any(|v| !v.is_finite() || *v <= 0.0) {
        bail!("timing samples must be nonempty, finite, positive milliseconds");
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let percentile = |p: f64| sorted[((sorted.len() as f64 * p).ceil() as usize).saturating_sub(1)];
    Ok(Distribution {
        samples: sorted.len(),
        median_ms: percentile(0.5),
        p95_ms: percentile(0.95),
        p99_ms: percentile(0.99),
        max_ms: sorted[sorted.len() - 1],
    })
}

/// Validate provenance and summarize each run separately without comparing or claiming speedups.
pub fn summarize(input: Measurements) -> Result<Report> {
    if input.schema_version != 1 {
        bail!("unsupported measurements schema; expected 1");
    }
    if [
        &input.scenario,
        &input.backend,
        &input.minecraft_version,
        &input.hardware,
        &input.settings,
    ]
    .iter()
    .any(|s| s.trim().is_empty())
        || input.runs.is_empty()
    {
        bail!("measurement provenance and at least one run are required");
    }
    let sufficient_repetitions = input.runs.len() >= 5;
    let runs = input
        .runs
        .iter()
        .map(|r| {
            Ok(RunSummary {
                frames: distribution(&r.frame_ms)?,
                ticks: distribution(&r.tick_ms)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Report {
        schema_version: 1,
        scenario: input.scenario,
        backend: input.backend,
        minecraft_version: input.minecraft_version,
        hardware: input.hardware,
        settings: input.settings,
        runs,
        sufficient_repetitions,
        measurement_source: "user-supplied; not independently verified",
        performance_claim: "none; no baseline comparison or statistical significance test performed",
    })
}
