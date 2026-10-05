//! The algorithm playground (`apps/algo`, issue #4) on a host started
//! in-process: known answers, the run's meter and stop reasons, cancellation,
//! and the other apps answering while it computes.
//!
//! No assertion here is about a duration. What is asserted about
//! responsiveness is that requests to the other apps *complete* while heavy
//! runs are in flight on a host with fewer workers than heavy clients, and
//! that the heavy runs yielded.

mod common;

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use common::*;

const WEBHOOKS_SECRET: &str = "test-admin-secret";

/// `body` as a form value.
fn form_value(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Runs `graph` with `algorithm` and answers the result fragment.
fn run_matching(addr: SocketAddr, graph: &str, algorithm: &str) -> Answer {
    let body = format!(
        "graph={}&algorithm={algorithm}&part=result",
        form_value(graph)
    );
    send_raw(
        addr,
        format!(
            "POST /algo/matching HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
}

/// The number after `prefix` in `text`, thousands separators and all.
fn number_after(text: &str, prefix: &str) -> u64 {
    let at = text
        .find(prefix)
        .unwrap_or_else(|| panic!("no `{prefix}` in {text}"));
    text[at + prefix.len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

/// The number just before `suffix` in `text`, thousands separators and all.
fn number_before(text: &str, suffix: &str) -> u64 {
    let at = text
        .find(suffix)
        .unwrap_or_else(|| panic!("no `{suffix}` in {text}"));
    let digits: String = text[..at]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(char::is_ascii_digit)
        .collect();
    digits.chars().rev().collect::<String>().parse().unwrap()
}

/// The matching size a result reports, after checking the page proved it.
fn proved_size(answer: &Answer) -> u64 {
    assert_eq!(answer.status, 200, "{answer:?}");
    assert!(
        answer.body.contains("data-proved=true"),
        "not proved: {}",
        answer.body
    );
    number_after(&answer.body, "Maximum: ")
}

/// Runs `formula` with a decision `budget` (`""` for the default) and
/// answers the result fragment.
fn run_sat(addr: SocketAddr, formula: &str, budget: &str) -> Answer {
    let body = format!(
        "formula={}&budget={budget}&part=result",
        form_value(formula)
    );
    send_raw(
        addr,
        format!(
            "POST /algo/sat HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
}

/// A SAT result's verdict: `sat` (and then the page checked the
/// assignment), `unsat` or `unknown`.
fn verdict(answer: &Answer) -> &str {
    assert_eq!(answer.status, 200, "{answer:?}");
    let at = answer
        .body
        .find("data-verdict=")
        .unwrap_or_else(|| panic!("no verdict: {}", answer.body));
    let rest = &answer.body[at + "data-verdict=".len()..];
    let verdict = &rest[..rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap()];
    if verdict == "sat" {
        assert!(rest.contains("data-checked=true"), "{}", answer.body);
    }
    verdict
}

/// The `v` line of a satisfiable result: each variable's value.
fn assignment(answer: &Answer) -> Vec<i64> {
    let marker = "<pre class=diag>v ";
    let at = answer.body.find(marker).expect("a v line");
    let line = &answer.body[at + marker.len()..];
    line[..line.find("</pre>").unwrap()]
        .split_whitespace()
        .map(|n| n.parse::<i64>().unwrap())
        .take_while(|n| *n != 0)
        .collect()
}

#[test]
fn sat_answers_known_formulas_and_checks_its_models() {
    let apps = apps(&[sample("algo")]);
    let host = start(&apps, 2);
    let addr = host.addr;
    for (formula, expected) in [
        ("p cnf 3 2\n1 -3 0\n2 3 -1 0\n", "sat"),
        ("1 2 0\n-1 2 0\n1 -2 0\n-1 -2 0\n", "unsat"),
        ("1 0\n-1 0\n", "unsat"),
        ("pigeonhole 6 5", "unsat"),
        ("pigeonhole 5 5", "sat"),
        ("random 3 60 256 1", "sat"),
        ("random 3 60 256 2", "unsat"),
        ("random 3 60 256 3", "unsat"),
    ] {
        assert_eq!(verdict(&run_sat(addr, formula, "")), expected, "{formula}");
    }
    // The unique model of three of the four two-variable clauses.
    let one = run_sat(addr, "1 2 0\n-1 2 0\n1 -2 0\n", "");
    assert_eq!(assignment(&one), vec![1, 2]);
    // A model of a random formula satisfies it, checked here as well as in
    // the page: every clause has a true literal.
    let random = run_sat(addr, "random 3 60 256 1", "");
    let model = assignment(&random);
    assert_eq!(model.len(), 60);
    let formula_start = random
        .body
        .find("p cnf 60 256")
        .expect("the formula as read");
    let clauses: Vec<Vec<i64>> = random.body[formula_start..]
        .lines()
        .skip(1)
        .take_while(|line| line.ends_with(" 0"))
        .map(|line| {
            line.split_whitespace()
                .map(|n| n.parse::<i64>().unwrap())
                .take_while(|n| *n != 0)
                .collect()
        })
        .collect();
    // The page shows the first 200 of them.
    assert_eq!(clauses.len(), 200);
    for clause in &clauses {
        assert!(
            clause
                .iter()
                .any(|literal| model[literal.unsigned_abs() as usize - 1] == *literal),
            "{clause:?} unsatisfied"
        );
    }
    // The sudoku: solved, drawn as its grid, and its first row is the known
    // one.
    let sudoku = run_sat(
        addr,
        "sudoku 53..7....6..195....98....6.8...6...34..8.3..17...2...6.6....28....419..5....8..79",
        "",
    );
    assert_eq!(verdict(&sudoku), "sat");
    assert!(sudoku.body.contains("the solved sudoku"));
    let model = assignment(&sudoku);
    let first_row: Vec<i64> = (0..9)
        .map(|column| (1..=9).find(|d| model[9 * column + d - 1] > 0).unwrap() as i64)
        .collect();
    assert_eq!(first_row, vec![5, 3, 4, 6, 7, 8, 9, 1, 2]);
    // A budget the search runs past is the app's own stop, in the page.
    let gave_up = run_sat(addr, "pigeonhole 8 7", "100");
    assert_eq!(verdict(&gave_up), "unknown");
    assert!(gave_up.body.contains("data-stop=app"));
    assert!(gave_up.body.contains("budget of 100 decisions"));
    // What it cannot read is answered with why, and names are escaped.
    let bad = run_sat(addr, "1 2 0\n3 <x> 0\n", "");
    assert!(bad.body.contains("data-stop=app"), "{}", bad.body);
    assert!(bad.body.contains("line 2"));
    assert!(!bad.body.contains("<x>"));
    let page = get(addr, "/algo/sat?example=sudoku");
    assert_eq!(page.status, 200);
    assert!(page.body.contains("data-autorun"));
    assert!(page.body.contains("sudoku 53..7"));
}

#[test]
fn a_sat_run_out_of_fuel_says_so() {
    let apps = apps(&[sample("algo")]);
    let host = start(&apps, 1);
    let answer = send_raw(
        host.addr,
        b"GET /algo/sat?example=heavy&part=result HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(answer.status, 500, "{answer:?}");
    assert_eq!(answer.header("x-cove-stop"), Some("fuel"));
    assert!(answer.body.contains("fuel budget of 400000000 exhausted"));
}

/// Runs the annealing page with `query` and answers the result.
fn run_anneal(addr: SocketAddr, query: &str) -> Answer {
    let answer = get(addr, &format!("/algo/anneal?{query}&part=result"));
    assert_eq!(answer.status, 200, "{answer:?}");
    answer
}

/// The best length a run reports: `run A</dt><dd>best <strong>6.2157`.
fn best_of(answer: &Answer, run: &str) -> f64 {
    let marker = format!("run {run}</dt><dd>best <strong>");
    let at = answer
        .body
        .find(&marker)
        .unwrap_or_else(|| panic!("no run {run}: {}", answer.body));
    let rest = &answer.body[at + marker.len()..];
    rest[..rest.find('<').unwrap()].parse().unwrap()
}

#[test]
fn annealing_compares_two_runs_reproducibly() {
    let apps = apps(&[sample("algo")]);
    let host = start(&apps, 2);
    let addr = host.addr;
    // The default comparison: the hot start beats the cold one, both
    // answers are tours, and a second run is the same run.
    let first = run_anneal(addr, "example=cooling");
    assert!(first.body.contains("data-checked=true"), "{}", first.body);
    let (a, b) = (best_of(&first, "A"), best_of(&first, "B"));
    assert!(a < b, "annealed {a} against quenched {b}");
    let again = run_anneal(addr, "example=cooling");
    assert_eq!((best_of(&again, "A"), best_of(&again, "B")), (a, b));
    // Another run seed is another run; the problem seed is another problem.
    let reseeded = run_anneal(addr, "example=cooling&a_seed=9");
    assert_ne!(best_of(&reseeded, "A"), a);
    assert_eq!(best_of(&reseeded, "B"), b);
    // One axis, two series, a legend, the references, both tours, a table.
    assert_eq!(first.body.matches("<path class=current").count(), 2);
    assert!(first.body.contains("stroke-dasharray=\"6 4\""));
    assert!(first.body.contains("greedy 6.989"));
    assert!(
        first.body.contains("run A&#39;s best tour") || first.body.contains("run A's best tour")
    );
    assert!(first.body.contains("Table view: the trajectories"));
    // On the circle the optimum is known, and found.
    let circle = run_anneal(addr, "example=circle");
    assert!(
        circle.body.contains("The better one is the known optimum."),
        "{}",
        circle.body
    );
    let optimum = 40.0 * 2.0 * 0.45 * (std::f64::consts::PI / 40.0).sin();
    assert!(
        (best_of(&circle, "A") - optimum).abs() < 1e-3,
        "{}",
        best_of(&circle, "A")
    );
    // What it cannot run is answered with why.
    for (query, why) in [
        ("n=3", "4 to 500 points"),
        ("a_iterations=0", "run A takes 1 to 5,000,000 iterations"),
        ("b_start=-1", "run B&#39;s temperatures"),
        ("a_seed=x", "run A&#39;s seed"),
    ] {
        let refused = run_anneal(addr, query);
        assert!(
            refused.body.contains("data-stop=app"),
            "{query}: {}",
            refused.body
        );
        assert!(refused.body.contains(why), "{query}: {}", refused.body);
    }
    let page = get(addr, "/algo/anneal?example=circle");
    assert!(page.body.contains("data-autorun"));
    assert!(page.body.contains("value=circle checked"));
    // Too much for the fuel: the host stops it and says so.
    let heavy = get(addr, "/algo/anneal?example=heavy&part=result");
    assert_eq!(heavy.status, 500, "{heavy:?}");
    assert_eq!(heavy.header("x-cove-stop"), Some("fuel"));
}

#[test]
fn the_examples_have_their_known_maximum_and_a_proof() {
    let apps = apps(&[sample("algo")]);
    let host = start(&apps, 2);
    let addr = host.addr;
    for (graph, size) in [
        (
            "ann: cook clean\nbob: cook\ncat: clean drive\ndan: drive fix\neve: fix serve\n",
            5,
        ),
        (
            "s1: logic algebra\ns2: logic algebra\ns3: logic algebra\ns4: algebra topology geometry\n",
            3,
        ),
        ("a x\na y\nb x\n", 2),
        ("left: a b c\nright: x\n", 0),
    ] {
        for algorithm in ["hopcroft-karp", "augmenting", "both"] {
            let answer = run_matching(addr, graph, algorithm);
            assert_eq!(proved_size(&answer), size, "{graph:?} by {algorithm}");
        }
    }
    // A generated graph is the same graph every time, and both algorithms
    // find a matching of the same size, which the cover proves maximum.
    let first = run_matching(addr, "random 2000 2000 12000 42", "both");
    let size = proved_size(&first);
    assert!(
        first.body.contains("The two agree on the size"),
        "{first:?}"
    );
    assert_eq!(
        proved_size(&run_matching(
            addr,
            "random 2000 2000 12000 42",
            "hopcroft-karp"
        )),
        size
    );
    // Too large to draw: the table still lists it.
    assert!(!first.body.contains("<svg"));
    assert!(first.body.contains("Table view"));
    // A small one is drawn, with its legend.
    let drawn = run_matching(addr, "random 14 14 34 7", "hopcroft-karp");
    assert!(drawn.body.contains("<svg"));
    assert!(drawn.body.contains("in König&#39;s cover") || drawn.body.contains("in König's cover"));
    assert!(drawn.body.contains("class=matched"));
}

#[test]
fn the_pages_carry_the_meter_and_escape_what_they_are_given() {
    let apps = apps(&[sample("algo")]);
    let host = start(&apps, 1);
    let addr = host.addr;
    let index = get(addr, "/algo/");
    assert_eq!(index.status, 200);
    assert!(index.body.contains("/algo/matching"));
    let page = get(addr, "/algo/matching?example=seminars");
    assert_eq!(page.status, 200);
    let csp = page.header("content-security-policy").unwrap();
    assert!(csp.contains("script-src 'self'"), "{csp}");
    assert!(page.body.contains("data-autorun"));
    assert!(page.body.contains("s1: logic algebra"));
    let script = get(addr, "/algo/app.js");
    assert_eq!(script.status, 200);
    assert!(script
        .header("content-type")
        .unwrap()
        .starts_with("text/javascript"));
    assert!(script.body.contains("AbortController"));
    assert!(script.body.contains("x-cove-stop"));
    // Every answer of a run says what the run cost.
    let answer = run_matching(addr, "a x\nb y\n", "hopcroft-karp");
    for name in [
        "x-cove-run-fuel",
        "x-cove-run-instructions",
        "x-cove-run-worker-us",
    ] {
        let value: u64 = answer.header(name).unwrap().parse().unwrap();
        assert!(value > 0, "{name}");
    }
    assert_eq!(answer.header("x-cove-run-yields"), Some("0"));
    assert_eq!(answer.header("x-cove-stop"), None);
    // Names are text, wherever they land.
    let hostile = run_matching(addr, "<script>alert(1)</script> \"x'&\n", "both");
    assert_eq!(hostile.status, 200);
    assert!(!hostile.body.contains("<script>"), "{}", hostile.body);
    assert!(hostile
        .body
        .contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    // A graph it cannot read, or one over the bounds, is answered with why.
    let bad = run_matching(addr, "a b c\n", "hopcroft-karp");
    assert_eq!(bad.status, 200);
    assert!(bad.body.contains("data-stop=app"), "{}", bad.body);
    assert!(bad.body.contains("line 1"));
    let big = run_matching(addr, "random 6000 10 10 1", "hopcroft-karp");
    assert!(big.body.contains("1 to 5000 vertices"), "{}", big.body);
    // The form posts without the script, and the answer is the whole page.
    let body = format!("graph={}&algorithm=both", form_value("a x\na y\nb x\n"));
    let posted = send_raw(
        addr,
        format!(
            "POST /algo/matching HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
    assert!(posted.body.starts_with("<!doctype html>"));
    assert_eq!(proved_size(&posted), 2);
}

#[test]
fn a_run_a_limit_stops_says_which_limit() {
    // The shipped limits, with less fuel, and a second copy with a short
    // deadline instead.
    let shipped = std::fs::read_to_string(samples().join("algo/app.toml")).unwrap();
    assert!(
        shipped.contains("fuel = 400000000"),
        "the shipped app.toml changed shape"
    );
    let low_fuel = shipped.replace("fuel = 400000000", "fuel = 20000000");
    let short = shipped.replace("deadline = \"10s\"", "deadline = \"30ms\"");
    let apps = apps(&[
        AppSpec {
            name: "algo",
            from: samples().join("algo"),
            config: Some(&low_fuel),
        },
        AppSpec {
            name: "hurried",
            from: samples().join("algo"),
            config: Some(&short),
        },
    ]);
    let host = start(&apps, 2);
    let addr = host.addr;
    let heavy = "random 5000 5000 50000 1";
    let out_of_fuel = run_matching(addr, heavy, "augmenting");
    assert_eq!(out_of_fuel.status, 500, "{out_of_fuel:?}");
    assert_eq!(out_of_fuel.header("x-cove-stop"), Some("fuel"));
    assert!(out_of_fuel
        .body
        .contains("fuel budget of 20000000 exhausted"));
    let fuel: u64 = out_of_fuel
        .header("x-cove-run-fuel")
        .unwrap()
        .parse()
        .unwrap();
    assert!(fuel >= 20_000_000, "{fuel}");
    assert_eq!(count(&host, "algo", "errors.fuel"), 1);
    // A small graph is still fine on the same budget.
    assert_eq!(proved_size(&run_matching(addr, "a x\n", "both")), 1);

    let body = format!(
        "graph={}&algorithm=augmenting&part=result",
        form_value(heavy)
    );
    let late = send_raw(
        addr,
        format!(
            "POST /hurried/matching HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
    assert_eq!(late.status, 504, "{late:?}");
    assert_eq!(late.header("x-cove-stop"), Some("deadline"));
    assert_eq!(count(&host, "hurried", "errors.deadline"), 1);
}

#[test]
fn a_client_that_goes_away_cancels_its_run() {
    let apps = apps(&[sample("algo")]);
    let host = start(&apps, 1);
    let addr = host.addr;
    let body = format!(
        "graph={}&algorithm=augmenting&part=result",
        form_value("random 5000 5000 50000 1")
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .write_all(
            format!(
                "POST /algo/matching HTTP/1.1\r\nHost: localhost\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .unwrap();
    wait_until("the heavy run to start", || {
        count(&host, "algo", "in_flight") == 1
    });
    // What a page's Cancel does: the fetch is aborted, the connection closed.
    drop(stream);
    wait_until("the run to be cancelled", || {
        count(&host, "algo", "errors.cancelled") == 1
    });
    assert_eq!(count(&host, "algo", "in_flight"), 0);
    assert_eq!(count(&host, "algo", "errors.fuel"), 0);
    // The app serves the next request as usual.
    assert_eq!(
        proved_size(&run_matching(addr, "a x\n", "hopcroft-karp")),
        1
    );
}

/// The webhook lab's shipped config with its admin secret given literally.
fn webhooks_config() -> String {
    let shipped = std::fs::read_to_string(samples().join("webhooks/app.toml")).unwrap();
    let config = shipped.replace(
        "admin = { env = \"WEBHOOKS_ADMIN_TOKEN\" }",
        &format!("admin = {{ value = \"{WEBHOOKS_SECRET}\" }}"),
    );
    assert!(
        config.contains(WEBHOOKS_SECRET),
        "the shipped app.toml changed shape"
    );
    config
}

/// Heavy matching runs from more clients than the host has workers, and
/// meanwhile `hello` and the webhook lab answer: every one of their requests
/// completes, and the heavy runs were sliced — they yielded, because the
/// other apps' requests were waiting.
fn the_other_apps_answer_while_algo_computes(backend: &str) {
    let config = webhooks_config();
    let apps = apps(&[
        sample("algo"),
        sample("hello"),
        AppSpec {
            name: "webhooks",
            from: samples().join("webhooks"),
            config: Some(&config),
        },
    ]);
    let mut options = options(&apps, 2);
    options.backend = backend.parse().unwrap();
    let host = cove_host::Host::start(options).expect("the host starts");
    let addr = host.addr;
    assert_eq!(app_stats(&host, "algo")["tier"], backend);
    // An endpoint to post webhooks to.
    let made = send_raw(
        addr,
        format!(
            "POST /webhooks/admin/endpoints HTTP/1.1\r\nHost: lab.test\r\nConnection: close\r\n\
             Authorization: Bearer {WEBHOOKS_SECRET}\r\nAccept: application/json\r\n\
             Content-Length: 9\r\n\r\nname=load"
        )
        .as_bytes(),
    );
    assert_eq!(made.status, 201, "{made:?}");
    let made: serde_json::Value = serde_json::from_str(&made.body).unwrap();
    let id = made["id"].as_str().unwrap().to_string();

    // Four clients, two workers: each asks for a heavy run again as soon as
    // it is answered — two for the simple matching algorithm on the large
    // graph, one for DPLL refuting 8 pigeons in 7 holes, one for the
    // annealing comparison.
    let stop = Arc::new(AtomicBool::new(false));
    let heavy: Vec<_> = (0..4)
        .map(|client| {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut answers = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    answers.push(match client {
                        1 => run_sat(addr, "pigeonhole 8 7", ""),
                        2 => get(addr, "/algo/anneal?example=cooling&part=result"),
                        _ => run_matching(addr, "random 2000 2000 12000 42", "augmenting"),
                    });
                }
                answers
            })
        })
        .collect();
    wait_until("heavy runs to hold both workers", || {
        count(&host, "algo", "in_flight") >= 2
    });
    let mut answered = 0;
    for turn in 0..10 {
        // Each request is made while the heavy runs are in flight.
        assert!(count(&host, "algo", "in_flight") >= 1);
        let hello = get(addr, "/hello/?name=Cove");
        assert_eq!(hello.status, 200, "{hello:?}");
        assert_eq!(hello.body, "Hello, Cove! (GET /)\n");
        let payload = format!("{{\"turn\":{turn}}}");
        let hook = send_raw(
            addr,
            format!(
                "POST /webhooks/in/{id}/push HTTP/1.1\r\nHost: lab.test\r\nConnection: close\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
                payload.len()
            )
            .as_bytes(),
        );
        assert_eq!(hook.status, 200, "{hook:?}");
        answered += 2;
    }
    stop.store(true, Ordering::Relaxed);
    let mut runs = 0;
    for (client, answers) in heavy.into_iter().enumerate() {
        // Every heavy answer is the right one: the same matching size, or
        // the same refutation in the same number of decisions.
        let mut seen = Vec::new();
        for answer in answers.join().unwrap() {
            assert_eq!(
                answer.status, 200,
                "a heavy run of client {client}: {answer:?}"
            );
            seen.push(match client {
                1 => {
                    assert_eq!(verdict(&answer), "unsat", "{answer:?}");
                    number_before(&answer.body, " decisions")
                }
                2 => {
                    assert!(answer.body.contains("data-checked=true"), "{answer:?}");
                    number_before(&answer.body, " moves taken")
                }
                _ => proved_size(&answer),
            });
        }
        assert!(seen.windows(2).all(|pair| pair[0] == pair[1]), "{seen:?}");
        runs += seen.len();
    }
    assert_eq!(answered, 20);
    assert!(runs >= 1);
    // The heavy runs were asked to yield and did.
    assert!(count(&host, "algo", "yields") > 0);
    assert_eq!(count(&host, "hello", "ok"), 10);
    assert_eq!(count(&host, "webhooks", "errors.runtime"), 0);
    // What the runtime could not yield: recorded, not asserted (see
    // `apps/algo/README.md`, "Yields on the native tier").
    let stats = app_stats(&host, "algo");
    eprintln!(
        "algo on {backend}: {runs} heavy runs, yields {}, yield requests {}, declined {}, overdue {}",
        stats["yields"], stats["yield_requests"], stats["yields_declined"], stats["overdue_yields"]
    );
}

#[test]
fn the_other_apps_answer_while_algo_computes_on_the_vm() {
    the_other_apps_answer_while_algo_computes("vm");
}

#[cfg(all(target_arch = "x86_64", unix))]
#[test]
fn the_other_apps_answer_while_algo_computes_on_the_native_tier() {
    the_other_apps_answer_while_algo_computes("native");
}

/// What keeps the heavy runs yielding on the native tier, held without a
/// clock: every function between the entry and the algorithms' loops has
/// machine code. One of them left on the encoded tier — a host call or a
/// lambda written in it is enough — puts the loops below an encoded frame,
/// where a compiled run cannot yield (ADR 0085), and the host's other apps
/// wait for the whole run (`apps/algo/README.md`, "Yields on the native
/// tier").
#[cfg(all(target_arch = "x86_64", unix))]
#[test]
fn the_heavy_path_has_machine_code_on_the_native_tier() {
    let apps = apps(&[sample("algo")]);
    let mut options = options(&apps, 1);
    options.backend = "native".parse().unwrap();
    let host = cove_host::Host::start(options).expect("the host starts");
    let detail = get(host.addr, "/_host/apps/algo");
    assert_eq!(detail.status, 200);
    let detail: serde_json::Value = serde_json::from_str(&detail.body).unwrap();
    let refused: Vec<&str> = detail["native"]["refusals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["function"].as_str().unwrap())
        .collect();
    for function in [
        "algo.handle",
        "algo.matchingPage",
        "algo.matchingResult",
        "algo.timed",
        "matching.parse",
        "matching.random",
        "matching.distinctSorted",
        "matching.build",
        "matching.hopcroftKarp",
        "matching.augmenting",
        "matching.certify",
        "algo.satPage",
        "algo.satResult",
        "sat.read",
        "sat.generate",
        "sat.random",
        "sat.pigeonhole",
        "sat.sudoku",
        "sat.solve",
        "sat.unsatisfied",
        "algo.annealPage",
        "algo.annealResult",
        "algo.timedRun",
        "anneal.points",
        "anneal.circle",
        "anneal.run",
        "anneal.exp",
        "anneal.ln",
        "anneal.tourLength",
        "anneal.nearestNeighbour",
    ] {
        assert!(
            !refused.contains(&function),
            "{function} is on the encoded tier: {refused:?}"
        );
    }
    // The one host call is a leaf of its own.
    assert!(refused.contains(&"algo.now"), "{refused:?}");
}
