# The algorithm playground

A Cove app on cove-host (issue #4). Each page takes an input — written, or
one of the examples, some generated from a seed — runs an algorithm on it in
the request's own isolate, and shows the answer, an independent check of
it, what the run cost and, when a limit stopped it, which limit. The
algorithms are Cove; the host supplies the clock the app times itself by,
the limits, and nothing else. It is also the CPU-heavy real app the host's
time slice is tested against: while it computes, the other apps on the host
keep answering.

| path (below `/algo`) | what |
| --- | --- |
| `GET /` | the algorithms |
| `GET /matching` | maximum bipartite matching: the form, with the first example |
| `GET /matching?example=<name>[&algorithm=…]` | the form filled with an example; the page's script runs it at once |
| `POST /matching` (`graph`, `algorithm`) | runs a graph; the page with the result (works without scripts) |
| `GET /sat`, `GET /sat?example=<name>`, `POST /sat` (`formula`, `budget`) | satisfiability, the same way |
| `…&part=result` (`GET` or `POST`) | the result alone, as HTML — what the page's script asks for |
| `GET /app.js` | the page's script |

`algorithm` is `hopcroft-karp` (the default), `augmenting` or `both`.

## Running it

```console
$ cargo build --profile checked
$ ./target/checked/cove-host serve --apps apps
$ open http://127.0.0.1:8080/algo/matching?example=seminars
$ curl -s 'http://127.0.0.1:8080/algo/matching?example=large&algorithm=both&part=result' \
    | grep -o 'Maximum: [0-9,]* pairs'
Maximum: 1,996 pairs
```

Every answer of a run carries the host's meter for that run in its headers
(`x-cove-run-fuel`, `-instructions`, `-yields`, `-yields-declined`,
`-parks`, `-worker-us`, `-wall-us`), and a run a limit stopped carries
`x-cove-stop` (`fuel`, `deadline`, `cancelled`, …):

```console
$ curl -s -o /dev/null -D - 'http://127.0.0.1:8080/algo/matching?example=heavy&algorithm=augmenting&part=result' \
    | grep -i -e '^HTTP' -e x-cove-stop -e x-cove-run-fuel
HTTP/1.1 500 Internal Server Error
x-cove-run-fuel: 400000479
x-cove-stop: fuel
```

## The page

The form posts, so the page works without scripts. With them, `app.js`
runs the form with `fetch` instead, and that is what gives the page its
three run-time features:

- **Cancel.** The button aborts the fetch; the browser closes the
  connection, and the host cancels the run at its next safepoint (it is
  counted as `errors.cancelled`). A heavy run can always be withdrawn.
- **Why it stopped.** A run the host stopped is answered with its status, the
  runtime's diagnostic and `x-cove-stop`; the page says which limit it was
  — *Stopped: fuel. The run used up its fuel budget (app.toml:
  limits.fuel)…*, or the deadline, the heap, the queue — and shows the
  diagnostic under it. A stop the app makes itself (an input it cannot read,
  or over its bounds) is answered in the result, with the line at fault.
- **What it cost.** The "host meter" line is filled from the `x-cove-run-*`
  headers: the fuel and instructions the runtime counted, how often the run
  yielded and declined to, its time on a worker and from admission to
  answer. An app cannot read its own meter, which is why it comes from the
  headers rather than the page. The algorithms' own measures — their time on
  the app's clock and their work in edge scans — are in the page itself.

Example links fill the form and let the script run it (`data-autorun`), so a
heavy example's stop reason is shown in the page rather than instead of it.
The pages carry a CSP that allows only the app's own script
(`script-src 'self'`), only same-origin fetches, inline styles and nothing
else; every name from the input is escaped wherever it lands.

## Maximum bipartite matching

### Input

```text
# a comment
a x              an edge from left vertex a to right vertex x
b: x y z         edges from b
left: a b c      the left vertices, in order (optional)
right: x y z     the right vertices, in order (optional)
random 40 40 120 7
                 a generated graph: left and right vertex counts, edges, seed
```

A vertex an edge names joins its side if it is not there yet, in order of
first appearance, so `left:`/`right:` are only needed for an isolated vertex
or an order. The two sides are separate namespaces (`left:` and `right:` are
keywords). A repeated edge counts once. `random` must be the only line that
is not a comment; it draws exactly that many distinct edges uniformly from
the `left × right` possible ones (`l1…`, `r1…`), with the playground's own
PRNG (below) — the same four numbers are the same graph on every machine
and both backends.

Bounds: at most **5,000 vertices a side and 50,000 edges** (the app's own,
answered in the page); a graph is drawn up to 40 vertices a side, and the
table view lists the first 500 pairs.

### What runs

- **Hopcroft–Karp**, O(E √V): rounds ("phases") of one breadth-first search
  that layers the graph from the free left vertices, then depth-first
  searches that augment along vertex-disjoint shortest paths in the layers.
- **Simple augmenting paths** (Kuhn's algorithm), O(V E): one search per left
  vertex, no greedy start. It is there to compare against, and it is the
  heavy one: on `heavy` it runs out of fuel where Hopcroft–Karp needs a
  fraction of it.
- **`both`** runs the two on the same graph and checks that they agree.

Both searches are iterative with an explicit stack, so a long augmenting
path is not a deep Cove call stack. The work is counted the same way in
both — adjacency entries examined, "edge scans" — beside the time on the
app's clock.

**The check does not trust the algorithm.** `matching.certify` takes the
answer and verifies every pair is an edge and no vertex is used twice; then
it builds **König's vertex cover** — from the free left vertices, follow
unmatched edges right and matched edges back, and take the left vertices
*not* reached and the right vertices reached — and checks, edge by edge, that
every edge has an end in it and that it has as many vertices as the
matching has pairs. A cover of size *k* means no matching is larger than
*k*, so a matching and a cover of equal size prove each other optimal. The
page says "Maximum: N pairs" only when that check passes; the drawing marks
the cover with squares.

### The picture

The ledger's chart rules (`apps/ledger/chart`): fixed colours (the first
categorical colour for the matching, the second for the cover), text in
neutral ink, a legend, and nothing said by colour alone — matched edges are
thick and blue against thin grey edges, matched vertices are filled and
unmatched ones hollow, cover vertices are squares. Every edge and vertex has
a tooltip. The table view under it lists the pairs, the unmatched vertices
and the cover.

### Examples

| example | graph | maximum |
| --- | --- | ---: |
| `jobs` | five people, five jobs | 5 (perfect) |
| `seminars` | three students who all want the same two seminars, and a fourth: Hall's condition fails for the three | 3 of 4; the cover is {logic, algebra, s4} |
| `greedy` | `a x`, `a y`, `b x`: the greedy first choice has to be undone | 2 |
| `random` | `random 14 14 34 7` (drawn) | 12 |
| `large` | `random 2000 2000 12000 42` | 1,996 |
| `heavy` | `random 5000 5000 50000 1`, with `both`: the simple algorithm runs out of fuel | — (stopped: fuel) |

## Satisfiability (SAT)

### Input

DIMACS CNF, or one generator line:

```text
c a comment
p cnf 3 2            variables and clauses (optional)
1 -3 0               a clause: literals, ended by 0
2 3 -1 0
random 3 60 256 7    random k-SAT: k, variables, clauses, seed
pigeonhole 6 5       pigeons, holes
sudoku 53..7....6..195....98....6.8...6...34..8.3..17...2...6.6....28....419..5....8..79
```

A clause may span lines and the last may omit its `0`; `%` ends the input
(as some benchmark files do). A repeated literal counts once and a clause
holding a literal and its negation is dropped (the page notes it when the
`p` line's count then differs). An empty clause is refused, as is a
variable above the `p` line's count. The reader scans bytes — no string is
compared — so on the native tier it has machine code (see below).

Generators:

- **`random k n m seed`**: `m` clauses of `k` distinct variables, each
  negated with probability ½, from the playground's PRNG. Near 4.26 clauses
  a variable, random 3-SAT is about as likely satisfiable as not: of
  `random 3 60 256 s`, seed 1 is satisfiable and seeds 2 and 3 are not.
- **`pigeonhole p h`**: every pigeon in a hole, no two pigeons in one
  (`p · h` variables, `p + h·p(p−1)/2` clauses). Unsatisfiable exactly when
  `p > h`, and exponentially hard for DPLL (it is resolution, and the
  principle has no short resolution proof).
- **`sudoku <81 cells>`**: variable `81r + 9c + d` is "cell (r, c) holds
  d + 1"; each cell exactly one digit, each row, column and box each digit
  exactly once (11,988 clauses), each given a unit clause.

Bounds: **5,000 variables, 50,000 clauses, 200,000 literals.**

### What runs

DPLL (`sat.solve`): **unit propagation over two watched literals** per
clause (a linked list of watch slots per literal, so a watch moves in
O(1)); **pure literals** assigned once, at the root; a **static branching
order** — the variable in the most clauses first, its more frequent sign
first, by a counting sort; **chronological backtracking** and no clause
learning. The page reports decisions, conflicts, propagations, the
deepest decision level and the pure literals.

A **decision budget** (the form's field, default 1,000,000) is the app's
own stop: past it the answer is *Unknown: the search gave up*, said in the
page. The host's fuel and deadline stop it otherwise — `heavy` (10 pigeons,
9 holes) runs out of fuel, and the page says *Stopped: fuel*.

**The check**: a satisfying assignment is evaluated against every clause
(`sat.unsatisfied`) before the page says "Satisfiable". An unsatisfiable
answer has no certificate here (DPLL without learning keeps no proof); it
is checked in the tests instead, on formulas whose answer is known.

### The picture

A satisfying assignment is drawn as a grid of squares, variable 1 top left,
forty a row: filled in the first categorical colour for true, hollow for
false (so the two differ by more than colour), each with a tooltip; up to
1,000 variables. A sudoku is drawn as its grid instead: the givens in bold
ink, the digits the solver found in the secondary ink, a legend for the
two. The table view is the DIMACS `v` line, and the formula as read (the
first 200 clauses).

### Examples

| example | formula | answer | search (native) |
| --- | --- | --- | --- |
| `tiny` | 3 variables, 2 clauses | satisfiable | 2 decisions |
| `contradiction` | all four clauses over two variables | unsatisfiable | 1 decision, 2 conflicts |
| `pigeonhole` | 6 pigeons, 5 holes | unsatisfiable | 374 decisions, 1.6 ms |
| `random` | `random 3 60 256 1` | satisfiable | 17 decisions |
| `sudoku` | the classic puzzle (30 givens) | satisfiable, one solution | 0 decisions: propagation alone solves it |
| `hard` | 8 pigeons, 7 holes | unsatisfiable | 32,780 decisions, ~170 ms |
| `heavy` | 10 pigeons, 9 holes | — | stopped: fuel |

## Limits

`app.toml`, per request:

| limit | value | why |
| --- | --- | --- |
| `fuel` | 400,000,000 | `large` by the simple algorithm takes about 125 M (both algorithms and the check, native tier), SAT's `hard` about 160 M; matching's and SAT's `heavy` do not fit, on purpose |
| `deadline` | 10 s | far above any run that fits its fuel, on the VM too; a run parked or queued past it is stopped |
| `max_heap_words` | 2 Mi words (16 MiB) | a 5,000 × 5,000 graph with 50,000 edges, or a formula at its bounds, and their working arrays are well inside it |
| `max_in_flight` | 4 | at most four runs of this app at once, on any number of workers: a burst of heavy runs cannot take every worker of a larger host |
| `max_queued` | 32 | past it, 429 for this app only |
| `max_request_bytes` | 256 KiB | a written graph at the edge bound fits; a DIMACS file larger than that is over the literal bound anyway |
| `max_response_bytes` | 4 MiB | the largest page (500 table rows) is far inside it |
| `max_host_calls` | 64 | the clock is read four times a run |

The app's own bounds (vertices, edges; variables, clauses, literals; the
decision budget) are checked before anything runs and answered in the page.

## Reproducibility: the PRNG

`rng` is xorshift64 (Marsaglia 2003): one 64-bit word of state, three
shifts and xors, no multiplication — Cove's `*` stops the run on overflow,
and shifts and `bitXor` read an `Int` as a word. A seed is mixed with the
golden-ratio word and the generator is turned eight times before use. Its
first outputs from Marsaglia's state are checked against the published
sequence (`rng_test.cove`). The host's `random` is not used anywhere: it
cannot be replayed.

## Tests

```console
$ ./target/checked/cove-host test --apps apps algo     # the Cove test fns
$ cargo test --profile checked --test algo              # the app on a host
```

- `matching_test.cove`: the examples' known maxima, each by both algorithms
  and proved by the certificate; `seminars`' cover is exactly the one its
  comment names; on 60 small random graphs both agree with an exhaustive
  search; on larger ones they agree and are proved; a non-maximum matching
  and a matching with a non-edge are *not* proved; a seed is always the same
  graph with exactly the edges asked for; the input format and every refusal.
- `sat_test.cove`: small formulas with known answers (and the unique model
  of one); the pigeonhole principle for 1–5 holes, both ways; on 80 random
  3-SAT formulas (3–10 variables, 35 of them satisfiable) the solver agrees
  with trying all 2^n assignments, and every model it gives satisfies every
  clause; a seed is always the same formula, with distinct variables per
  clause; the sudoku's known first row, and a sudoku with two fives in a row
  unsatisfiable; the decision budget; every refusal, with the line.
- `rng_test.cove`: Marsaglia's sequence, seeds, ranges.
- `crates/cove-host/tests/algo.rs`: the known answers through the page,
  for all three algorithm choices, with the proof; the meter headers; CSP,
  the script, escaping of hostile vertex names, refusals; a run out of fuel
  answered `x-cove-stop: fuel`, one past its deadline `deadline`; SAT's
  known answers through the page, its models re-checked in Rust against the
  formula as the page prints it, the sudoku's first row, the budget's
  *Unknown*, SAT's `heavy` stopped by fuel; a client
  that goes away cancels its heavy run (`errors.cancelled`, nothing in
  flight after); and **the responsiveness test** below, on the VM and on the
  native tier; and that every function on the heavy path has machine code.

## Responsiveness

**Tested** (`algo.rs::the_other_apps_answer_while_algo_computes_on_*`, no
duration asserted): a host with **two workers** and **four clients**, two
asking for the simple matching algorithm on `large` and two for SAT's `hard`
(DPLL refuting 8 pigeons in 7 holes), each again as soon as it is answered;
while at least one heavy run is in flight, ten `hello` requests and ten
webhooks posted to the webhook lab all complete, the heavy answers all agree,
and the heavy runs yielded (`yields > 0`). On the VM and on the native tier.

**Measured** (`sh bench/algo.sh 3 <backend>`, four workers, `hello` open
loop at 500 req/s, latency from the intended start; K heavy clients closed
loop on `example=large&algorithm=augmenting`):

2026-10-05, i7-10700K (8 cores, 16 threads), macOS 26.6.2, `--profile
checked`, Cove at 9272282, load average 3–7 with other work on the machine.
Medians of three repetitions, 5,000 `hello` requests each; raw output in
`bench/results/algo-2026-10-05-{native,vm}.txt`.

| tier | heavy clients | `hello` p50 | `hello` p99 | heavy runs answered in the 10 s | yields per heavy run | declined per run | overdue |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| native | 0 | 1.18 ms | 2.04 ms | — | — | — | — |
| native | 4 | 2.84 ms | **4.68 ms** | ~410 | 32 | ~290 | 0 |
| native | 8 | 3.18 ms | **4.95 ms** | ~410 | 34 | ~290 | 0 |
| VM | 0 | 1.21 ms | 2.00 ms | — | — | — | — |
| VM | 4 | 2.84 ms | **4.65 ms** | ~64 | 227 | 0 | 0 |
| VM | 8 | 2.66 ms | **4.40 ms** | ~66 | 220 | 0 | 0 |

The same with SAT's `hard` as the heavy run (`MIX=algo-sat HEAVY="0 4" sh
bench/algo.sh 3 <backend>`; `bench/results/algo-sat-2026-10-05-*.txt`):

| tier | heavy clients | `hello` p50 | `hello` p99 | heavy runs answered in the 10 s | yields per heavy run | declined per run | overdue |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| native | 0 | 1.24 ms | 2.06 ms | — | — | — | — |
| native | 4 | 2.70 ms | **4.52 ms** | ~250 | 57 | 0 | 0 |
| VM | 0 | 1.15 ms | 2.03 ms | — | — | — | — |
| VM | 4 | 2.65 ms | **4.45 ms** | ~48 | 302 | 0 | 0 |

With every worker held by a heavy run, a `hello` waits for the next yield —
at most one 2 ms slice plus a tick of the monitor — and its p99 goes from 2
to under 5 ms. Eight heavy clients are no worse than four: the app's
`max_in_flight = 4` keeps the other four queued in *its own* queue, where
they cost `hello` nothing. The heavy matching run is about 6.5 times faster on
the native tier (≈ 100 ms against ≈ 620 ms on a worker), the DPLL run about
5 times (≈ 180 ms against ≈ 940 ms). On the native tier the SAT runs decline
no yield at all: everything from the entry to the solver's loop has machine
code (the request is a `GET`, so no form is decoded).

```console
$ cargo build --profile checked
$ sh bench/algo.sh 3 native > bench/results/algo-$(date +%F)-native.txt
$ sh bench/algo.sh 3 vm > bench/results/algo-$(date +%F)-vm.txt
$ MIX=algo-sat HEAVY="0 4" sh bench/algo.sh 3 native > bench/results/algo-sat-$(date +%F)-native.txt
```

## Yields on the native tier

**A shape of Cove code that stops a compiled run from yielding**, found
here, measured, and worked around in the app. It is ADR 0085's remaining
limit — *a run below an encoded callee of compiled code cannot yield* — met
by an ordinary program, and it goes to Cove upstream.

**The shape.** The first version timed each algorithm the obvious way:

```cove
fn timed(key: String, graph: Graph) -> Timed {
  let started = time.nowMicros()
  let found = if key == "augmenting" { matching.augmenting(graph) } else { matching.hopcroftKarp(graph) }
  Timed(key: key, found: found, micros: time.nowMicros() - started)
}
```

The template code generator does not lower a host call (`CallHost`), so
`timed` — and `matchingResult`, which reads the clock the same way — stayed
on the encoded tier, while `handle` and `matchingPage` above them and
`augmenting` below them were compiled. Every heavy loop therefore ran below
an encoded frame that compiled code had called, where a yield request is
declined. `/_host/apps/algo` now says so directly:

```json
{"function": "algo.timed", "instruction": "CallHost", "reason": "an instruction is not lowered",
 "at": "algo/matching_page.cove:135:17", "source": "let started = time.nowMicros()"}
```

**The effect**, `bench/algo.sh` on the native tier with that version
(`bench/results/algo-2026-10-05-native-before-leaf-clock.txt`, one
repetition): with four heavy clients on four workers, **`hello`'s p99 was
10.2 s** (p50 5.0 s; 142 ms from the send — each `hello` waited for a whole
heavy run, and the open loop's queue compounded it), against 2.0 ms alone;
with eight, 76 s. The host's counters for those runs: 663 heavy runs, 625
yield requests, **624 `overdue_yields`** (asked and still holding the worker
20 ms later — every one), **72,205,050 `yields_declined`** (every safepoint
poll below the encoded frame), and the 623 yields that did happen were each
at the end of a run's encoded stretch. The in-process test showed the same:
2,547,643 declined and 22 overdue of 22 requests, against 0 and 0 on the
VM.

**The workaround**, in the app: the clock is read in a leaf of its own,
`now()` (`pages.cove`), so the only encoded frame is that one and it returns
at once; `timed` and `matchingResult` have machine code again. The same
limit applies to a function that writes a lambda (`FuncRef` is not lowered
either), so the graph generator's sort no longer uses `sorted(by:)`: a merge
sort of its own (`matching.distinctSorted`) keeps the generator compiled.
After: 0 overdue in the table above, and `hello`'s p99 4.7 ms.
`algo.rs::the_heavy_path_has_machine_code_on_the_native_tier` holds this
without a clock, by asserting that every function from `handle` to the
loops is compiled and that `algo.now` is the encoded one.

**What is still encoded**, from `/_host/apps/algo` (11 functions before the workaround, 9 after):

| function | instruction | why it is encoded | heavy? |
| --- | --- | --- | --- |
| `algo.now` | `CallHost` | the clock | no: a leaf, returns at once |
| `algo.drawMatching`, `algo.matchingTable` | `FuncRef` | they write a closure | no: at most 40 × 40 vertices / 500 rows, and no algorithm below them |
| `matching.Names.of` (and its two lambdas) | `FuncRef`, `Cmp` on `String` | sorts names with `sorted(by:)`, compares strings | short; a written graph at the bounds is tens of ms here, which declines |
| `matching.Names.indexOf`, `matching.wordsOf`, `text.parseForm` | `CmpBranch` on `String` | a `String` comparison is "an operand outside a bound" | short leaves |

The ~290 declined yields per heavy run that remain are those leaves (the
input's words, the form) being polled while they run: none is long enough
to be overdue.

**SAT met the third shape.** `sat.read` began `if generator != "" {` — and
a `String` comparison is "an operand outside a bound" to the code
generator, so the whole reader, the DIMACS byte scanner and the calls into
the generators, was left on the encoded tier below compiled `satResult`.
Comparing `byteLength()` instead (and `startsWith` for the generator's
name) gave all of `sat` machine code; `algo.rs` holds that too.

**A wrong answer, not only a held worker: a copy that is sliced.** The
responsiveness test failed once in a full suite run, and the message it
now prints said why: a heavy matching run on the native tier answered 500,

```text
error[cove::runtime]: `runCopy` writes 12000 element(s) to 0 of a destination of 0
   --> algo/matching/matching.cove:384:14
384 |   var from = values.toVector()
```

(and, in other runs, `… to 0 of a destination of 1`, `this run has no
memory left`, and once from `counts.snapshot()` in `matching.build`:
`writes 2001 element(s) to 0 of a destination of 4`). `Array.toVector` is
lowered as `Len`, `Alloc(store, len)`, `RunCopy`: the store that came back
from the allocation was not the size asked for. Under load — two workers,
eight clients on `example=large&algorithm=augmenting` — it was **2 runs of
400** (16,347 yields); with `--slice 0`, so that nothing yields, **0 of 800**;
on the VM, **0 of 150** (41,641 yields). So it is the native tier resuming a
sliced run, at or around an allocation with a length from a slot.

`bench/repro/` reproduces it without the playground: one app of 70 lines in
the generator's shape (draw into a vector, `freeze`, `toVector` in a callee,
merge-sort, repeat), and `sh bench/repro/run.sh [native|vm] [slice]` asks it
160 times, 16 at a time, on two workers: **13 of 160** failed on the native
tier with the 2 ms slice, **0 of 160** with `--slice 0`, 0 on the VM (and 28
of 160 with `toArray` in place of `freeze`, still always at `toVector`).

**The workaround**, in the app: the three copies on the generator's path
(`cells.toVector()` in `random`, the two in `distinctSorted`, and
`counts.snapshot()` in `build`) push their elements one by one instead
(`matching.copyOf`). After it: **0 of 600** matching runs (25,150 yields)
and **0 of 400** SAT runs (53,193 yields) failed under the same load. The
SAT solver makes no such copy.

**For upstream (myuon/cove)**: (1) a function that makes any host call is
left on the encoded tier by the template compiler, and so is any function
that writes a lambda or compares two `String`s; (2) compiled code below such a frame cannot yield
(ADR 0085), so an algorithm whose caller reads the clock — the natural way to
time it — holds its worker for its whole run, with nothing at the source
level to say so. Either lowering `CallHost`/`FuncRef` (a call to a runtime
helper), or letting a compiled callee of an encoded frame yield, removes the
trap; until then a host's `native.refusals` report is how an app finds it.
(3) **A sliced native run can resume with a wrong-sized allocation**
(`Array.toVector`, `Vector.snapshot`): an incorrect answer, not a slow one,
reproduced by `bench/repro/`. This is the one to fix first. (4) A compiled
loop whose body is one `toVector()` of a 12,000-element array and a length
check, 40,000 turns, was asked to yield 22 times and never did — 0 yields,
0 declined, 22 overdue: such a loop does not appear to reach a safepoint the
monitor's request is seen at.

## Cove gaps met while writing it

For upstream (myuon/cove), besides the native-tier shapes above:

- **No `Float.exp`/`ln`** in the standard library (`sqrt` is there).
- **An `if` in statement position must still have matching branch types**:
  `if a { x += 1 } else { v.set(i, 0) }` is refused because `set` answers an
  `Option`. Reordering the branch so it ends in a `Unit` statement is the
  workaround everywhere in this app.
- **An element type inferred only from later use can pass `check` and fail
  lowering** with an empty diagnostic: `var levels = Vector.of()` used by
  `pop()` before any `push` checked, then `cove-host check` reported "does
  not lower:" and nothing else (the test runner said "the type of this
  expression was never settled `_`" without a location). Writing
  `Vector<Int>` on the binding fixes it.
- **`freeze()` cannot see through a helper that returns a fresh vector**
  (`var a = filled(n, -1)` … `a.freeze()` is refused), so the algorithms
  answer with `toArray()`, an O(n) copy; and there is no
  `Vector.filled(n, value)` to make one.
