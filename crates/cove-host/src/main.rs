//! `cove-host`: serve, check and test the Cove apps in a directory.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use cove_host::toolchain;
use cove_host::{Backend, Forwarding, Host, HostModules, OpsListener, PublicOrigin, ServeOptions};

#[derive(Parser)]
#[command(
    name = "cove-host",
    version = env!("COVE_HOST_VERSION"),
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
        /// Where the admin listener (updates) listens; keep it on localhost.
        #[arg(long, default_value = "127.0.0.1:8081")]
        admin: String,
        /// Run without an admin listener: no updates but a restart.
        #[arg(long)]
        no_admin: bool,
        /// Which listener serves `/_host/` (stats, ops page, app details,
        /// logs): `public`, unauthenticated as before, or `admin`, where the
        /// public listener answers 404 for all of `/_host/`. Use `admin`
        /// behind a reverse proxy.
        #[arg(long, default_value = "public")]
        ops_listener: OpsListener,
        /// The origin clients reach the host at, such as
        /// `https://tools.example`: apps are told this scheme and host
        /// whatever the request says (behind a TLS-terminating proxy).
        #[arg(long)]
        public_origin: Option<PublicOrigin>,
        /// Believe the request's `X-Forwarded-Proto` and `X-Forwarded-Host`
        /// (only behind a proxy that sets them); without it, and without
        /// `--public-origin`, apps are told `http` and the request's `Host`.
        #[arg(long)]
        trust_proxy: bool,
        /// On SIGTERM or Ctrl-C, how long to wait for requests in flight, in
        /// seconds; new ones are answered 503 meanwhile.
        #[arg(long, default_value_t = 10.0)]
        shutdown_grace: f64,
    },
    /// Load an app's directory again and switch the running host to the new
    /// version if it loads (or add the app, for a new name). In-flight
    /// requests finish on the old version.
    Update {
        app: String,
        #[command(flatten)]
        admin: AdminArgs,
    },
    /// Stop routing to an app on the running host.
    Remove {
        app: String,
        #[command(flatten)]
        admin: AdminArgs,
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

/// How `update` and `remove` reach the running host.
#[derive(clap::Args)]
struct AdminArgs {
    /// The host's admin listener.
    #[arg(long, default_value = "127.0.0.1:8081")]
    admin: String,
    /// The file holding the admin token; the host's `<data>/admin.token`.
    #[arg(long, default_value = "data/admin.token")]
    token_file: PathBuf,
}

/// Sends one admin request; prints the answer; fails unless it was a 2xx.
fn admin(args: &AdminArgs, method: reqwest::Method, path: &str) -> ExitCode {
    let token = match std::fs::read_to_string(&args.token_file) {
        Ok(token) => token.trim().to_string(),
        Err(e) => {
            eprintln!(
                "cove-host: cannot read the admin token from `{}`: {e}",
                args.token_file.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let url = format!("http://{}{path}", args.admin);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("cove-host: {e}");
            return ExitCode::FAILURE;
        }
    };
    let answer = runtime.block_on(async {
        let response = reqwest::Client::new()
            .request(method, &url)
            .bearer_auth(token)
            .timeout(Duration::from_secs(600))
            .send()
            .await?;
        let status = response.status();
        Ok::<_, reqwest::Error>((status, response.text().await?))
    });
    match answer {
        Ok((status, body)) if status.is_success() => {
            print!("{body}");
            ExitCode::SUCCESS
        }
        Ok((status, body)) => {
            eprint!("cove-host: {status}\n{body}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("cove-host: cannot reach the admin listener at {url}: {e}");
            ExitCode::FAILURE
        }
    }
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
            admin,
            no_admin,
            ops_listener,
            public_origin,
            trust_proxy,
            shutdown_grace,
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
            options.admin = (!no_admin).then_some(admin);
            options.ops_listener = ops_listener;
            options.forwarding = Forwarding {
                public_origin,
                trust_proxy,
            };
            eprintln!("cove-host: loading apps from {}", options.apps.display());
            let host = match Host::start(options) {
                Ok(host) => host,
                Err(why) => {
                    eprintln!("cove-host: {why}");
                    return ExitCode::FAILURE;
                }
            };
            eprintln!("cove-host {}", env!("COVE_HOST_VERSION"));
            eprint!("{}", host.banner());
            host.wait_for_signal();
            eprintln!("cove-host: shutting down");
            let left = host.shutdown(Duration::from_secs_f64(shutdown_grace.max(0.0)));
            if left > 0 {
                eprintln!("cove-host: {left} request(s) still in flight after {shutdown_grace} s; stopping anyway");
            }
            ExitCode::SUCCESS
        }
        Command::Update { app, admin: args } => {
            admin(&args, reqwest::Method::POST, &format!("/apps/{app}/update"))
        }
        Command::Remove { app, admin: args } => {
            admin(&args, reqwest::Method::DELETE, &format!("/apps/{app}"))
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
