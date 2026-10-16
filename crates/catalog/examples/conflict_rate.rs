//! Measures how often conservative commits conflict with a steady writer
//! (BS-007.T6), against the Compose Lakekeeper (`make services-up`).
//!
//! A synthetic sink appends a file every `--interval-secs`. Model jobs of
//! several durations, given as fractions of the interval, run back to back
//! in two modes:
//!
//! - `aligned`: start when Inventory observes a new `main` snapshot, as
//!   commit-aligned scheduling does;
//! - `random`: start at a random point in the interval.
//!
//! The report counts publications, conflicts the reload check caught, and
//! conflicts the catalog rejected, next to the `1 - d/T` success rate
//! expected for random starts.
//!
//! ```text
//! cargo run -p bergsmith-catalog --example conflict_rate -- \
//!     --interval-secs 300 --fractions 0.25,0.5,0.75 --jobs 20 --out report.json
//! ```

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a measurement tool that stops on any setup error"
)]

#[path = "../tests/support/mod.rs"]
mod support;

use std::collections::BTreeSet;
use std::io::Write as _;
use std::time::{Duration, Instant};

use serde_json::json;
use support::Warehouse;
use support::sink::{JobResult, MaintenanceJob, SyntheticSink, next_main};

struct Options {
    interval: Duration,
    fractions: Vec<f64>,
    jobs: u32,
    poll: Duration,
    out: Option<String>,
}

fn options() -> Options {
    let mut options = Options {
        interval: Duration::from_secs(300),
        fractions: vec![0.25, 0.5, 0.75],
        jobs: 20,
        poll: Duration::from_millis(500),
        out: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--interval-secs" => {
                options.interval = Duration::from_secs_f64(value.parse().expect("seconds"));
            }
            "--fractions" => {
                options.fractions = value
                    .split(',')
                    .map(|f| f.parse().expect("a fraction"))
                    .collect();
            }
            "--jobs" => options.jobs = value.parse().expect("a count"),
            "--poll-ms" => options.poll = Duration::from_millis(value.parse().expect("ms")),
            "--out" => options.out = Some(value),
            other => panic!("unknown flag {other}"),
        }
    }
    options
}

#[derive(Default)]
struct Tally {
    published: u32,
    replan_before_send: u32,
    rejected: u32,
}

impl Tally {
    fn record(&mut self, result: JobResult) {
        match result {
            JobResult::Published => self.published += 1,
            JobResult::ReplanBeforeSend => self.replan_before_send += 1,
            JobResult::Rejected => self.rejected += 1,
        }
    }

    fn total(&self) -> u32 {
        self.published + self.replan_before_send + self.rejected
    }
}

/// A random delay in `[0, interval)`.
fn random_delay(interval: Duration) -> Duration {
    let (_, random) = uuid::Uuid::now_v7().as_u64_pair();
    let millis = u64::try_from(interval.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    Duration::from_millis(random % millis)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = options();
    let warehouse = Warehouse::create().await;
    let name = warehouse.create_table("db", "events").await;
    let jobs = warehouse.connect().await;
    let sink = SyntheticSink::new(warehouse.connect().await, name.clone());
    sink.commit_once().await;
    let _running = sink.spawn(options.interval);
    let started = Instant::now();
    let mut rewritten = BTreeSet::new();
    let mut rows = Vec::new();
    let mut stdout = std::io::stdout();
    writeln!(
        stdout,
        "mode     d/T   jobs published replan-before-send rejected success expected-random"
    )?;
    for mode in ["aligned", "random"] {
        for &fraction in &options.fractions {
            let duration = options.interval.mul_f64(fraction);
            let mut tally = Tally::default();
            while tally.total() < options.jobs {
                if mode == "aligned" {
                    let last = jobs
                        .load_table(&name, false)
                        .await?
                        .metadata
                        .current_snapshot_id();
                    next_main(&jobs, &name, last, options.poll).await;
                } else {
                    tokio::time::sleep(random_delay(options.interval)).await;
                }
                if let Some(result) =
                    MaintenanceJob::run(&jobs, &sink, duration, &mut rewritten).await
                {
                    tally.record(result);
                }
            }
            let success = f64::from(tally.published) / f64::from(tally.total());
            writeln!(
                stdout,
                "{mode:<8} {fraction:<5} {:<4} {:<9} {:<18} {:<8} {success:<7.2} {:.2}",
                tally.total(),
                tally.published,
                tally.replan_before_send,
                tally.rejected,
                1.0 - fraction
            )?;
            rows.push(json!({
                "mode": mode,
                "duration_fraction": fraction,
                "jobs": tally.total(),
                "published": tally.published,
                "replan_before_send": tally.replan_before_send,
                "rejected": tally.rejected,
                "success_rate": success,
                "expected_random_success_rate": 1.0 - fraction,
            }));
        }
    }
    let report = json!({
        "interval_secs": options.interval.as_secs_f64(),
        "elapsed_secs": started.elapsed().as_secs_f64(),
        "sink_retries": sink.retries().await,
        "results": rows,
    });
    writeln!(stdout, "sink retries: {}", report["sink_retries"])?;
    if let Some(path) = options.out {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    }
    Ok(())
}
