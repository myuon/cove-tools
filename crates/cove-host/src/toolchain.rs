//! `cove-host check` and `cove-host test`: the toolchain an app's author
//! runs, with the host's schemas and the host's modules.
//!
//! `cove check` cannot see `web`, `log` or `timer`: they are this crate's,
//! and cove#151 was closed by decision — the embedder ships its own checker.
//! `check` runs the compile, the admission and the lowering `serve` runs at
//! load ([`apps::compile`], [`apps::admit`], [`apps::lower`]), so it cannot
//! pass an app the server would refuse. `test` runs each `test fn` the way
//! `cove test` does — lowered as an entry of its own, on the VM, an `Err` is
//! a failure pointing at its assertion — with the app's grant (not the
//! test's derived one), the app's limits, and the host's modules answered
//! through their blocking path, since a test runs to its end on one thread.
//! Adapted from `examples/edge/host/src/toolchain.rs` in the Cove repository.

use std::path::Path;
use std::sync::Arc;

use cove_diag::{render, Diagnostic, Severity, Span};
use cove_runtime::{Budget, Runtime, Value, Vm};
use cove_sema::resolve::DeclaredTest;
use cove_sema::HostSchemas;

use crate::apps::{self, App, AppState, Compiled};
use crate::hosts::{AppContext, HostModules};

/// What a command printed, and whether it succeeded.
#[derive(Debug, Default)]
pub struct Report {
    pub out: String,
    pub err: String,
    pub ok: bool,
}

/// `cove-host check [app…]`.
///
/// Each app's modules are checked against the host's schemas and their
/// notices printed as `cove check` prints them; then each app gets one line —
/// what its entry requires, what `app.toml` grants, and whether the host
/// would load it. Fails if any app would be refused.
pub fn check(root: &Path, only: &[String], modules: &HostModules) -> Result<Report, String> {
    let mut report = Report::default();
    let mut refused = 0;
    let mut warnings = 0;
    let dirs = apps::app_dirs(root, only)?;
    for (name, dir) in &dirs {
        let (mut app, config) = apps::describe(name, dir);
        if config.is_some() {
            app.state = match apps::compile(dir, name, modules) {
                Ok(compiled) => {
                    for notice in &compiled.program.notices {
                        if notice.severity == Severity::Warning {
                            warnings += 1;
                        }
                        report.err.push_str(&render(&compiled.sources, notice));
                    }
                    match verdict(&mut app, &compiled, modules) {
                        Ok(()) => AppState::Refused(String::new()),
                        Err(why) => AppState::Refused(why),
                    }
                }
                Err(why) => {
                    report.err.push_str(&diagnostics_of(&why));
                    AppState::Refused(why)
                }
            };
        }
        let line = match &app.state {
            AppState::Refused(why) if why.is_empty() => "ok".to_string(),
            AppState::Refused(why) => {
                refused += 1;
                // A refusal that is not a diagnostic already printed above —
                // the grant, a spawn — is printed in full.
                if !why.starts_with("does not") && !why.starts_with("checks with warnings") {
                    if let Some((_, rest)) = why.split_once('\n') {
                        report.err.push_str(rest);
                        if !rest.ends_with('\n') {
                            report.err.push('\n');
                        }
                    }
                }
                format!("REFUSED: {}", why.lines().next().unwrap_or_default())
            }
            AppState::Ready(_) => unreachable!("check does not prepare"),
        };
        let open = if app.open { " (lower bound)" } else { "" };
        report.out.push_str(&format!(
            "{:<10} requires [{}]{open}  granted [{}]  {line}\n",
            app.name,
            apps::list(&app.required),
            apps::list(&app.granted),
        ));
    }
    report.out.push_str(&format!(
        "checked {} app(s) against the host's schemas ({}); {warnings} warning(s), {refused} refused\n",
        dirs.len(),
        modules
            .schemas()
            .iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>()
            .join(", "),
    ));
    report.ok = refused == 0;
    Ok(report)
}

fn verdict(app: &mut App, compiled: &Compiled, modules: &HostModules) -> Result<(), String> {
    apps::admit(app, compiled)?;
    apps::lower(app, compiled, modules).map(drop)
}

/// What a module that did not compile prints: the diagnostics alone.
fn diagnostics_of(why: &str) -> String {
    for stage in ["does not parse:\n", "does not check:\n"] {
        if let Some(rendered) = why.strip_prefix(stage) {
            return rendered.to_string();
        }
    }
    format!("{why}\n")
}

/// The diagnostic a failing test is reported as — `cove test`'s.
const FAILED: &str = "cove::test::failed";

/// `cove-host test [app…]`: every `test fn` in each app's modules.
pub fn test(
    root: &Path,
    only: &[String],
    filter: Option<&str>,
    modules: &HostModules,
) -> Result<Report, String> {
    let mut report = Report::default();
    let (mut ran, mut failed, mut uncompiled) = (0, 0, 0);
    for (name, dir) in apps::app_dirs(root, only)? {
        let (app, config) = apps::describe(&name, &dir);
        if config.is_none() {
            uncompiled += 1;
            if let AppState::Refused(why) = &app.state {
                report.out.push_str(&format!("fail  {name:<10} {why}\n"));
            }
            continue;
        }
        let compiled = match apps::compile(&dir, &name, modules) {
            Ok(compiled) => compiled,
            Err(why) => {
                uncompiled += 1;
                report
                    .out
                    .push_str(&format!("fail  {name:<10} does not compile\n"));
                report.err.push_str(&diagnostics_of(&why));
                continue;
            }
        };
        let sources = Arc::new(compiled.sources);
        let program = Arc::new(compiled.program);
        let tests = program.tests();
        // The app's own tests, not the standard library's.
        for test in tests.iter().filter(|test| {
            !(test.module == "std" || test.module.starts_with("std."))
                && filter.is_none_or(|filter| test.qualified_name().contains(filter))
        }) {
            ran += 1;
            let line = format!("{name:<10} {}", test.qualified_name());
            match run_test(test, &app, modules, &sources, &program) {
                None => report.out.push_str(&format!("ok    {line}\n")),
                Some(diagnostic) => {
                    failed += 1;
                    report.out.push_str(&format!("fail  {line}\n"));
                    report.err.push_str(&render(&sources, &diagnostic));
                }
            }
        }
    }
    let mut summary = format!("ran {ran} test(s), {} passed", ran - failed);
    if failed > 0 {
        summary.push_str(&format!(", {failed} failed"));
    }
    if uncompiled > 0 {
        summary.push_str(&format!("; {uncompiled} app(s) did not compile"));
    }
    report.out.push_str(&summary);
    report.out.push('\n');
    report.ok = failed == 0 && uncompiled == 0;
    Ok(report)
}

/// Runs one test as `cove test` would, with the app's grant, modules and
/// limits; the diagnostic to report when it failed.
fn run_test(
    test: &DeclaredTest,
    app: &App,
    modules: &HostModules,
    sources: &Arc<cove_diag::SourceMap>,
    program: &Arc<cove_sema::resolve::Program>,
) -> Option<Diagnostic> {
    let missing = test
        .entry
        .required_capabilities
        .iter()
        .map(|capability| capability.as_str().to_string())
        .find(|capability| !app.granted.contains(capability));
    if let Some(missing) = missing {
        return Some(
            Diagnostic::error(
                FAILED,
                format!(
                    "test `{}` requires `{missing}`, which app.toml does not grant app `{}`",
                    test.qualified_name(),
                    app.name
                ),
            )
            .at(test.entry.decl.name.span)
            .rule("`cove-host test` grants a test what the host grants its app."),
        );
    }
    let lowered = match cove_ir::lower_entry(
        program,
        sources,
        &HostSchemas::only(modules.schemas()),
        test.module,
        test.name,
    ) {
        Ok(ir) => ir,
        Err(items) => {
            return Some(
                Diagnostic::error(
                    FAILED,
                    format!(
                        "test `{}` could not be lowered: {}",
                        test.qualified_name(),
                        items
                            .iter()
                            .map(|item| item.message.clone())
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                )
                .at(test.entry.decl.name.span),
            )
        }
    };
    let context = AppContext {
        app: app.name.clone(),
        quiet: true,
        counters: Arc::clone(&app.counters),
    };
    let runtime = Runtime::new(
        Arc::clone(program),
        Arc::clone(sources),
        Arc::new(modules.registry(&app.granted, &context)),
    );
    let mut vm = Vm::new(&runtime, runtime.hosts(), &lowered);
    let outcome = vm.run_entry_within(
        Budget::new(app.limits.run.clone()),
        test.module,
        test.name,
        Vec::new(),
    );
    let assertion = vm
        .assertion_failure()
        .map(|(span, message)| (span, message.to_string()));
    match outcome {
        Ok(value) => {
            let message = failure_message(&value)?;
            Some(failure(test, &message, assertion))
        }
        Err(error) => {
            let mut diagnostic = error.to_diagnostic();
            diagnostic.message =
                format!("test `{}` failed: {}", test.qualified_name(), error.message);
            Some(diagnostic)
        }
    }
}

fn failure_message(value: &Value) -> Option<String> {
    Some(
        value
            .err_payload()?
            .first()
            .map(ToString::to_string)
            .unwrap_or_default(),
    )
}

fn failure(test: &DeclaredTest, message: &str, assertion: Option<(Span, String)>) -> Diagnostic {
    let span = match assertion {
        Some((span, recorded)) if recorded == message => span,
        _ => test.entry.decl.name.span,
    };
    Diagnostic::error(
        FAILED,
        format!("test `{}` failed: {message}", test.qualified_name()),
    )
    .at(span)
    .rule(
        "A test reports failure as an `Err`, the way every Cove function reports expected failure.",
    )
}
