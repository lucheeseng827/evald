# evald — the hot-tier engine decision

> `PLAN.md` §6 named the hot tier **the single biggest storage risk** and refused to pick
> an engine without data: *"benchmark the LSM vs a plain segmented-WAL-of-Arrow-batches
> before committing — for an append-mostly, query-elsewhere workload we may not need an LSM
> at all."* `ROADMAP.md` carried the same line as a Beta gate: *"pick one with data."*
> `Cargo.toml` still says `HOT-TIER engine: DEFERRED ON PURPOSE`.
>
> **Status: decided 2026-09-16 — no LSM. The shipped segmented WAL plus in-memory index
> stays.** This document is the reasoning, kept so a future change to the engine has
> something to argue against. Every number below is labelled *measured* or *derived*, and
> every measurement is reproducible with `tests/hot_tier_bounds.rs`.
>
> With this closed, both GA storage gates and the engine question are answered, which is
> what `ee/THREE_YEAR_PLAN.md` calls the **OSS 1.0 candidate**.

---

## 0. The decision, in one line

**Keep the hot tier in memory, bounded and shed; do not put an LSM on the crash-durable
write path.** The three things an LSM would buy are each either already provided by the WAL
or priced far above what they solve, and adding one would mean a second on-disk format to
freeze in the same month `docs/FORMAT.md` froze the first.

---

## 1. What is actually shipped

Spans are fsynced to a segmented write-ahead log — **that fsync is the ACK** — and then held
in memory, grouped by WAL segment, until the compactor writes them to Parquet and advances
the watermark in one redb transaction. The in-memory tier is therefore a **read cache over
data that is already durable on disk**, not a second copy of record. On restart it is
rebuilt by replaying the segments above the watermark.

Two properties follow, and they are what the rest of this document turns on:

- **Its size is a window, not a total.** It holds one compaction interval's worth of ingest,
  not the store. `--compact-interval-secs` (default 5) sets that window directly.
- **The window is capped.** `--max-hot-spans` (default 300,000, sized by §4 rather than
  inherited) sheds ingest with `429 + Retry-After` rather than growing memory.
  `tests/hot_tier_bounds.rs` asserts this, including that the store resumes once the
  compactor drains the backlog.

## 2. What an LSM would buy, and whether that is a problem we have

An LSM hot tier (the candidate in `Cargo.toml` was `fjall`) offers three things over an
in-memory one. Taking them in turn, against measurement rather than intuition:

**(a) Spill to disk, so memory is not the bound.** Real, and the one with teeth — see §3,
where 1M un-compacted spans cost 1.8 GiB resident. But the data is *already on disk*: the
same 1M spans are 1,474 MiB of WAL at the same moment. An LSM would not save the write; it
would let evald avoid holding a redundant copy in RAM. The cheaper form of that same saving
is a shorter window, which is one flag and no new format.

**(b) Bounded recovery.** An LSM recovers by opening a file; evald replays the WAL. Measured
at 4.5 s for 1M spans (§3) — proportional, not unbounded, and at a realistic window
(§4) it is tens of milliseconds.

**(c) Indexed reads over the un-compacted set.** Real: the hot tier is scanned linearly, so
a trace lookup is O(hot). Measured at 40 ms against 1M spans, 3.8 ms against 100k. At a
realistic window it is sub-millisecond, and the moment spans reach Parquet they are behind
the redb trace index anyway.

Against those, what an LSM costs:

- **A second on-disk format**, three weeks after `docs/FORMAT.md` froze the first and
  committed to reading N-1 across releases. Two formats is two migration stories.
- **Write stalls on the ACK path** — `PLAN.md` §6's own second risk. An LSM backing up on
  L0→L1 compaction stalls writes, and with a serialized writer that propagates to the
  client. Today the ACK is one fsync with nothing behind it.
- **Maturity on the crash-durable path.** `PLAN.md` called a young LSM's on-disk format the
  single biggest storage risk. That judgement has not changed; what changed is that the
  alternative now has a soak gate and a frozen format behind it.
- **A worse failure mode.** The current bound fails *loudly and observably*: shedding shows
  up as `429`, `evald_ingest_shedding` and `evald_spans_shed_total`. Spilling to disk
  converts that into silent disk growth and a later, harder failure.

## 3. Measurements

Taken with `cargo test --release --test hot_tier_bounds -- --ignored --nocapture`, one size
per process (RSS across loop iterations is meaningless — the allocator does not return freed
pages, so a later larger run reports a *smaller* delta). Spans are LLM-shaped: an 800-byte
prompt and a 200-byte completion, model, provider, token counts, session and user ids,
payloads inline. Machine: 4 vCPU Intel Xeon @ 2.10 GHz, 16 GiB, Linux 6.18, container
overlay filesystem. All *measured*.

| un-compacted spans | RSS delta | bytes/span | span footprint | WAL on disk | replay on open | trace lookup | full hot scan |
|---|---|---|---|---|---|---|---|
| 100,000 | 188 MiB | 1,969 | 1,670 | 147 MiB | 0.38 s | 3.8 ms | 0.24 s |
| 500,000 | 927 MiB | 1,943 | 1,670 | 737 MiB | 2.03 s | 20.2 ms | 1.45 s |
| 1,000,000 | 1,849 MiB | 1,939 | 1,670 | 1,474 MiB | 4.50 s | 40.5 ms | 3.27 s |

"Span footprint" is the deterministic companion to RSS: the struct plus everything it owns,
counted field by field. The gap between it and RSS — about 16% — is allocator slack and the
runtime, and the fact that the two track each other is why the RSS column can be trusted.

Everything is linear in span count, which is the result that matters: there is no knee, no
degradation, and no surprise at the shed threshold. Ingest throughput is measured separately
in `docs/BENCHMARKS.md` (94,763 spans/s at 32 connections with 100-span batches,
fsync-before-ACK), and durability under sustained load in `docs/SOAK.md`.

## 4. The sizing consequence, which is the real finding

Memory is **ingest rate × compaction interval × bytes per span**, capped by
`--max-hot-spans`. At the shipped defaults and the payload measured above (*derived* from
the table):

| sustained ingest | hot tier after one 5 s interval | resident |
|---|---|---|
| 1,000 spans/s | 5,000 spans | ~10 MiB |
| 10,000 spans/s | 50,000 spans | ~93 MiB |
| 50,000 spans/s | 250,000 spans | ~460 MiB |

So at ordinary rates the hot tier is small and every cost in §3 is negligible. The hazard is
at the other end: **`--max-hot-spans 1000000` implies roughly 1.8 GiB resident at the shed
threshold** for 1 KiB spans. An operator who sets a 2 GiB container limit and leaves the
default gets an OOM kill *instead of* the backpressure the bound exists to provide — the
failure the design is supposed to prevent, arrived at through the setting meant to prevent
it.

So the default was wrong. Measuring this also turned up a defect worth fixing rather than
budgeting around: the read path collected every matching span and truncated afterwards, so a
bounded read of the hot tier transiently allocated a second copy of it — 1.6 GiB to return
100 rows. It now selects the newest rows as it scans: **1.26 s against 3.27 s** at a million
hot spans, and the copy is gone. What remains on that path is the hot/cold dedup set, which
keys every resident span — unavoidable, since deciding it per cold span by scanning the hot
tier would be quadratic. That key was a pair of cloned `String`s at ~0.3 KiB per span; it is
now the ids decoded back to their 24 bytes, held inline, at ~0.07 KiB per span including the
table. Exact, not hashed: a hash would be smaller still and would put a silent
wrong-answer — a span missing from a result — on the read path.

With that fixed, the bound was sized against the **whole process** at a full cycle — tier
filled, a bounded read, compaction (which clones a sealed segment and builds an Arrow batch
from it), a cold merge, and a SQL aggregate — rather than against the tier alone. Peak over
the process baseline, plus the ~114 MiB a real `serve` starts at
(`ee/docs/FLEET-BENCHMARKS.md`), against the **1 GiB** limit the shipped k3s and Helm
manifests set for a node:

| `--max-hot-spans` | spans resident | process peak | vs 1 GiB |
|---|---|---|---|
| 200,000 | 375 MiB | ~668 MiB | 65% |
| 250,000 | 467 MiB | ~760 MiB | 74% |
| **300,000** (default) | **560 MiB** | **~852 MiB** | **83%** |
| 350,000 | 652 MiB | ~945 MiB | 92% |
| 500,000 | 929 MiB | ~1,222 MiB | over |

**300,000** leaves that limit a margin of about 172 MiB for a query heavier than the
aggregate measured here, which ran over a single cycle's blocks. 250,000 is the more
conservative point at 74%; 350,000 reaches 92% and 500,000 exceeds the limit outright. The
original 1,000,000 was
~1.8 GiB in spans alone, before anything else ran: the OOM killer arrived before the bound
could shed, which is the failure the bound exists to prevent, and the manifests' own comment
claimed to "cover the hot tier riding an ingest burst" while it could not.

Note what scales with what. Spans and the dedup set scale with the bound, at ~2 KiB per
resident span together. Compaction's ~155 MiB scales with `--seal-threshold` instead, so a
deployment that is tight on memory has two knobs, not one. `docs/CONFIG.md` carries both.

Headroom is still ample: six sealed segments at the default 50,000-span seal threshold, and
about six times the steady-state window of the measured fleet deployment (17.5k spans/s over
a 3 s interval is ~52k spans resident). The trade is explicit — a store sustaining more than
~60k spans/s at the default 5 s interval now exceeds the bound in normal operation and must
raise it,
along with its memory limit. That is the right way round: the previous default did not shed
at that rate either, it just ran until the kernel intervened.

`docs/CONFIG.md` carries the flag, the arithmetic, and how to size both from each other.

## 5. What would reopen this

A decision is only closed if it says what would change it. Any of these, and this document
is wrong:

- **A read pattern over un-compacted spans that cannot tolerate a linear scan** — for
  example a live-tail UI filtering a busy store at the shed threshold. The cheap answer is
  an in-memory index over the hot tier, not an LSM.
- **A sustained rate where the smallest useful compaction interval still costs more memory
  than the deployment has.** At the default 5 s interval a 1 GiB budget is exhausted around
  100k spans/s — just past the fastest rate `docs/BENCHMARKS.md` has measured evald
  accepting. The first response is a shorter interval, and only if that runs out does
  spill-to-disk become the argument.
- **A requirement to keep un-compacted spans across a restart without a replay**, i.e. a
  recovery-time objective tighter than seconds at the shed bound.
- **The hot tier growing a second job** — indexing, retention, or serving something other
  than "the last few seconds of ingest". Then it is no longer an append-mostly window and
  the analysis above does not apply.

## 6. What this decision did not do

**No `fjall` hot tier was built and benchmarked head to head.** `PLAN.md` asked for that as
a gate *before committing* to an engine, and it is worth being exact about why the gate is
being closed without it rather than pretending it was run.

The comparison it was meant to settle was between two unknowns. It no longer is: one side
has shipped, and now has a soak gate holding sustained ingest across repeated `kill -9`
cycles (`docs/SOAK.md`), a frozen on-disk format (`docs/FORMAT.md`), published throughput
(`docs/BENCHMARKS.md`) and the table in §3. Building the other side would cost a
from-scratch storage engine spike, and §2 shows what it would have to beat: costs that are
already negligible at realistic windows, for a price that includes a second on-disk format
and a worse failure mode.

If a trigger in §5 fires, the benchmark is the right next step and this document is the
specification for it — the columns in §3 are exactly the ones a candidate has to beat.
