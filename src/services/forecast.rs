//! Robust linear trend, and what it is honest to conclude from one.
//!
//! Written for "when does this disk fill", where ordinary least squares is the
//! wrong tool: a single log rotation, or a backup that writes 50 GB and deletes
//! it an hour later, drags an OLS line far enough to move a predicted date by
//! weeks. Theil-Sen takes the *median* of the pairwise slopes instead, so it
//! shrugs off up to roughly 29% of the samples being outliers, which is about
//! what a busy filesystem throws at it.
//!
//! The estimator is the easy half. The half that decides whether the feature is
//! worth having is [`Verdict::Unclear`]: a date nobody can rely on is worse than
//! no date, because an operator only has to be burned once before they stop
//! reading the number at all. So a trend has to clear the scatter it is drawn
//! through before this module will name a day.

/// A fitted line plus what is needed to judge it.
#[derive(Debug, Clone)]
pub struct Trend {
    /// Median of the pairwise slopes, in units per second.
    pub slope_per_sec: f64,
    /// Lower and upper quartile of the pairwise slopes. Not a confidence
    /// interval - an honest spread, and named as one at every call site.
    pub slope_q1: f64,
    pub slope_q3: f64,
    /// Robust scale of the residuals (`1.4826 * MAD`), comparable to a standard
    /// deviation but unmoved by the outliers this whole module exists for.
    pub residual_sigma: f64,
    pub points: usize,
}

/// A trend must move the series by this many robust sigmas across the window
/// before it counts as a trend rather than as the shape of the noise. Three is
/// the usual "clearly not chance" line, and on a disk that genuinely fills the
/// signal clears it by orders of magnitude.
const SIGNAL_TO_NOISE: f64 = 3.0;

/// Fewer than this and the median of the pairwise slopes is not meaningfully a
/// median. Four points give six slopes, the smallest sample worth the name.
const MIN_POINTS: usize = 4;

/// Pairwise slopes are O(n^2). At the cap that is ~125k comparisons per series,
/// which is cheap; anything longer is thinned first, and that costs nothing real
/// because a trend does not sharpen with more points once the window is covered
/// evenly.
const MAX_POINTS: usize = 500;

fn median_sorted(v: &[f64]) -> f64 {
    let n = v.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn quantile_sorted(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let idx = ((v.len() - 1) as f64 * q).round() as usize;
    v[idx.min(v.len() - 1)]
}

/// Evenly thin a series to at most `MAX_POINTS`, keeping both ends.
fn subsample(points: &[(f64, f64)]) -> Vec<(f64, f64)> {
    if points.len() <= MAX_POINTS {
        return points.to_vec();
    }
    let stride = points.len() as f64 / MAX_POINTS as f64;
    let mut out: Vec<(f64, f64)> = (0..MAX_POINTS)
        .map(|i| points[((i as f64 * stride) as usize).min(points.len() - 1)])
        .collect();
    // Striding almost never lands on the newest sample, and for a forecast the
    // newest sample is the one the extrapolation starts from.
    if let (Some(last), Some(end)) = (out.last().copied(), points.last().copied())
        && last.0 != end.0
    {
        out.push(end);
    }
    out
}

/// Theil-Sen fit over `(x, y)`. `None` when there is not enough to fit, or when
/// every sample shares one timestamp.
pub fn theil_sen(points: &[(f64, f64)]) -> Option<Trend> {
    if points.len() < MIN_POINTS {
        return None;
    }
    let pts = subsample(points);

    let mut slopes: Vec<f64> = Vec::with_capacity(pts.len() * pts.len() / 2);
    for i in 0..pts.len() {
        for j in (i + 1)..pts.len() {
            let dx = pts[j].0 - pts[i].0;
            if dx != 0.0 {
                slopes.push((pts[j].1 - pts[i].1) / dx);
            }
        }
    }
    if slopes.is_empty() {
        return None;
    }
    slopes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let slope = median_sorted(&slopes);
    let slope_q1 = quantile_sorted(&slopes, 0.25);
    let slope_q3 = quantile_sorted(&slopes, 0.75);

    let mut intercepts: Vec<f64> = pts.iter().map(|(x, y)| y - slope * x).collect();
    intercepts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Kept local: the line through the bulk, used only to measure scatter.
    let intercept = median_sorted(&intercepts);

    let mut resid: Vec<f64> = pts
        .iter()
        .map(|(x, y)| (y - (slope * x + intercept)).abs())
        .collect();
    resid.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // 1.4826 rescales a MAD to the sigma of a normal distribution.
    let residual_sigma = 1.4826 * median_sorted(&resid);

    Some(Trend {
        slope_per_sec: slope,
        slope_q1,
        slope_q3,
        residual_sigma,
        points: pts.len(),
    })
}

/// What the trend supports saying out loud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The drift across the window is lost in the scatter. No date.
    Unclear,
    /// A real trend, but not toward full inside the horizon asked about.
    Stable,
    /// Filling, and it reaches full within the horizon.
    Filling,
    /// Freeing space.
    Draining,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Unclear => "unclear",
            Verdict::Stable => "stable",
            Verdict::Filling => "filling",
            Verdict::Draining => "draining",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Forecast {
    pub verdict: Verdict,
    pub bytes_per_day: f64,
    /// Days until `used` reaches `total` at the median slope. Only set when the
    /// verdict is `Filling`.
    pub days_until_full: Option<f64>,
    /// The same date from the quartile slopes - the steeper slope fills sooner,
    /// so it gives the near end. The far end is `None` when the slower quartile
    /// is not filling at all, i.e. it is genuinely open.
    pub days_until_full_low: Option<f64>,
    pub days_until_full_high: Option<f64>,
}

/// Turn a fit into a verdict against a capacity and a horizon.
///
/// `window_secs` is the span the fit was taken over: the test for "is this a
/// trend" is whether the line moves the series further across that span than
/// the residuals scatter it, which is a question the slope alone cannot answer.
pub fn forecast_full(
    trend: &Trend,
    used_bytes: f64,
    total_bytes: f64,
    window_secs: f64,
    horizon_days: f64,
) -> Forecast {
    let bytes_per_day = trend.slope_per_sec * 86_400.0;
    let flat = Forecast {
        verdict: Verdict::Unclear,
        bytes_per_day,
        days_until_full: None,
        days_until_full_low: None,
        days_until_full_high: None,
    };

    let signal = (trend.slope_per_sec * window_secs).abs();
    if signal < SIGNAL_TO_NOISE * trend.residual_sigma {
        return flat;
    }
    if trend.slope_per_sec < 0.0 {
        return Forecast {
            verdict: Verdict::Draining,
            ..flat
        };
    }

    let free = total_bytes - used_bytes;
    if free <= 0.0 || total_bytes <= 0.0 {
        // Already full: a date would be in the past, which reads as nonsense.
        return Forecast {
            verdict: Verdict::Stable,
            ..flat
        };
    }

    let days = |slope_per_sec: f64| -> Option<f64> {
        if slope_per_sec <= 0.0 {
            return None;
        }
        Some(free / (slope_per_sec * 86_400.0))
    };

    match days(trend.slope_per_sec) {
        Some(d) if d <= horizon_days => Forecast {
            verdict: Verdict::Filling,
            bytes_per_day,
            days_until_full: Some(d),
            days_until_full_low: days(trend.slope_q3),
            days_until_full_high: days(trend.slope_q1),
        },
        // Filling, but so slowly that naming a day months out would imply a
        // precision this estimator does not have.
        _ => Forecast {
            verdict: Verdict::Stable,
            ..flat
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hourly samples of a disk filling at a steady rate, with `noise` of jitter.
    fn series(hours: usize, start: f64, per_hour: f64, noise: &[f64]) -> Vec<(f64, f64)> {
        (0..hours)
            .map(|i| {
                let x = (i * 3600) as f64;
                let n = if noise.is_empty() {
                    0.0
                } else {
                    noise[i % noise.len()]
                };
                (x, start + per_hour * i as f64 + n)
            })
            .collect()
    }

    const WINDOW: f64 = 336.0 * 3600.0;

    #[test]
    fn a_clean_ramp_recovers_its_slope() {
        let t = theil_sen(&series(336, 100.0e9, 1.0e9, &[])).expect("fit");
        // 1 GB/h is 24 GB/day.
        assert!(
            (t.slope_per_sec * 86_400.0 - 24.0e9).abs() < 1.0e6,
            "got {} per day",
            t.slope_per_sec * 86_400.0
        );
    }

    /// The reason this is Theil-Sen and not least squares: one enormous spike
    /// that goes away again must not bend the line.
    #[test]
    fn a_transient_spike_does_not_move_the_slope() {
        let mut pts = series(336, 100.0e9, 1.0e9, &[]);
        // A backup writes 400 GB across six hours, then deletes it.
        for p in pts.iter_mut().skip(100).take(6) {
            p.1 += 400.0e9;
        }
        let t = theil_sen(&pts).expect("fit");
        assert!(
            (t.slope_per_sec * 86_400.0 - 24.0e9).abs() < 1.0e9,
            "spike moved the slope to {} per day",
            t.slope_per_sec * 86_400.0
        );
    }

    #[test]
    fn a_steady_fill_names_a_day() {
        let t = theil_sen(&series(336, 900.0e9, 1.0e9, &[])).expect("fit");
        // 24 GB/day, with 240 GB of headroom left, is ten days.
        let f = forecast_full(&t, 760.0e9, 1000.0e9, WINDOW, 60.0);
        assert_eq!(f.verdict, Verdict::Filling);
        let d = f.days_until_full.expect("a day");
        assert!((9.0..11.0).contains(&d), "got {d} days");
    }

    /// A disk that thrashes up and down without going anywhere must produce no
    /// date at all - this is the case the whole gate exists for.
    #[test]
    fn noise_without_a_trend_stays_unclear() {
        let t = theil_sen(&series(
            336,
            500.0e9,
            0.0,
            &[0.0, 40.0e9, -30.0e9, 20.0e9, -35.0e9],
        ))
        .expect("fit");
        let f = forecast_full(&t, 500.0e9, 1000.0e9, WINDOW, 60.0);
        assert_eq!(f.verdict, Verdict::Unclear, "{f:?}");
        assert!(f.days_until_full.is_none());
    }

    /// A real but tiny trend buried in large jitter is still not something to
    /// put a date on.
    #[test]
    fn a_trend_smaller_than_its_scatter_stays_unclear() {
        let t = theil_sen(&series(
            336,
            500.0e9,
            0.0005e9,
            &[0.0, 30.0e9, -25.0e9, 18.0e9, -22.0e9],
        ))
        .expect("fit");
        let f = forecast_full(&t, 500.0e9, 1000.0e9, WINDOW, 60.0);
        assert_eq!(f.verdict, Verdict::Unclear, "{f:?}");
    }

    #[test]
    fn freeing_space_reads_as_draining_and_names_no_day() {
        let t = theil_sen(&series(336, 800.0e9, -1.0e9, &[])).expect("fit");
        let f = forecast_full(&t, 500.0e9, 1000.0e9, WINDOW, 60.0);
        assert_eq!(f.verdict, Verdict::Draining);
        assert!(f.days_until_full.is_none());
        assert!(f.bytes_per_day < 0.0);
    }

    /// Filling, but a century out. Naming a day there would imply a precision
    /// two weeks of samples cannot support.
    #[test]
    fn a_fill_beyond_the_horizon_reads_as_stable() {
        let t = theil_sen(&series(336, 100.0e9, 0.001e9, &[])).expect("fit");
        let f = forecast_full(&t, 100.0e9, 1000.0e9, WINDOW, 60.0);
        assert_eq!(f.verdict, Verdict::Stable, "{f:?}");
        assert!(f.days_until_full.is_none());
    }

    #[test]
    fn the_quartile_range_brackets_the_median_estimate() {
        let t = theil_sen(&series(336, 500.0e9, 1.0e9, &[0.0, 2.0e9, -1.5e9, 1.0e9])).expect("fit");
        let f = forecast_full(&t, 900.0e9, 1000.0e9, WINDOW, 60.0);
        assert_eq!(f.verdict, Verdict::Filling);
        let (lo, mid, hi) = (
            f.days_until_full_low.expect("low"),
            f.days_until_full.expect("mid"),
            f.days_until_full_high.expect("high"),
        );
        assert!(
            lo <= mid && mid <= hi,
            "range {lo}..{mid}..{hi} is not ordered"
        );
    }

    #[test]
    fn too_few_points_do_not_fit() {
        assert!(theil_sen(&[(0.0, 1.0), (1.0, 2.0)]).is_none());
    }

    #[test]
    fn one_timestamp_repeated_does_not_fit() {
        let pts = vec![(5.0, 1.0), (5.0, 2.0), (5.0, 3.0), (5.0, 4.0)];
        assert!(theil_sen(&pts).is_none());
    }

    /// The subsample must keep the newest point: the extrapolation starts there.
    #[test]
    fn subsampling_keeps_both_ends() {
        let t = theil_sen(&series(5000, 0.0, 1.0e6, &[])).expect("fit");
        assert!(t.points <= MAX_POINTS + 1);
        assert!(
            (t.slope_per_sec * 86_400.0 - 24.0e6).abs() < 1.0e4,
            "subsampling changed the slope: {}",
            t.slope_per_sec * 86_400.0
        );
    }
}
