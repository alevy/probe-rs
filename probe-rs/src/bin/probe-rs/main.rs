mod cmd;
mod util;

include!(concat!(env!("OUT_DIR"), "/meta.rs"));

use std::cmp::Reverse;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::{ffi::OsString, fs::File, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use itertools::Itertools;
use probe_rs::flashing::{BinOptions, Format, IdfOptions};
use probe_rs::{Lister, Target};
use serde::Serialize;
use serde::{de::Error, Deserialize, Deserializer};
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};
use tracing::metadata::LevelFilter;
use tracing_subscriber::{
    fmt::format::FmtSpan, prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt,
    EnvFilter, Layer,
};

use crate::util::parse_u32;
use crate::util::parse_u64;

const MAX_LOG_FILES: usize = 20;

#[derive(clap::Parser)]
#[clap(
    name = "probe-rs",
    about = "The probe-rs CLI",
    version = meta::CARGO_VERSION,
    long_version = meta::LONG_VERSION
)]
struct Cli {
    /// Location for log file
    ///
    /// If no location is specified, the behaviour depends on `--log-to-folder`.
    #[clap(long, global = true)]
    log_file: Option<PathBuf>,
    /// Enable logging to the default folder. This option is ignored if `--log-file` is specified.
    #[clap(long, global = true)]
    log_to_folder: bool,
    #[clap(subcommand)]
    subcommand: Subcommand,
}

#[derive(clap::Subcommand)]
enum Subcommand {
    /// Debug Adapter Protocol (DAP) server. See https://probe.rs/docs/tools/debugger/
    DapServer(cmd::dap_server::Cmd),
    /// List all connected debug probes
    List(cmd::list::Cmd),
    /// Gets info about the selected debug probe and connected target
    Info(cmd::info::Cmd),
    /// Resets the target attached to the selected debug probe
    Reset(cmd::reset::Cmd),
    /// Run a GDB server
    Gdb(cmd::gdb::Cmd),
    /// Basic command line debugger
    Debug(cmd::debug::Cmd),
    /// Download memory to attached target
    Download(cmd::download::Cmd),
    /// Erase all nonvolatile memory of attached target
    Erase(cmd::erase::Cmd),
    /// Flash and run an ELF program
    #[clap(name = "run")]
    Run(cmd::run::Cmd),
    /// Attach to rtt logging
    #[clap(name = "attach")]
    Attach(cmd::attach::Cmd),
    /// Trace a memory location on the target
    #[clap(name = "trace")]
    Trace(cmd::trace::Cmd),
    /// Configure and monitor ITM trace packets from the target.
    #[clap(name = "itm")]
    Itm(cmd::itm::Cmd),
    Chip(cmd::chip::Cmd),
    /// Measure the throughput of the selected debug probe
    Benchmark(cmd::benchmark::Cmd),
    /// Profile on-target runtime performance of target ELF program
    Profile(cmd::profile::ProfileCmd),
    Read(cmd::read::Cmd),
    Write(cmd::write::Cmd),
    /// Executes a test binary that uses embedded-test
    #[clap(name = "test")]
    Test(cmd::test::Cmd),
}

/// Shared options for core selection, shared between commands
#[derive(clap::Parser)]
pub(crate) struct CoreOptions {
    #[clap(long, default_value = "0")]
    core: usize,
}

/// A helper function to deserialize a default [`Format`] from a string.
fn format_from_str<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Format>, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::String(s) => match Format::from_str(s.as_str()) {
            Ok(format) => Ok(Some(format)),
            Err(e) => Err(D::Error::custom(e)),
        },
        _ => Ok(None),
    }
}

#[derive(clap::Parser, Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct FormatOptions {
    /// If a format is provided, use it.
    /// If a target has a preferred format, we use that.
    /// Finally, if neither of the above cases are true, we default to ELF.
    #[clap(value_enum, ignore_case = true, long)]
    #[serde(deserialize_with = "format_from_str")]
    format: Option<Format>,
    /// The address in memory where the binary will be put at. This is only considered when `bin` is selected as the format.
    #[clap(long, value_parser = parse_u64)]
    pub base_address: Option<u64>,
    /// The number of bytes to skip at the start of the binary file. This is only considered when `bin` is selected as the format.
    #[clap(long, value_parser = parse_u32, default_value = "0")]
    pub skip: u32,
    /// The idf bootloader path
    #[clap(long)]
    pub idf_bootloader: Option<PathBuf>,
    /// The idf partition table path
    #[clap(long)]
    pub idf_partition_table: Option<PathBuf>,
}

impl FormatOptions {
    /// If a format is provided, use it.
    /// If a target has a preferred format, we use that.
    /// Finally, if neither of the above cases are true, we default to [`Format::default()`].
    pub fn into_format(self, target: &Target) -> anyhow::Result<Format> {
        let format = self.format.unwrap_or_else(|| match target.default_format {
            probe_rs_target::BinaryFormat::Idf => Format::Idf(Default::default()),
            probe_rs_target::BinaryFormat::Raw => Default::default(),
        });
        Ok(match format {
            Format::Bin(_) => Format::Bin(BinOptions {
                base_address: self.base_address,
                skip: self.skip,
            }),
            Format::Hex => Format::Hex,
            Format::Elf => Format::Elf,
            Format::Idf(_) => {
                let bootloader = if let Some(path) = self.idf_bootloader {
                    Some(std::fs::read(path)?)
                } else {
                    None
                };

                let partition_table = if let Some(path) = self.idf_partition_table {
                    Some(esp_idf_part::PartitionTable::try_from(std::fs::read(
                        path,
                    )?)?)
                } else {
                    None
                };

                Format::Idf(IdfOptions {
                    bootloader,
                    partition_table,
                })
            }
            Format::Uf2 => Format::Uf2,
        })
    }
}

/// Determine the default location for the logfile
///
/// This has to be called as early as possible, and while the program
/// is single-threaded. Otherwise, determining the local time might fail.
fn default_logfile_location() -> Result<PathBuf> {
    let project_dirs = directories::ProjectDirs::from("rs", "probe-rs", "probe-rs")
        .context("the application storage directory could not be determined")?;
    let directory = project_dirs.data_dir();
    let logname = sanitize_filename::sanitize_with_options(
        format!(
            "{}.log",
            OffsetDateTime::now_local()?.unix_timestamp_nanos() / 1_000_000
        ),
        sanitize_filename::Options {
            replacement: "_",
            ..Default::default()
        },
    );
    std::fs::create_dir_all(directory).context(format!("{directory:?} could not be created"))?;

    let log_path = directory.join(logname);

    Ok(log_path)
}

/// Prune all old log files in the `directory`.
fn prune_logs(directory: &Path) -> Result<(), anyhow::Error> {
    // Get the path and elapsed creation time of all files in the log directory that have the '.log'
    // suffix.
    let mut log_files = fs::read_dir(directory)?
        .filter_map(|entry| {
            if let Ok(entry) = entry {
                let path = entry.path();
                if path.extension() == Some(OsStr::new("log")) {
                    let metadata = fs::metadata(&path).ok()?;
                    let last_modified = metadata.created().ok()?;
                    Some((path, last_modified))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect_vec();

    // Order all files by the elapsed creation time with smallest first.
    log_files.sort_unstable_by_key(|(_, b)| Reverse(*b));

    // Iterate all files except for the first `MAX_LOG_FILES` and delete them.
    for (path, _) in log_files.iter().skip(MAX_LOG_FILES) {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Returns the cleaned arguments for the handler of the respective end binary (cli, cargo-flash, cargo-embed, etc.).
fn multicall_check(args: &[OsString], want: &str) -> Option<Vec<OsString>> {
    let argv0 = Path::new(&args[0]);
    if let Some(command) = argv0.file_stem().and_then(|f| f.to_str()) {
        if command == want {
            return Some(args.to_vec());
        }
    }

    if let Some(command) = args.get(1).and_then(|f| f.to_str()) {
        if command == want {
            return Some(args[1..].to_vec());
        }
    }

    None
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if let Some(args) = multicall_check(&args, "cargo-flash") {
        cmd::cargo_flash::main(args);
        return Ok(());
    }
    if let Some(args) = multicall_check(&args, "cargo-embed") {
        cmd::cargo_embed::main(args);
        return Ok(());
    }

    let utc_offset = UtcOffset::current_local_offset()
        .context("Failed to determine local time for timestamps")?;

    // Parse the commandline options.
    let matches = Cli::parse_from(args);

    // Setup the probe lister, list all probes normally
    let lister = Lister::new();

    // the DAP server has special logging requirements. Run it before initializing logging,
    // so it can do its own special init.
    if let Subcommand::DapServer(cmd) = matches.subcommand {
        return cmd::dap_server::run(cmd, &lister, utc_offset);
    }

    let log_path = if let Some(location) = matches.log_file {
        Some(location)
    } else if matches.log_to_folder {
        let location =
            default_logfile_location().context("Unable to determine default log file location.")?;
        prune_logs(
            location
                .parent()
                .expect("A file parent directory. Please report this as a bug."),
        )?;
        Some(location)
    } else {
        None
    };

    let stdout_subscriber = tracing_subscriber::fmt::layer()
        .compact()
        .with_writer(std::io::stderr)
        .without_time()
        .with_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::ERROR.into())
                .from_env_lossy(),
        );

    let _append_guard = if let Some(ref log_path) = log_path {
        let log_file = File::create(log_path)?;

        let (file_appender, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
            .lossy(false)
            .buffered_lines_limit(128 * 1024)
            .finish(log_file);

        let file_subscriber = tracing_subscriber::fmt::layer()
            .json()
            .with_file(true)
            .with_line_number(true)
            .with_span_events(FmtSpan::FULL)
            .with_writer(file_appender);

        tracing_subscriber::registry()
            .with(stdout_subscriber)
            .with(file_subscriber)
            .init();

        Some(guard)
    } else {
        tracing_subscriber::registry()
            .with(stdout_subscriber)
            .init();

        None
    };

    if let Some(ref log_path) = log_path {
        tracing::info!("Writing log to {:?}", log_path);
    }

    let result = match matches.subcommand {
        Subcommand::DapServer { .. } => unreachable!(), // handled above.
        Subcommand::List(cmd) => cmd.run(&lister),
        Subcommand::Info(cmd) => cmd.run(&lister),
        Subcommand::Gdb(cmd) => cmd.run(&lister),
        Subcommand::Reset(cmd) => cmd.run(&lister),
        Subcommand::Debug(cmd) => cmd.run(&lister),
        Subcommand::Download(cmd) => cmd.run(&lister),
        Subcommand::Run(cmd) => cmd.run(&lister, true, utc_offset),
        Subcommand::Attach(cmd) => cmd.run(&lister, utc_offset),
        Subcommand::Erase(cmd) => cmd.run(&lister),
        Subcommand::Trace(cmd) => cmd.run(&lister),
        Subcommand::Itm(cmd) => cmd.run(&lister),
        Subcommand::Chip(cmd) => cmd.run(),
        Subcommand::Benchmark(cmd) => cmd.run(&lister),
        Subcommand::Profile(cmd) => cmd.run(&lister),
        Subcommand::Read(cmd) => cmd.run(&lister),
        Subcommand::Write(cmd) => cmd.run(&lister),
        Subcommand::Test(cmd) => cmd.run(&lister, true, utc_offset),
    };

    if let Some(ref log_path) = log_path {
        tracing::info!("Wrote log to {:?}", log_path);
    }

    result
}
