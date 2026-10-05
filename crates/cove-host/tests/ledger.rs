//! The bench ledger (`apps/ledger`, issue #3) on a host started in-process:
//! posting, validation, units, duplicates, persistence and escaping.

mod common;

use std::net::SocketAddr;

use common::*;
use serde_json::{json, Value as Json};

const SECRET: &str = "test-ledger-secret";

/// The sample's `app.toml` with the post secret given literally.
fn config() -> String {
    let shipped = std::fs::read_to_string(samples().join("ledger/app.toml")).unwrap();
    let config = shipped.replace(
        "post = { env = \"LEDGER_TOKEN\" }",
        &format!("post = {{ value = \"{SECRET}\" }}"),
    );
    assert!(
        config.contains(SECRET),
        "the shipped app.toml changed shape"
    );
    config
}

fn ledger() -> Apps {
    let config = config();
    apps(&[AppSpec {
        name: "ledger",
        from: samples().join("ledger"),
        config: Some(Box::leak(config.into_boxed_str())),
    }])
}

/// One of the converted real runs in `apps/ledger/samples`.
fn sample_run(id: &str) -> String {
    std::fs::read_to_string(samples().join(format!("ledger/samples/{id}.json"))).unwrap()
}

fn request(addr: SocketAddr, method: &str, path: &str, headers: &[&str], body: &str) -> Answer {
    let mut head =
        format!("{method} {path} HTTP/1.1\r\nHost: ledger.test\r\nConnection: close\r\n");
    for header in headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    send_raw(addr, head.as_bytes())
}

fn bearer() -> String {
    format!("Authorization: Bearer {SECRET}")
}

/// Posts `body` as a run with the secret.
fn post_run(addr: SocketAddr, body: &str) -> Answer {
    request(
        addr,
        "POST",
        "/ledger/api/runs",
        &[&bearer(), "Content-Type: application/json"],
        body,
    )
}

fn page(addr: SocketAddr, path: &str) -> Answer {
    let answer = request(addr, "GET", path, &[], "");
    assert_eq!(answer.status, 200, "{path}: {answer:?}");
    answer
}

fn stored(addr: SocketAddr, id: &str) -> Json {
    let answer = page(addr, &format!("/ledger/api/runs/{id}"));
    serde_json::from_str(&answer.body).unwrap()
}

/// A small valid run, to be changed by each test.
fn minimal(id: &str) -> Json {
    json!({
        "id": id,
        "repository": "myuon/cove",
        "commit": "abc1234",
        "measuredAt": "2026-10-05T03:13:54Z",
        "environment": {"cpu": "test cpu", "os": "test os"},
        "toolchain": {"rustc": "1.98.1"},
        "conditions": {"workers": "4"},
        "results": [{
            "case": "hello",
            "input": "c=64",
            "inputSize": 64,
            "backend": "vm",
            "load": [1.5, 2.0],
            "metrics": {
                "throughput": {"unit": "req/s", "values": [1000, 1100]},
                "p99": {"unit": "ms", "values": [2.5, 3.0]}
            }
        }]
    })
}

/// The `path`s of a 400's problems.
fn problem_paths(answer: &Answer) -> Vec<String> {
    assert_eq!(answer.status, 400, "{answer:?}");
    let body: Json = serde_json::from_str(&answer.body).unwrap();
    body["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["path"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn posting_needs_the_secret_and_reading_does_not() {
    let apps = ledger();
    let host = start(&apps, 1);
    let addr = host.addr;
    let body = minimal("r1").to_string();
    for header in [
        None,
        Some("Authorization: Bearer wrong".to_string()),
        Some("Authorization: Basic dTp3cm9uZw==".to_string()),
    ] {
        let mut headers = vec!["Content-Type: application/json"];
        if let Some(header) = &header {
            headers.push(header);
        }
        let refused = request(addr, "POST", "/ledger/api/runs", &headers, &body);
        assert_eq!(refused.status, 401, "{header:?}: {refused:?}");
        let deleted = request(addr, "DELETE", "/ledger/api/runs/r1", &headers, "");
        assert_eq!(deleted.status, 401);
    }
    assert_eq!(page(addr, "/ledger/api/runs").body.trim(), "[]");
    // Basic with the secret as the password, as a browser would send it.
    let basic = format!("Authorization: Basic {}", base64(&format!("ci:{SECRET}")));
    let posted = request(
        addr,
        "POST",
        "/ledger/api/runs",
        &[&basic, "Content-Type: application/json"],
        &body,
    );
    assert_eq!(posted.status, 201, "{posted:?}");
    // Reading is open.
    assert!(page(addr, "/ledger/").body.contains("r1"));
    assert!(page(addr, "/ledger/runs/r1").body.contains("abc1234"));
    // Not a JSON body, and from another site: refused even with the secret.
    let form = request(
        addr,
        "POST",
        "/ledger/api/runs",
        &[&bearer(), "Content-Type: text/plain"],
        &body,
    );
    assert_eq!(form.status, 415);
    for cross in ["Origin: https://evil.example", "Sec-Fetch-Site: cross-site"] {
        let forged = request(
            addr,
            "DELETE",
            "/ledger/api/runs/r1",
            &[&bearer(), cross],
            "",
        );
        assert_eq!(forged.status, 403, "{cross}");
    }
    let deleted = request(addr, "DELETE", "/ledger/api/runs/r1", &[&bearer()], "");
    assert_eq!(deleted.status, 200, "{deleted:?}");
    assert_eq!(request(addr, "GET", "/ledger/runs/r1", &[], "").status, 404);
}

#[test]
fn a_valid_run_is_stored_as_sent_and_in_canonical_units() {
    let apps = ledger();
    let host = start(&apps, 1);
    let addr = host.addr;
    let mut run = minimal("units");
    run["results"][0]["metrics"] = json!({
        "p50": {"unit": "us", "values": [850, 900, 875]},
        "p99": {"unit": "s", "values": [0.0025]},
        "memory": {"unit": "KiB", "values": [2048, 3072]},
        "cpu": {"unit": "ns", "values": [1500000]},
        "throughput": {"unit": "req/min", "values": [600]},
    });
    let posted = post_run(addr, &run.to_string());
    assert_eq!(posted.status, 201, "{posted:?}");
    let answer: Json = serde_json::from_str(&posted.body).unwrap();
    assert_eq!(answer["stored"], true);
    assert_eq!(answer["url"], "/ledger/runs/units");
    let back = stored(addr, "units");
    assert_eq!(back["repository"], "myuon/cove");
    assert_eq!(back["commit"], "abc1234");
    assert_eq!(back["measuredAt"], "2026-10-05T03:13:54.000Z");
    assert_eq!(back["environment"]["cpu"], "test cpu");
    let result = &back["results"][0];
    assert_eq!(result["case"], "hello");
    assert_eq!(result["input"], "c=64");
    assert_eq!(result["inputSize"].as_f64(), Some(64.0));
    assert_eq!(floats(&result["load"]), [1.5, 2.0]);
    let metrics = &result["metrics"];
    // What was sent is kept...
    assert_eq!(metrics["p50"]["unit"], "us");
    assert_eq!(floats(&metrics["p50"]["values"]), [850.0, 900.0, 875.0]);
    // ...beside the canonical unit of its dimension.
    for (name, unit, canonical) in [
        ("p50", "ms", vec![0.85, 0.9, 0.875]),
        ("p99", "ms", vec![2.5]),
        ("memory", "MiB", vec![2.0, 3.0]),
        ("cpu", "ms", vec![1.5]),
        ("throughput", "/s", vec![10.0]),
    ] {
        assert_eq!(metrics[name]["canonicalUnit"], unit, "{name}");
        assert_eq!(floats(&metrics[name]["canonical"]), canonical, "{name}");
    }
    assert_eq!(metrics["throughput"]["better"], "higher");
    assert_eq!(metrics["p99"]["better"], "lower");
    // The page shows the canonical values: the median of 0.85, 0.875, 0.9 ms.
    let html = page(addr, "/ledger/runs/units").body;
    assert!(
        html.contains(">0.875<br><span class=range>0.85–0.9</span>"),
        "{html}"
    );
    assert!(html.contains("sent in us: 850, 900, 875"), "{html}");
    // A metric keeps its dimension: `p99` in KiB is refused once it was a time.
    let mut sizes = minimal("sizes");
    sizes["results"][0]["metrics"]["p99"] = json!({"unit": "KiB", "values": [1]});
    assert_eq!(
        problem_paths(&post_run(addr, &sizes.to_string())),
        ["results[0].metrics.p99.unit"]
    );
}

#[test]
fn an_invalid_run_is_refused_with_every_reason() {
    let apps = ledger();
    let host = start(&apps, 1);
    let addr = host.addr;
    let not_json = post_run(addr, "{\"id\": ");
    assert_eq!(problem_paths(&not_json), [""]);
    assert!(not_json.body.contains("not JSON"), "{not_json:?}");
    assert_eq!(problem_paths(&post_run(addr, "[]")), [""]);
    assert_eq!(
        problem_paths(&post_run(addr, "{\"id\": \"a\", \"id\": \"b\"}")),
        [""]
    );
    let mut missing = problem_paths(&post_run(addr, "{}"));
    missing.sort();
    assert_eq!(
        missing,
        [
            "commit",
            "environment",
            "id",
            "measuredAt",
            "repository",
            "results"
        ]
    );
    // One rule broken per case: (what is changed, the path that names it).
    let cases: Vec<(Json, &str)> = vec![
        (json!({"id": "has space"}), "id"),
        (json!({"id": "x/../y"}), "id"),
        (json!({"commit": ""}), "commit"),
        (json!({"measuredAt": "2026-10-05 03:13"}), "measuredAt"),
        (json!({"measuredAt": "2026-10-05T03:13:54"}), "measuredAt"),
        (
            json!({"measured_at": "2026-10-05T03:13:54Z"}),
            "measured_at",
        ),
        (json!({"environment": {}}), "environment"),
        (json!({"environment": {"cores": 8}}), "environment.cores"),
        (json!({"schema": 2}), "schema"),
        (json!({"results": []}), "results"),
        (json!({"results": [1]}), "results[0]"),
    ];
    for (change, path) in cases {
        let mut run = minimal("bad");
        for (key, value) in change.as_object().unwrap() {
            run[key] = value.clone();
        }
        let answer = post_run(addr, &run.to_string());
        assert!(
            problem_paths(&answer).contains(&path.to_string()),
            "{change}: {answer:?}"
        );
    }
    let metric_cases: Vec<(Json, &str)> = vec![
        (
            json!({"p99": {"unit": "mss", "values": [1]}}),
            "results[0].metrics.p99.unit",
        ),
        (
            json!({"p99": {"values": [1]}}),
            "results[0].metrics.p99.unit",
        ),
        (
            json!({"p99": {"unit": "ms"}}),
            "results[0].metrics.p99.values",
        ),
        (
            json!({"p99": {"unit": "ms", "values": []}}),
            "results[0].metrics.p99.values",
        ),
        (
            json!({"p99": {"unit": "ms", "values": [1, null]}}),
            "results[0].metrics.p99.values[1]",
        ),
        (
            json!({"p99": {"unit": "ms", "values": [-1]}}),
            "results[0].metrics.p99.values[0]",
        ),
        (
            json!({"p99": {"unit": "ms", "values": ["1"]}}),
            "results[0].metrics.p99.values[0]",
        ),
        (json!({"p99": null}), "results[0].metrics.p99"),
        (json!({"p99": 1.5}), "results[0].metrics.p99"),
        (
            json!({"9lives": {"unit": "ms", "values": [1]}}),
            "results[0].metrics.9lives",
        ),
        (
            json!({"p99": {"unit": "ms", "values": [1], "better": "up"}}),
            "results[0].metrics.p99.better",
        ),
        (
            json!({"p99": {"unit": "ms", "values": [1], "value": 1}}),
            "results[0].metrics.p99.value",
        ),
        (json!({}), "results[0].metrics"),
    ];
    for (metrics, path) in metric_cases {
        let mut run = minimal("bad");
        run["results"][0]["metrics"] = metrics.clone();
        let answer = post_run(addr, &run.to_string());
        assert!(
            problem_paths(&answer).contains(&path.to_string()),
            "{metrics}: {answer:?}"
        );
    }
    // Two results for one case, input and backend.
    let mut twice = minimal("bad");
    let first = twice["results"][0].clone();
    twice["results"] = json!([first.clone(), first]);
    assert_eq!(
        problem_paths(&post_run(addr, &twice.to_string())),
        ["results[1]"]
    );
    // Every problem is reported at once, each with a message.
    let mut many = minimal("bad id");
    many["commit"] = json!(7);
    many["results"][0]["metrics"]["p99"]["unit"] = json!("parsecs");
    let answer = post_run(addr, &many.to_string());
    assert_eq!(problem_paths(&answer).len(), 3, "{answer:?}");
    assert!(
        answer.body.contains("`parsecs` is not a unit"),
        "{answer:?}"
    );
    // Nothing of any of it was stored.
    assert_eq!(page(addr, "/ledger/api/runs").body.trim(), "[]");
}

#[test]
fn reposting_a_run_is_idempotent_and_a_conflict_is_refused() {
    let apps = ledger();
    let host = start(&apps, 1);
    let addr = host.addr;
    let run = minimal("again");
    assert_eq!(post_run(addr, &run.to_string()).status, 201);
    // The same content — even written differently — is a duplicate.
    let pretty = serde_json::to_string_pretty(&run).unwrap();
    let repeat = post_run(addr, &pretty);
    assert_eq!(repeat.status, 200, "{repeat:?}");
    let answer: Json = serde_json::from_str(&repeat.body).unwrap();
    assert_eq!(answer["duplicate"], true);
    assert_eq!(answer["stored"], false);
    let listed: Json = serde_json::from_str(&page(addr, "/ledger/api/runs").body).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    // Different content under the same ID is an explicit error.
    let mut other = run.clone();
    other["results"][0]["metrics"]["p99"]["values"] = json!([2.5, 3.5]);
    let conflict = post_run(addr, &other.to_string());
    assert_eq!(conflict.status, 409, "{conflict:?}");
    assert!(
        conflict.body.contains("results[0]") && conflict.body.contains("differs"),
        "{conflict:?}"
    );
    let mut moved = run.clone();
    moved["commit"] = json!("def5678");
    let conflict = post_run(addr, &moved.to_string());
    assert_eq!(conflict.status, 409);
    assert!(conflict
        .body
        .contains("`commit` is `abc1234`, not `def5678`"));
    // The stored run is untouched.
    assert_eq!(
        floats(&stored(addr, "again")["results"][0]["metrics"]["p99"]["values"]),
        [2.5, 3.0]
    );
    // Deleted, the other content can be posted.
    assert_eq!(
        request(addr, "DELETE", "/ledger/api/runs/again", &[&bearer()], "").status,
        200
    );
    assert_eq!(post_run(addr, &other.to_string()).status, 201);
}

#[test]
fn an_unmeasured_metric_is_absent_not_zero() {
    let apps = ledger();
    let host = start(&apps, 1);
    let addr = host.addr;
    let mut run = minimal("gaps");
    run["results"] = json!([
        {"case": "a", "backend": "vm", "metrics": {
            "p99": {"unit": "ms", "values": [0]},
            "throughput": {"unit": "req/s", "values": [10]}}},
        {"case": "b", "backend": "vm", "metrics": {
            "throughput": {"unit": "req/s", "values": [20]}}}
    ]);
    assert_eq!(post_run(addr, &run.to_string()).status, 201);
    let back = stored(addr, "gaps");
    assert!(back["results"][1]["metrics"].get("p99").is_none());
    assert_eq!(
        floats(&back["results"][0]["metrics"]["p99"]["values"]),
        [0.0]
    );
    let html = page(addr, "/ledger/runs/gaps").body;
    // `a` measured a p99 of zero; `b` did not measure one.
    let row_a = html
        .split("title=\"the trend over runs\">a</a>")
        .nth(1)
        .unwrap();
    let row_b = html
        .split("title=\"the trend over runs\">b</a>")
        .nth(1)
        .unwrap();
    assert!(
        row_a.split("</tr>").next().unwrap().contains(">0</td>"),
        "{html}"
    );
    let row_b = row_b.split("</tr>").next().unwrap();
    assert!(row_b.contains("title=\"not measured\">–</td>"), "{row_b}");
    assert!(!row_b.contains(">0</td>"), "{row_b}");
}

#[test]
fn real_runs_survive_a_restart() {
    let apps = ledger();
    let ids = [
        "cove-589-capacity",
        "cove-593-capacity",
        "cove-host-perf-2026-10-05",
    ];
    {
        let host = start(&apps, 2);
        for id in ids {
            let posted = post_run(host.addr, &sample_run(id));
            assert_eq!(posted.status, 201, "{id}: {posted:?}");
        }
    }
    let host = start(&apps, 1);
    let addr = host.addr;
    let listed: Json = serde_json::from_str(&page(addr, "/ledger/api/runs").body).unwrap();
    let listed: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    // Newest measured first.
    assert_eq!(
        listed,
        [
            "cove-host-perf-2026-10-05",
            "cove-593-capacity",
            "cove-589-capacity"
        ]
    );
    let html = page(addr, "/ledger/").body;
    for id in ids {
        assert!(html.contains(&format!("/ledger/runs/{id}\"")), "{id}");
    }
    // #593's native crunch at 16 in flight: 2,770, 2,783.x and 2,784 req/s.
    let run = stored(addr, "cove-593-capacity");
    assert_eq!(run["commit"], "11ce12f");
    let native = run["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["case"] == "crunch" && r["input"] == "c=16" && r["backend"] == "native")
        .unwrap();
    assert_eq!(
        native["metrics"]["throughput"]["values"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(page(addr, "/ledger/runs/cove-593-capacity")
        .body
        .contains("2,783"));
    // And they compare after the restart: #589's VM against #593's.
    let compared = comparison(addr, "a=cove-589-capacity&b=cove-593-capacity");
    let crunch = row(&compared, "crunch", "c=16", "vm");
    assert_eq!(crunch["comparable"], "ok", "{crunch}");
    let throughput = metric(crunch, "throughput");
    assert!(
        throughput["ratio"].as_f64().unwrap().abs() < 0.2,
        "{throughput}"
    );
    assert!(page(
        addr,
        "/ledger/compare?a=cove-589-capacity&b=cove-593-capacity"
    )
    .body
    .contains("Largest differences"));
    // And the trend of a case both measured: two runs, a point each.
    let trend = page(addr, "/ledger/cases/trend?case=crunch&input=c%3D16").body;
    for id in ["cove-589-capacity", "cove-593-capacity"] {
        assert!(
            trend.contains(&format!("data-series=\"vm\" data-run=\"{id}\"")),
            "{id}"
        );
    }
    // Reposting after the restart is still a duplicate.
    let again = post_run(addr, &sample_run("cove-589-capacity"));
    assert_eq!(again.status, 200, "{again:?}");
}

#[test]
fn everything_a_poster_controls_is_escaped() {
    let apps = ledger();
    let host = start(&apps, 1);
    let addr = host.addr;
    let evil = "<script>alert(1)</script>";
    let img = "<img src=x onerror=alert(2)>";
    let mut run = minimal("escape");
    run["repository"] = json!(evil);
    run["commit"] = json!(img);
    run["note"] = json!(evil);
    run["source"] = json!(img);
    run["environment"] = json!({ evil: img, "cpu": evil });
    run["toolchain"] = json!({ "rustc": evil });
    run["conditions"] = json!({ img: evil });
    run["results"][0]["case"] = json!(evil);
    run["results"][0]["input"] = json!(img);
    run["results"][0]["backend"] = json!("\"><b>x</b>");
    let posted = post_run(addr, &run.to_string());
    assert_eq!(posted.status, 201, "{posted:?}");
    assert_eq!(post_run(addr, &minimal("plain").to_string()).status, 201);
    for path in [
        "/ledger/",
        "/ledger/runs/escape",
        "/ledger/compare?a=escape&b=plain&all=1",
        "/ledger/compare?a=plain&b=escape&all=1",
        "/ledger/cases",
        "/ledger/cases/trend?case=%3Cscript%3Ealert(1)%3C%2Fscript%3E&input=%3Cimg%20src%3Dx%20onerror%3Dalert(2)%3E",
    ] {
        let answer = page(addr, path);
        assert!(!answer.body.contains("<script>"), "{path}");
        assert!(!answer.body.contains("<img"), "{path}");
        assert!(!answer.body.contains("<b>x"), "{path}");
        assert!(answer.body.contains("&lt;script&gt;"), "{path}");
        assert!(
            answer
                .header("content-security-policy")
                .unwrap()
                .contains("default-src 'none'"),
            "{path}"
        );
    }
}

/// `GET /ledger/compare?<query>&format=json`.
fn comparison(addr: SocketAddr, query: &str) -> Json {
    let answer = page(addr, &format!("/ledger/compare?{query}&format=json"));
    serde_json::from_str(&answer.body).unwrap()
}

/// The row of a comparison for one case, input and backend.
fn row<'a>(comparison: &'a Json, case: &str, input: &str, backend: &str) -> &'a Json {
    comparison["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["case"] == case && r["input"] == input && r["backend"] == backend)
        .unwrap_or_else(|| panic!("no row {case} {input} {backend}: {comparison}"))
}

/// One metric of a comparison's row.
fn metric<'a>(row: &'a Json, name: &str) -> &'a Json {
    row["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name)
        .unwrap_or_else(|| panic!("no metric {name}: {row}"))
}

/// A run of one result, `hello` on `vm`, with `p99` as `values` (in ms),
/// measured at `at`, on `cpu`, under `workers`, with `rustc` and `load`.
fn variant(id: &str, commit: &str, at: &str, change: impl FnOnce(&mut Json)) -> String {
    let mut run = minimal(id);
    run["commit"] = json!(commit);
    run["measuredAt"] = json!(at);
    change(&mut run);
    run.to_string()
}

#[test]
fn a_comparison_marks_measurements_that_are_not_comparable() {
    let apps = ledger();
    let host = start(&apps, 1);
    let addr = host.addr;
    let runs = [
        variant("base", "aaa1111", "2026-10-01T00:00:00Z", |_| {}),
        // The same machine and conditions, a slower p99.
        variant("same", "bbb2222", "2026-10-02T00:00:00Z", |r| {
            r["results"][0]["metrics"]["p99"]["values"] = json!([4.0, 4.5]);
        }),
        variant("elsewhere", "ccc3333", "2026-10-03T00:00:00Z", |r| {
            r["environment"]["cpu"] = json!("another cpu");
            r["results"][0]["metrics"]["p99"]["values"] = json!([40.0]);
        }),
        variant("otherwise", "ddd4444", "2026-10-04T00:00:00Z", |r| {
            r["results"][0]["conditions"] = json!({"workers": "8"});
        }),
        variant("newer", "eee5555", "2026-10-05T00:00:00Z", |r| {
            r["toolchain"]["rustc"] = json!("1.99.0");
        }),
        variant("busy", "fff6666", "2026-10-06T00:00:00Z", |r| {
            r["results"][0]["load"] = json!([7.5, 8.0]);
        }),
    ];
    for run in &runs {
        assert_eq!(post_run(addr, run).status, 201);
    }
    let same = comparison(addr, "a=base&b=same");
    let hello = row(&same, "hello", "c=64", "vm");
    assert_eq!(hello["comparable"], "ok");
    let p99 = metric(hello, "p99");
    // Medians 2.75 and 4.25 ms: +1.5 ms, +54.5%, the ranges apart.
    assert_eq!(p99["delta"].as_f64(), Some(1.5));
    assert!((p99["ratio"].as_f64().unwrap() - 1.5 / 2.75).abs() < 1e-9);
    assert_eq!(p99["overlap"], false);
    assert_eq!(same["largest"][0]["metric"], "p99");
    let html = page(addr, "/ledger/compare?a=base&b=same").body;
    assert!(html.contains("+54.5% ▼"), "{html}");
    for (other, level, reason) in [
        (
            "elsewhere",
            "no",
            "environment: cpu `test cpu` vs `another cpu`",
        ),
        ("otherwise", "no", "conditions: workers `4` vs `8`"),
        ("newer", "warn", "toolchain: rustc `1.98.1` vs `1.99.0`"),
        ("busy", "warn", "load average 1.8 vs 7.8"),
    ] {
        let compared = comparison(addr, &format!("a=base&b={other}"));
        let hello = row(&compared, "hello", "c=64", "vm");
        assert_eq!(hello["comparable"], level, "{other}: {hello}");
        assert!(
            hello["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r == reason),
            "{other}: {hello}"
        );
        // A measurement that is not comparable has no place among the
        // largest differences; a warning does.
        let listed = !compared["largest"].as_array().unwrap().is_empty();
        assert_eq!(listed, level == "warn", "{other}: {compared}");
    }
    // On the page it is hidden unless asked for, and then marked.
    let hidden = page(addr, "/ledger/compare?a=base&b=elsewhere").body;
    assert!(hidden.contains("1 not comparable (hidden"), "{hidden}");
    assert!(!hidden.contains("another cpu"), "{hidden}");
    let shown = page(addr, "/ledger/compare?a=base&b=elsewhere&all=1").body;
    assert!(
        shown.contains("not comparable: environment: cpu"),
        "{shown}"
    );
    // The runs a comparison with `base` means something against are offered.
    let offered = page(addr, "/ledger/compare?a=base").body;
    for (id, listed) in [
        ("same", true),
        ("newer", true),
        ("busy", true),
        ("elsewhere", false),
        ("otherwise", true),
    ] {
        assert_eq!(
            offered.contains(&format!("b={id}\"")),
            listed,
            "{id}: {offered}"
        );
    }
    // A side that does not exist.
    assert_eq!(
        request(addr, "GET", "/ledger/compare?a=base&b=nothing", &[], "").status,
        404
    );
}

#[test]
fn a_metric_one_side_did_not_measure_is_not_compared_with_zero() {
    let apps = ledger();
    let host = start(&apps, 1);
    let addr = host.addr;
    let a = variant("a", "aaa1111", "2026-10-01T00:00:00Z", |r| {
        r["results"][0]["metrics"]["errors"] = json!({"unit": "count", "values": [0]});
    });
    let b = variant("b", "bbb2222", "2026-10-02T00:00:00Z", |r| {
        let metrics = r["results"][0]["metrics"].as_object_mut().unwrap();
        metrics.remove("p99");
        metrics.insert("errors".into(), json!({"unit": "count", "values": [3]}));
    });
    assert_eq!(post_run(addr, &a).status, 201);
    assert_eq!(post_run(addr, &b).status, 201);
    let compared = comparison(addr, "a=a&b=b");
    let hello = row(&compared, "hello", "c=64", "vm");
    let p99 = metric(hello, "p99");
    assert!(
        p99["b"].is_null() && p99["delta"].is_null() && p99["ratio"].is_null(),
        "{p99}"
    );
    // A measured zero is compared — 0 to 3 is +3 — but has no ratio.
    let errors = metric(hello, "errors");
    assert_eq!(errors["delta"].as_f64(), Some(3.0));
    assert!(errors["ratio"].is_null(), "{errors}");
    let html = page(addr, "/ledger/compare?a=a&b=b").body;
    assert!(html.contains("not measured in B"), "{html}");
}

#[test]
fn a_commit_is_compared_by_all_its_runs() {
    let apps = ledger();
    let host = start(&apps, 1);
    let addr = host.addr;
    let one = |id: &str, commit: &str, at: &str, case: &str, p99: f64| {
        variant(id, commit, at, |r| {
            r["results"][0]["case"] = json!(case);
            r["results"][0]["metrics"]["p99"]["values"] = json!([p99]);
        })
    };
    for run in [
        one("x1", "1111111aaaa", "2026-10-01T00:00:00Z", "hello", 2.0),
        one("x2", "1111111aaaa", "2026-10-02T00:00:00Z", "crunch", 5.0),
        // Measured again later on the same commit: the later one counts.
        one("x3", "1111111aaaa", "2026-10-03T00:00:00Z", "hello", 3.0),
        one("y1", "2222222bbbb", "2026-10-04T00:00:00Z", "hello", 6.0),
    ] {
        assert_eq!(post_run(addr, &run).status, 201);
    }
    let compared = comparison(addr, "a=commit:1111111&b=commit:2222222bbbb");
    assert_eq!(compared["a"], "commit 1111111 (3 runs)");
    let hello = row(&compared, "hello", "c=64", "vm");
    assert_eq!(hello["a"], "x3");
    assert_eq!(hello["b"], "y1");
    assert_eq!(metric(hello, "p99")["delta"].as_f64(), Some(3.0));
    let crunch = row(&compared, "crunch", "c=64", "vm");
    assert_eq!(crunch["a"], "x2");
    assert!(crunch["b"].is_null());
    assert!(page(
        addr,
        "/ledger/compare?a=commit:1111111&b=commit:2222222bbbb"
    )
    .body
    .contains("Only in A (1)"));
}

#[test]
fn a_trend_draws_every_run_and_leaves_gaps() {
    let apps = ledger();
    let host = start(&apps, 1);
    let addr = host.addr;
    let both = |id: &str, at: &str, vm: [f64; 2], go: Option<[f64; 2]>| {
        variant(id, &format!("{id}0000"), at, |r| {
            let mut results = vec![json!({
                "case": "hello", "input": "c=64", "backend": "vm",
                "metrics": {"p99": {"unit": "ms", "values": vm}}
            })];
            if let Some(go) = go {
                results.push(json!({
                    "case": "hello", "input": "c=64", "backend": "go",
                    "metrics": {"p99": {"unit": "us", "values": [go[0] * 1000.0, go[1] * 1000.0]}}
                }));
            }
            results.push(json!({
                "case": "solo", "backend": "vm",
                "metrics": {"p99": {"unit": "ms", "values": [1.0]}}
            }));
            r["results"] = json!(results);
        })
    };
    for run in [
        both("r1", "2026-10-01T00:00:00Z", [2.0, 3.0], Some([1.0, 1.5])),
        // Go was not measured in r2.
        both("r2", "2026-10-02T00:00:00Z", [4.0, 5.0], None),
        both("r3", "2026-10-03T00:00:00Z", [3.0, 3.0], Some([1.0, 2.0])),
    ] {
        assert_eq!(post_run(addr, &run).status, 201);
    }
    let cases = page(addr, "/ledger/cases").body;
    assert!(
        cases.contains("/ledger/cases/trend?case=hello&amp;input=c%3D64"),
        "{cases}"
    );
    let data: Json = serde_json::from_str(
        &page(
            addr,
            "/ledger/cases/trend?case=hello&input=c%3D64&format=json",
        )
        .body,
    )
    .unwrap();
    assert_eq!(data["runs"], json!(["r1", "r2", "r3"]));
    let p99 = &data["charts"][0];
    assert_eq!(p99["metric"], "p99");
    assert_eq!(p99["unit"], "ms");
    let series = p99["series"].as_array().unwrap();
    assert_eq!(series[0]["backend"], "go");
    assert_eq!(series[1]["backend"], "vm");
    // Go's microseconds are on the same millisecond axis, and r2 is a gap.
    let go = series[0]["points"].as_array().unwrap();
    assert_eq!(go[0]["median"].as_f64(), Some(1.25));
    assert!(go[1].is_null());
    assert_eq!(go[2]["least"].as_f64(), Some(1.0));
    let vm: Vec<f64> = series[1]["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["median"].as_f64().unwrap())
        .collect();
    assert_eq!(vm, [2.5, 4.5, 3.0]);

    let html = page(addr, "/ledger/cases/trend?case=hello&input=c%3D64").body;
    let svg = html
        .split("<svg")
        .nth(1)
        .unwrap()
        .split("</svg>")
        .next()
        .unwrap();
    for (series, run, median) in [
        ("vm", "r1", "2.5"),
        ("vm", "r2", "4.5"),
        ("vm", "r3", "3.0"),
        ("go", "r1", "1.25"),
        ("go", "r3", "1.5"),
    ] {
        assert!(
            svg.contains(&format!(
                "data-series=\"{series}\" data-run=\"{run}\" data-median=\"{median}\""
            )),
            "{series} {run}: {svg}"
        );
    }
    // The gap: no go point for r2, and go's two points are not joined.
    assert!(!svg.contains("class=point data-series=\"go\" data-run=\"r2\""));
    assert_eq!(svg.matches("class=line data-series=\"vm\"").count(), 1);
    assert_eq!(svg.matches("class=line data-series=\"go\"").count(), 0);
    // Whiskers from the least to the most; none where the two are equal.
    assert!(svg.contains(
        "class=whisker data-series=\"vm\" data-run=\"r1\" data-least=\"2.0\" data-most=\"3.0\""
    ));
    assert!(!svg.contains("class=whisker data-series=\"vm\" data-run=\"r3\""));
    // One axis, named with its unit; the legend for two series; colours in
    // the fixed order; tooltips; text in ink, not series colours.
    assert_eq!(svg.matches("rotate(-90)").count(), 1);
    assert!(svg.contains(">p99 (ms)</text>"));
    assert!(svg.contains("class=legend data-series=\"go\""));
    assert!(svg.contains("class=legend data-series=\"vm\""));
    assert!(svg.contains("class=line data-series=\"vm\" d=\"M"));
    assert!(
        svg.contains("stroke=\"#eb6834\" stroke-width=\"2\""),
        "vm is the second colour"
    );
    assert!(svg.contains("fill=\"#2a78d6\""), "go is the first");
    assert!(
        svg.contains("<title>vm · p99 2.5 ms (2–3, ×2) · r1 · r10000"),
        "{svg}"
    );
    for colour in ["#2a78d6", "#eb6834"] {
        assert!(!svg.contains(&format!("<text fill=\"{colour}\"")));
        assert!(
            !svg.contains(&format!("fill=\"{colour}\">")),
            "text in {colour}"
        );
    }
    // The table view says the same, with a dash for the gap.
    let table = html.split("<details class=table>").nth(1).unwrap();
    assert!(
        table.contains("<td>r2</td><td class=\"num absent\" title=\"not measured\">–</td>"),
        "{table}"
    );
    // One series: no legend.
    let solo = page(addr, "/ledger/cases/trend?case=solo&input=").body;
    assert!(
        solo.contains("<svg") && !solo.contains("class=legend"),
        "{solo}"
    );
    // A run deleted is a run gone from the trend.
    assert_eq!(
        request(addr, "DELETE", "/ledger/api/runs/r2", &[&bearer()], "").status,
        200
    );
    let data: Json = serde_json::from_str(
        &page(
            addr,
            "/ledger/cases/trend?case=hello&input=c%3D64&format=json",
        )
        .body,
    )
    .unwrap();
    assert_eq!(data["runs"], json!(["r1", "r3"]));
    assert_eq!(
        request(
            addr,
            "GET",
            "/ledger/cases/trend?case=nothing&input=",
            &[],
            ""
        )
        .status,
        404
    );
}

/// A JSON array of numbers, as numbers (`2` and `2.0` alike).
fn floats(value: &Json) -> Vec<f64> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("not an array: {value}"))
        .iter()
        .map(|n| n.as_f64().unwrap())
        .collect()
}

/// Standard base64, for a Basic login.
fn base64(text: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for at in 0..4 {
            if at <= chunk.len() {
                out.push(ALPHABET[(word >> (18 - 6 * at) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
