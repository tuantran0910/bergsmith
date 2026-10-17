//! Reproducible process-isolated executor measurements (BS-010.T5).
//!
//! Fixture generation and independent oracle validation run in this driver.
//! `/usr/bin/time` measures only a release worker executing a pinned plan.

use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use bergsmith_common::ids::{JobId, PlanId};
use bergsmith_fixtures::{RowMultiset, read_data_files, spike};
use bergsmith_maintenance::{
    PlanDocument, PlanIds, PlanInput, PlanOutcome, PlannedFile, PlannedTask, Planner, PlannerConfig,
};
use bergsmith_protocol::artifact::TaskReceipt;
use bergsmith_protocol::hashing::receipt_digest;
use clap::Parser;
use iceberg::spec::{DataContentType, DataFileBuilder, DataFileFormat};
use prost::Message;
use serde::Serialize;
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    /// A built release bergsmith-worker binary.
    #[arg(long)]
    worker: PathBuf,
    /// A new directory for fixtures, receipts and report.json.
    #[arg(long)]
    directory: PathBuf,
    /// Repeat each fixture in fresh attempts.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..))]
    iterations: u32,
    /// Run just one named spike fixture.
    #[arg(long)]
    fixture: Option<String>,
}

#[derive(Serialize)]
struct Sample {
    fixture: String,
    iteration: u32,
    elapsed_millis: u64,
    peak_rss_bytes: u64,
    input_file_bytes: u64,
    output_file_bytes: u64,
    rows_read: u64,
    rows_written: u64,
    planner_estimate_millis: u64,
    duration_budget_millis: u64,
    oracle_verified: bool,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    match run(&Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "executor benchmark failed");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), Box<dyn Error>> {
    let worker = args.worker.canonicalize()?;
    std::fs::create_dir(&args.directory)?;
    let root = args.directory.canonicalize()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let fixtures = spike::all()?;
    if args
        .fixture
        .as_ref()
        .is_some_and(|name| !fixtures.iter().any(|f| f.name == name))
    {
        return Err("unknown spike fixture".into());
    }
    let mut samples = Vec::new();
    for spike in fixtures {
        if args.fixture.as_ref().is_some_and(|name| spike.name != name) {
            continue;
        }
        let directory = root.join(spike.name);
        std::fs::create_dir(&directory)?;
        let fixture = runtime.block_on(spike.table.write_to(&directory))?;
        let config = PlannerConfig {
            min_input_files: 1,
            min_files_removed: 0,
            ..PlannerConfig::default()
        };
        let outcome = runtime.block_on(Planner::new(config)?.plan(
            PlanInput {
                metadata: fixture.metadata(),
                metadata_location: Some(fixture.metadata_location()),
                file_io: fixture.file_io(),
                selection: &[],
            },
            PlanIds {
                job_id: JobId::from_uuid(Uuid::now_v7()),
                plan_id: PlanId::from_uuid(Uuid::now_v7()),
            },
        ))?;
        let PlanOutcome::Planned(plan) = outcome else {
            return Err("fixture did not produce a plan".into());
        };
        let plan_file = directory.join("plan.json");
        create(&plan_file, &plan.to_json()?)?;
        let hash = plan.sha256()?.to_hex();
        create(&directory.join("plan.sha256"), hash.as_bytes())?;
        let expected = runtime.block_on(fixture.live_rows())?;
        for iteration in 0..args.iterations {
            let receipts = directory.join(format!("run-{iteration}"));
            let output = run_worker(&worker, &plan_file, &hash, &receipts)?;
            create(&receipts.join("time.txt"), &output.stderr)?;
            let summary: serde_json::Value =
                serde_json::from_slice(&std::fs::read(receipts.join("summary.json"))?)?;
            let elapsed = summary["elapsed_millis"]
                .as_u64()
                .ok_or("missing duration")?;
            let (rows_read, rows_written, bytes_written) =
                verify(&runtime, &fixture, &plan, &receipts, &summary, &expected)?;
            samples.push(Sample {
                fixture: spike.name.to_owned(),
                iteration,
                elapsed_millis: elapsed,
                peak_rss_bytes: peak_rss(&String::from_utf8(output.stderr)?)
                    .ok_or("missing peak RSS")?,
                input_file_bytes: plan
                    .tasks()
                    .iter()
                    .flat_map(PlannedTask::sources)
                    .map(PlannedFile::file_size_bytes)
                    .sum(),
                output_file_bytes: bytes_written,
                rows_read,
                rows_written,
                planner_estimate_millis: plan.estimate().duration_millis,
                duration_budget_millis: plan.config().duration_budget_millis,
                oracle_verified: true,
            });
            tracing::info!(
                fixture = spike.name,
                iteration,
                elapsed_millis = elapsed,
                "benchmark sample verified"
            );
        }
    }
    create(
        &root.join("report.json"),
        &serde_json::to_vec_pretty(&samples)?,
    )?;
    Ok(())
}

fn run_worker(
    worker: &Path,
    plan: &Path,
    hash: &str,
    receipts: &Path,
) -> Result<std::process::Output, Box<dyn Error>> {
    let output = Command::new("/usr/bin/time")
        .arg(if cfg!(target_os = "macos") {
            "-l"
        } else {
            "-v"
        })
        .arg(worker)
        .arg("local")
        .arg("--plan")
        .arg(plan)
        .arg("--plan-sha256")
        .arg(hash)
        .arg("--receipts")
        .arg(receipts)
        .output()?;
    if !output.status.success() {
        tracing::error!(stderr = %String::from_utf8_lossy(&output.stderr), stdout = %String::from_utf8_lossy(&output.stdout), "benchmark subprocess failed");
        return Err("worker subprocess failed".into());
    }
    Ok(output)
}

fn verify(
    runtime: &tokio::runtime::Runtime,
    fixture: &bergsmith_fixtures::Fixture,
    plan: &PlanDocument,
    receipts: &Path,
    summary: &serde_json::Value,
    expected: &RowMultiset,
) -> Result<(u64, u64, u64), Box<dyn Error>> {
    let mut actual = RowMultiset::new();
    let (mut rows_read, mut rows_written, mut bytes_written) = (0, 0, 0);
    for task in plan.tasks() {
        let receipt = TaskReceipt::decode(
            std::fs::read(receipts.join(format!("{}.pb", task.ordinal())))?.as_slice(),
        )?;
        let hash = receipt_digest(&receipt).to_hex();
        let recorded = summary["receipts"]
            .as_array()
            .ok_or("missing receipts")?
            .iter()
            .find(|entry| entry["task_id"].as_str() == Some(receipt.task_id.as_str()))
            .ok_or("receipt not in summary")?;
        if recorded["receipt_sha256"].as_str() != Some(hash.as_str()) {
            return Err("receipt digest mismatch".into());
        }
        let mut files = Vec::new();
        for file in &receipt.outputs {
            files.push(
                DataFileBuilder::default()
                    .content(DataContentType::Data)
                    .file_path(file.path.clone())
                    .file_format(DataFileFormat::Parquet)
                    .file_size_in_bytes(u64::try_from(file.file_size_bytes)?)
                    .record_count(u64::try_from(file.record_count)?)
                    .build()?,
            );
        }
        let rows = runtime.block_on(read_data_files(
            fixture.metadata(),
            fixture.file_io(),
            &files,
        ))?;
        for (row, count) in rows.excess_over(&RowMultiset::new()) {
            for _ in 0..count {
                actual.insert(row.clone());
            }
        }
        let metrics = receipt.metrics.ok_or("missing metrics")?;
        rows_read += u64::try_from(metrics.rows_read)?;
        rows_written += u64::try_from(metrics.rows_written)?;
        bytes_written += u64::try_from(metrics.bytes_written)?;
    }
    if &actual != expected {
        return Err("replacement rows differ from independent oracle".into());
    }
    Ok((rows_read, rows_written, bytes_written))
}

fn peak_rss(stderr: &str) -> Option<u64> {
    for line in stderr.lines() {
        if line.contains("maximum resident set size") {
            return line.split_whitespace().next()?.parse().ok();
        }
        if line.contains("Maximum resident set size (kbytes)") {
            return line
                .rsplit(':')
                .next()?
                .trim()
                .parse::<u64>()
                .ok()?
                .checked_mul(1024);
        }
    }
    None
}

fn create(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(bytes)
}
