//! `cove-host`: serve, check and test the Cove apps in a directory.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use cove_host::toolchain;
use cove_host::{Backend, Host, HostModules, ServeOptions};

#[derive(Parser)]
#[command(
    name = "cove-host",
    version,
    about = "Hosts many small Cove web apps on one machine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load every app and serve them over HTTP.
    Serve {
        /// The directory holding one directory per app.
        #[arg(long, default_value = "apps")]
        apps: PathBuf,
        /// Where apps keep their state (`<data>/<app>/kv.sqlite3`).
        #[arg(long, default_value = "data")]
        data: PathBuf,
        /// Where to listen.
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: String,
        /// Worker threads that run Cove (default: one per hardware thread).
        #[arg(long)]
        workers: Option<usize>,
        /// Threads for HTTP and pending host work.
        #[arg(long, default_value_t = 2)]
        io_threads: usize,
        /// How long a run may hold a worker while others wait, in
        /// milliseconds; 0 never asks a run to yield.
        #[arg(long, default_value_t = 2.0)]
        slice: f64,
        /// Connections open at once; one more is answered 503.
        #[arg(long, default_value_t = 10_000)]
        max_connections: usize,
        /// Requests admitted at once over every app; one more is answered 503.
        #[arg(long, default_value_t = 10_000)]
        max_in_flight: usize,
        /// Which tier runs the apps: auto (native where available), vm, native.
        #[arg(long, default_value = "auto")]
        backend: Backend,
        /// Print no `log` lines.
        #[arg(long)]
        quiet: bool,
    },
    /// Check apps against the host's schemas and grants; non-zero if any
    /// would be refused.
    Check {
        #[arg(long, default_value = "apps")]
        apps: PathBuf,
        /// Only these apps.
        names: Vec<String>,
    },
    /// Run apps' `test fn`s with the host's modules, grants and limits.
    Test {
        #[arg(long, default_value = "apps")]
        apps: PathBuf,
        /// Only tests whose qualified name contains this.
        #[arg(long)]
        filter: Option<String>,
        /// Only these apps.
        names: Vec<String>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Serve {
            apps,
            data,
            addr,
            workers,
            io_threads,
            slice,
            max_connections,
            max_in_flight,
            backend,
            quiet,
        } => {
            let mut options = ServeOptions::new(apps);
            options.addr = addr;
            options.data = Some(data);
            if let Some(workers) = workers {
                options.workers = workers.max(1);
            }
            options.io_threads = io_threads;
            options.slice = (slice > 0.0).then(|| Duration::from_secs_f64(slice / 1e3));
            options.max_connections = max_connections;
            options.max_in_flight = max_in_flight;
            options.backend = backend;
            options.quiet = quiet;
            eprintln!("cove-host: loading apps from {}", options.apps.display());
            let host = match Host::start(options) {
                Ok(host) => host,
                Err(why) => {
                    eprintln!("cove-host: {why}");
                    return ExitCode::FAILURE;
                }
            };
            eprint!("{}", host.banner());
            host.wait_for_ctrl_c();
            eprintln!("cove-host: shutting down");
            ExitCode::SUCCESS
        }
        Command::Check { apps, names } => {
            finish(toolchain::check(&apps, &names, &HostModules::standard()))
        }
        Command::Test {
            apps,
            filter,
            names,
        } => finish(toolchain::test(
            &apps,
            &names,
            filter.as_deref(),
            &HostModules::standard(),
        )),
    }
}

fn finish(report: Result<toolchain::Report, String>) -> ExitCode {
    match report {
        Ok(report) => {
            eprint!("{}", report.err);
            print!("{}", report.out);
            if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(why) => {
            eprintln!("cove-host: {why}");
            ExitCode::FAILURE
        }
    }
}
