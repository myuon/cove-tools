//! Loading an app: read its config, check it once, check its authority,
//! prepare it once, compile it once.
//!
//! Every app is compiled as a package of its own — its modules and the
//! standard library — so an app cannot `use` another app's code, and what the
//! checker derives about its entry is about its code alone. What is paid here
//! is paid once per app: parsing, checking, lowering, `PreparedProgram::new`'s
//! encoding and verification, and on the native tier the code generator. A
//! request pays for an `OwnedVm` and nothing above it.
//!
//! An app is refused — and every other app still starts — when its config
//! does not read, it does not check (warnings included), its entry requires a
//! capability `app.toml` does not grant, or its code can `spawn` a task.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_diag::{render, Diagnostic, Severity, SourceMap};
use cove_ir::Inst;
use cove_runtime::{HostRegistry, OwnedVm, PreparedProgram, Runtime};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program;
use cove_sema::{Compiler, Config, HostSchemas};

use crate::config::{read_app, AppConfig, AppLimits};
use crate::hosts::{AppContext, HostModules};
use crate::stats::AppCounters;

/// Which tier runs an app's requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    /// The native tier where this host has one (Unix x86-64), and the
    /// encoded VM elsewhere, saying so once.
    #[default]
    Auto,
    /// The encoded VM, always.
    Vm,
    /// The native tier, and an app is refused where there is none.
    Native,
}

impl std::str::FromStr for Backend {
    type Err = String;
    fn from_str(text: &str) -> Result<Backend, String> {
        match text {
            "auto" => Ok(Backend::Auto),
            "vm" => Ok(Backend::Vm),
            "native" => Ok(Backend::Native),
            other => Err(format!("`{other}` is not a backend: auto, vm or native")),
        }
    }
}

/// How apps are loaded.
#[derive(Clone)]
pub struct LoadOptions {
    pub backend: Backend,
    /// Whether `log` prints nothing.
    pub quiet: bool,
    pub modules: HostModules,
}

/// One app, ready or refused.
pub struct App {
    /// The name that routes to it: `/hello/...` reaches `hello`.
    pub name: String,
    pub dir: PathBuf,
    /// `module.function`.
    pub entry: String,
    /// What `app.toml` grants.
    pub granted: BTreeSet<String>,
    /// What the checker derived the entry requires, when it checked.
    pub required: BTreeSet<String>,
    /// Whether `required` is a lower bound: the entry makes a call the call
    /// graph cannot follow.
    pub open: bool,
    pub limits: AppLimits,
    pub counters: Arc<AppCounters>,
    pub state: AppState,
}

/// Whether an app serves.
pub enum AppState {
    Ready(Box<Ready>),
    /// Not loaded, and why: printed at startup, answered to every request.
    Refused(String),
}

/// Everything a request's isolate is built from, shared by all of them.
pub struct Ready {
    pub module: String,
    pub function: String,
    /// For rendering a runtime error into a response.
    pub sources: Arc<SourceMap>,
    /// The app's grants and module instances, shared by every run: a run's
    /// budget is the run's, never the registry's (cove#577).
    pub hosts: Arc<HostRegistry>,
    pub runtime: Arc<Runtime>,
    /// Encoded, verified — and compiled, on the native tier — once.
    pub program: PreparedProgram,
    /// `"native"` or `"vm"`.
    pub tier: &'static str,
    pub cost: LoadCost,
}

/// What loading one app cost.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadCost {
    pub check: Duration,
    pub prepare: Duration,
    pub functions: usize,
}

impl Ready {
    /// A fresh isolate: its own heap, stack and budget, nothing of any other
    /// run's.
    pub fn isolate(&self) -> OwnedVm {
        OwnedVm::new(
            Arc::clone(&self.runtime),
            Arc::clone(&self.hosts),
            self.program.clone(),
        )
    }
}

impl App {
    pub fn ready(&self) -> Option<&Ready> {
        match &self.state {
            AppState::Ready(ready) => Some(ready),
            AppState::Refused(_) => None,
        }
    }

    /// The one-line account printed at startup and by `cove-host check`.
    pub fn describe(&self) -> String {
        let open = if self.open { " (lower bound)" } else { "" };
        let verdict = match &self.state {
            AppState::Ready(ready) => format!(
                "ok: {} fn on {}, checked in {:.1} ms, prepared in {:.1} ms",
                ready.cost.functions,
                ready.tier,
                ready.cost.check.as_secs_f64() * 1e3,
                ready.cost.prepare.as_secs_f64() * 1e3,
            ),
            AppState::Refused(why) => {
                format!("REFUSED: {}", why.lines().next().unwrap_or_default())
            }
        };
        format!(
            "{:<10} requires [{}]{open}  granted [{}]  {verdict}",
            self.name,
            list(&self.required),
            list(&self.granted),
        )
    }
}

pub fn list(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        "-".to_string()
    } else {
        set.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// The app directories under `root`, sorted: every directory holding an
/// `app.toml`. `only` narrows them to the names it lists.
pub fn app_dirs(root: &Path, only: &[String]) -> Result<Vec<(String, PathBuf)>, String> {
    let entries =
        std::fs::read_dir(root).map_err(|e| format!("cannot read `{}`: {e}", root.display()))?;
    let mut dirs: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("app.toml").is_file())
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_string();
            Some((name, path))
        })
        .collect();
    dirs.sort();
    for name in only {
        if !dirs.iter().any(|(found, _)| found == name) {
            return Err(format!(
                "no app named `{name}` in `{}` (an app is a directory with an `app.toml`)",
                root.display()
            ));
        }
    }
    if !only.is_empty() {
        dirs.retain(|(name, _)| only.contains(name));
    }
    Ok(dirs)
}

/// Loads every app under `root`. A refusal is per app; the error is only for
/// a directory that cannot be read.
pub fn load_all(root: &Path, options: &LoadOptions) -> Result<Vec<App>, String> {
    Ok(app_dirs(root, &[])?
        .into_iter()
        .map(|(name, dir)| load(&name, &dir, options))
        .collect())
}

/// An app as its directory and config describe it, before anything is
/// compiled; refused if the config does not read.
pub fn describe(name: &str, dir: &Path) -> (App, Option<AppConfig>) {
    let mut app = App {
        name: name.to_string(),
        dir: dir.to_path_buf(),
        entry: format!("{name}.handle"),
        granted: BTreeSet::new(),
        required: BTreeSet::new(),
        open: false,
        limits: AppLimits::default(),
        counters: Arc::new(AppCounters::default()),
        state: AppState::Refused(String::new()),
    };
    if let Err(why) = valid_name(name) {
        app.state = AppState::Refused(why);
        return (app, None);
    }
    match read_app(dir, name) {
        Ok(config) => {
            app.entry = config.entry.clone();
            app.granted = config.granted.clone();
            app.limits = config.limits.clone();
            (app, Some(config))
        }
        Err(why) => {
            app.state = AppState::Refused(why);
            (app, None)
        }
    }
}

/// An app's name is its route and its main module's name, so it has to be
/// both: a Cove identifier, and not one of the host's reserved prefixes.
fn valid_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
    if !first_ok || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "`{name}` is not an app name: a letter, then letters, digits or `_` \
             (it is the app's route and its module's name)"
        ));
    }
    Ok(())
}

/// Loads one app.
pub fn load(name: &str, dir: &Path, options: &LoadOptions) -> App {
    let (mut app, config) = describe(name, dir);
    if config.is_none() {
        return app;
    }
    app.state = match prepare(&mut app, options) {
        Ok(ready) => AppState::Ready(Box::new(ready)),
        Err(why) => AppState::Refused(why),
    };
    app
}

/// An app's modules, checked against the host's schemas.
pub struct Compiled {
    pub sources: SourceMap,
    pub program: Program,
    pub check: Duration,
}

/// Loads and checks the app in `dir` against `modules`' schemas.
///
/// The `Err` is rendered, after the stage that refused it.
pub fn compile(dir: &Path, name: &str, modules: &HostModules) -> Result<Compiled, String> {
    let started = Instant::now();
    let (sources, package) = load_package(dir, name)?;
    let program = Compiler::new()
        .with_schemas(HostSchemas::only(modules.schemas()))
        .compile(&package)
        .map_err(|items| format!("does not check:\n{}", report(&sources, &items)))?;
    Ok(Compiled {
        sources,
        program,
        check: started.elapsed(),
    })
}

/// Whether a checked app may be loaded: it checks without warnings, it
/// declares its entry, and the entry requires nothing `app.toml` does not
/// grant. Fills in what the entry requires.
///
/// The decision `serve` and `check` share, so that the checker cannot pass an
/// app the server would refuse.
pub fn admit(app: &mut App, compiled: &Compiled) -> Result<(), String> {
    let (module, function) = app
        .entry
        .split_once('.')
        .ok_or_else(|| format!("entry `{}` is not `module.function`", app.entry))?;
    let warnings: Vec<Diagnostic> = compiled
        .program
        .notices
        .iter()
        .filter(|item| item.severity == Severity::Warning)
        .cloned()
        .collect();
    if !warnings.is_empty() {
        return Err(format!(
            "checks with warnings:\n{}",
            report(&compiled.sources, &warnings)
        ));
    }
    let entry = compiled
        .program
        .lookup_fn(module, function)
        .ok_or_else(|| format!("`{module}` declares no `{function}`"))?;
    app.required = entry
        .required_capabilities
        .iter()
        .map(|capability| capability.as_str().to_string())
        .collect();
    app.open = entry.is_capability_open();
    let missing: Vec<String> = app.required.difference(&app.granted).cloned().collect();
    if !missing.is_empty() {
        return Err(format!(
            "`{}` requires {}, which app.toml does not grant",
            app.entry,
            missing
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(" and ")
        ));
    }
    Ok(())
}

/// Lowers the entry and refuses a program that can `spawn`.
///
/// A spawned task is a thread of its own, outside this host's worker pool and
/// its accounting; while one is alive its parent can neither park nor yield
/// (ADR 0080 §2, ADR 0084 §3), and it has no native tier (ADR 0085). So an
/// app that can spawn could hold a worker for its whole deadline and start
/// threads the pool does not count. Rather than admit it with a limit that
/// holds only in part, the host refuses it here, naming the `spawn`; the
/// run's `max_tasks = 0` is the backstop.
pub fn lower(
    app: &App,
    compiled: &Compiled,
    modules: &HostModules,
) -> Result<cove_ir::Program, String> {
    let (module, function) = app.entry.split_once('.').unwrap_or_default();
    let schemas = HostSchemas::only(modules.schemas());
    let lowered = cove_ir::lower_entry(
        &compiled.program,
        &compiled.sources,
        &schemas,
        module,
        function,
    )
    .map_err(|items| format!("does not lower:\n{}", report(&compiled.sources, &items)))?;
    refuse_spawn(&lowered, &compiled.sources)?;
    Ok(lowered)
}

/// The first `spawn` the lowered program can reach, as a refusal.
fn refuse_spawn(program: &cove_ir::Program, sources: &SourceMap) -> Result<(), String> {
    for function in &program.functions {
        for (at, inst) in function.code.iter().enumerate() {
            if matches!(inst, Inst::Spawn { .. }) {
                let mut diagnostic = Diagnostic::error(
                    "cove_host::spawn",
                    format!(
                        "`{}.{}` can spawn a task, which cove-host does not run",
                        function.module, function.name
                    ),
                )
                .rule(
                    "A spawned task runs outside the host's worker pool and keeps its parent \
                     from parking or yielding; cove-host refuses an app that can spawn.",
                );
                if let Some(span) = function.spans.get(at) {
                    diagnostic = diagnostic.at(*span);
                }
                return Err(format!(
                    "can spawn a task:\n{}",
                    render(sources, &diagnostic)
                ));
            }
        }
    }
    Ok(())
}

/// Compiles, checks the grant, lowers, prepares; or says why not.
fn prepare(app: &mut App, options: &LoadOptions) -> Result<Ready, String> {
    let compiled = compile(&app.dir, &app.name, &options.modules)?;
    admit(app, &compiled)?;
    let started = Instant::now();
    let lowered = lower(app, &compiled, &options.modules)?;
    let functions = lowered.functions.len();
    let prepared = PreparedProgram::new(Arc::new(lowered));
    let (program, tier) = match options.backend {
        Backend::Vm => (prepared, "vm"),
        Backend::Native => (
            prepared
                .with_native()
                .map_err(|why| format!("`--backend native`: {why}"))?,
            "native",
        ),
        Backend::Auto => match prepared.clone().with_native() {
            Ok(native) => (native, "native"),
            Err(why) => {
                note_fallback(&why.to_string());
                (prepared, "vm")
            }
        },
    };
    let prepare = started.elapsed();

    let (module, function) = app.entry.split_once('.').unwrap_or_default();
    let (module, function) = (module.to_string(), function.to_string());
    let context = AppContext {
        app: app.name.clone(),
        quiet: options.quiet,
        counters: Arc::clone(&app.counters),
    };
    let hosts = Arc::new(options.modules.registry(&app.granted, &context));
    let sources = Arc::new(compiled.sources);
    let runtime = Arc::new(Runtime::new(
        Arc::new(compiled.program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    ));
    Ok(Ready {
        module,
        function,
        sources,
        hosts,
        runtime,
        program,
        tier,
        cost: LoadCost {
            check: compiled.check,
            prepare,
            functions,
        },
    })
}

/// Says once per process that the native tier is not available here.
fn note_fallback(why: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        eprintln!(
            "cove-host: the native tier is not available on this host ({}); \
             apps run on the encoded VM",
            why.lines().next().unwrap_or_default()
        );
    });
}

/// The app's modules and the standard library, as a package of their own.
///
/// The `.cove` files directly in the app's directory are the module named
/// after the app; each subdirectory holding `.cove` files is a module named
/// after the subdirectory, which the app's files may `use`. Nothing outside
/// the directory is read.
pub fn load_package(dir: &Path, name: &str) -> Result<(SourceMap, Package), String> {
    let mut sources = SourceMap::new();
    let mut modules = BTreeMap::new();
    let mut main = None;
    let mut subdirs = Vec::new();
    for entry in
        std::fs::read_dir(dir).map_err(|e| format!("cannot read `{}`: {e}", dir.display()))?
    {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            subdirs.push(path);
        }
    }
    subdirs.sort();
    for (module, module_dir) in std::iter::once((name.to_string(), dir.to_path_buf())).chain(
        subdirs.into_iter().filter_map(|path| {
            let module = path.file_name()?.to_str()?.to_string();
            Some((module, path))
        }),
    ) {
        let units = read_units(&module_dir, dir, &mut sources)?;
        if units.is_empty() {
            continue;
        }
        if module == name {
            main = Some(());
        }
        modules.insert(
            module.clone(),
            Module {
                name: module,
                dir: module_dir,
                units,
            },
        );
    }
    if main.is_none() {
        return Err(format!("`{}` holds no `.cove` file", dir.display()));
    }
    cove_sema::stdlib::install(&mut sources, &mut modules)
        .map_err(|items| report(&sources, &items))?;
    let package = Package {
        root: dir.to_path_buf(),
        config: Config::default(),
        modules,
    };
    Ok((sources, package))
}

/// The `.cove` files directly in `dir`, parsed. Named relative to the app's
/// parent directory, so that an error points at `hello/hello.cove:3`.
fn read_units(dir: &Path, app_dir: &Path, sources: &mut SourceMap) -> Result<Vec<Unit>, String> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read `{}`: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("cove"))
        .collect();
    paths.sort();
    let shown_root = app_dir.parent().unwrap_or(app_dir);
    let mut units = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
        let shown = path.strip_prefix(shown_root).unwrap_or(&path).to_path_buf();
        let file = sources.add(shown, &text);
        let ast = cove_syntax::parse_file(sources, file)
            .map_err(|items| format!("does not parse:\n{}", report(sources, &items)))?;
        units.push(Unit { file, path, ast });
    }
    Ok(units)
}

/// Diagnostics, rendered the way `cove check` renders them.
pub fn report(sources: &SourceMap, items: &[Diagnostic]) -> String {
    items.iter().map(|item| render(sources, item)).collect()
}
