# Soak test — the durability gate

`PLAN.md` §6 names one gate before GA:

> **sustained ingest + kill-9 crash recovery + compaction under load, with
> fsync-correctness verification**

That is `tests/soak.rs`. This page says what it asserts, how to run it, how it was
verified to *fail*, and what the last run measured.

evald ACKs at the WAL fsync — a `200` means the span is on disk, not queued. Everything
below exists to test that one sentence under conditions that make it hard to keep true.

## Why a soak, when there is already a crash test

`tests/crash_recovery.rs` kills the server once, mid-ingest, and proves no ACK'd span is
lost. That catches a store that is wrong immediately.

It does not catch the failures that only appear after *several* crashes, which are the ones
that reach production:

- a recovery that loses a little each time, staying plausible for a long while;
- a compaction that stops committing once it has been interrupted, leaving an unbounded WAL
  behind a store that still answers every query correctly;
- a hot/cold dedup that starts double-counting only after both tiers have been rebuilt from
  a torn state a few times.

Each is invisible at one cycle and obvious at ten. The soak is the same claim as the crash
test, held long enough for those to show.

## What it asserts

Every cycle drives concurrent ingest against a live server, waits for compaction to commit
new Parquet blocks **while that load is running**, `SIGKILL`s the process mid-flight, and
reopens the store in a fresh process. Four properties are checked — 2, 3 and 4 after every
cycle, and 1 authoritatively once at the end, for the reason given below:

| # | Property | Assertion |
|---|---|---|
| 1 | **No ACK'd span is ever lost** | every span id handed a `200` is present after the final recovery — by **identity**, not by count |
| 2 | **No span is double-counted** | `COUNT(*) == COUNT(DISTINCT span_id)` |
| 3 | **Recovery never goes backwards** | rows after cycle *k* ≥ rows after cycle *k-1* |
| 4 | **Compaction keeps working under load** | each cycle commits ≥ 1 Parquet block that did not exist when the cycle began |

Property 1 is the fsync-correctness verification: the ACK floor is cumulative, so a span
ACK'd in cycle 1 must still be there after cycle 15.

Property 4 is deliberately *per cycle*, not "at least one block exists". A store that
quietly stops compacting after its first unclean shutdown passes 1–3 indefinitely while its
WAL grows without bound; only a per-cycle check catches it.

Recovery may legitimately return slightly **more** rows than were ACK'd — spans whose WAL
fsync completed just before the kill but whose `200` never made it back to the client. Every
run produces a handful.

**That surplus is why property 1 cannot be a count.** The obvious formulation, `recovered rows
≥ ACKs`, has a hole: if recovery loses an ACK'd row while one of those surplus rows remains,
the total is unchanged and the assertion passes — the gate reporting success with an
fsync-durable span missing, which is the exact thing it exists to deny. So the workers retain
the id of every span that got a `200`, and the check asks whether each of those ids is actually
in the store.

The cheap count form still runs after every cycle, because gross loss then fails in the cycle
that caused it rather than at the end. The identity check runs once, after the last cycle,
where it is equally strong — nothing ever re-adds a span, so an id lost in any cycle is still
absent at the end — and costs one scan instead of one per cycle.

Span ids come from one counter shared by every worker and every cycle, so no span is ever
written twice by the test itself. A duplicate in the store is the store's doing.

## Running it

Ignored by default, because `cargo test` should stay fast:

```bash
# the shape CI runs on every push (~45 s)
cargo test --test soak -- --ignored --nocapture

# the GA gate. EVALD_SOAK_SEAL matters here and is not optional: at the default 20 a
# run this long commits tens of thousands of blocks and stops on the EMFILE ceiling
# below, which is a real defect but not what these four properties measure.
EVALD_SOAK_SECS=1800 EVALD_SOAK_CYCLES=20 EVALD_SOAK_WORKERS=8 EVALD_SOAK_SEAL=1000 \
    cargo test --test soak -- --ignored --nocapture
```

| Variable | Default | Meaning |
|---|---|---|
| `EVALD_SOAK_SECS` | `45` | total wall-clock budget for the soak |
| `EVALD_SOAK_CYCLES` | `3` | kill -9 / recover cycles; the budget is split across them |
| `EVALD_SOAK_WORKERS` | `4` | concurrent ingest connections |
| `EVALD_SOAK_SEAL` | `20` | spans per sealed segment. Tiny by default so a 45 s run still commits blocks continuously; the shipped default is `50000`. **Raise it for long runs** — see the ceiling below. |

The assertions do not change with the clock. A longer run does not test anything new — it
tests the same four properties more times, which is the entire point of a soak.

The server under test is started with a deliberately tiny seal threshold and a one-second
compaction interval, so compaction commits blocks within seconds rather than only in a very
long run. That means the block counts below are far higher, and each block far smaller, than
any real deployment would produce; it is chosen to exercise the hot→cold commit protocol as
many times as possible, not to model production block sizing.

## Proving the gate can refuse

A gate that cannot fail is decoration. Each property was induced and caught:

| Property | Induced by | Result |
|---|---|---|
| 1 — no loss | deleting the WAL before the final recovery | `2464 of 34484 ACK'd spans are missing … (ids [32020, 32021, …])` → **fails**, naming them |
| 2 — no double-count | posting one `span_id` under many different `trace_id`s, through the ingest API alone | `12806 distinct span_ids but 12831 rows` → **fails** |
| 3 — no regression | asserting against a previous-cycle count raised above what the next cycle ingests | `recovered 23861 spans, down from 10013636` → **fails** |
| 4 — compaction under load | raising the seal threshold above anything a cycle ingests | `no new Parquet block was committed while 127052 spans were ingested` → **fails** |

Properties 1, 2 and 4 were induced through real mechanisms — a destroyed WAL, genuine
duplicate ids over the wire, compaction switched off. Property 1's proof shows it naming the
2,464 ids that went missing, which a count could only have reported as a number.

The *masking* window specifically — a loss small enough to hide behind the surplus — is argued
from the measured surplus (4–6 rows in every run above), not from a separately constructed
failure. Deleting one small block to engineer a loss that size does not produce one: the index
still references the file, so the next query fails outright rather than quietly returning fewer
rows. Worth knowing on its own, and it is why the argument here rests on the arithmetic.

Property 3 is a wiring proof: the
comparison is live and fires, but the narrow real case it exists for (a recovery that drops
rows while still clearing the cumulative ACK floor) could not be induced directly, because
the slack between recovered rows and ACKs is only a handful of spans. In practice a
regression large enough to matter trips property 1 first; property 3 is the backstop for the
case that does not.

An earlier attempt at the property-3 injection **passed**, because the offset used was
smaller than a cycle's ingest — the count had legitimately grown past it. Worth recording:
the first version of a fault injection not firing is as likely to mean the injection was too
weak as it is to mean the check is broken.

## Measured result

**CI shape — 45 s, 3 cycles, 4 workers, `EVALD_SOAK_SEAL=500`, run under `ulimit -n 1024`
to match a stock runner: PASS.**

| | |
|---|---|
| Spans ACK'd and recovered | 100,119 (every ACK'd id verified present) |
| Kill -9 cycles | 3 |
| Cold blocks committed under load | 198 (65 / 68 / 65 — flat across cycles) |
| Loss / double-count / regression | none |

Recovery returned a handful of rows *more* than were ACK'd in each cycle — the expected
in-flight slack described above, and a useful sign the ACK accounting is real rather than
tautological.

The fd limit is pinned deliberately in that run: the job's own seal threshold is chosen so
the block count stays far below a stock runner's limit, and verifying it under 1024 rather
than this machine's 20,000 is the difference between knowing that and assuming it.

**GA shape — 425 s, 12 cycles, 8 workers, `EVALD_SOAK_SEAL=1000`: PASS.**

| | |
|---|---|
| Spans ACK'd and recovered | **1,048,986** |
| Kill -9 cycles | 12 |
| Cold blocks committed under load | 1,055 (81–101 per cycle, flat) |
| Loss / double-count / regression | none, in any cycle |

A million spans and twelve uncleanly killed processes, with every ACK still present at the
end and nothing counted twice. That is the claim C13 exists to make.

Blocks per cycle stay flat rather than tailing off, which is property 4 doing its job: a
store that had quietly stopped compacting after one of those twelve crashes would show up
here as a cycle with zero new blocks, not as a slow drift nobody notices.

### A note on the throughput figures

They are indicative, not benchmarks, and two of the runs above were measured badly enough to
be worth saying so:

- An earlier version of this harness walked the whole blocks tree every 25 ms to decide when
  compaction had committed. On a growing tree that is real, growing load on the same disk the
  store is writing to. Removing it raised the CI shape from 57,076 to **81,971** spans on
  identical settings, and turned a per-cycle decline into a flat line. The harness now only
  looks once the cycle's budget is spent.
- One run overlapped with another test on the same machine, which showed up as a mid-run
  throughput dip that had nothing to do with the store.

Neither affects any of the four properties — those are logical, not timed. But it is the
reason this page does not publish a spans/s headline: throughput belongs to `bench/`, where
the method is controlled for.

### Disk

A long soak writes real data: the 12-cycle run above produced about a million spans and a
thousand Parquet blocks. Give it headroom — filling the disk mid-run does not produce an
interesting finding, it produces an unrelated failure somewhere else.

### The ceiling this gate found — and what fixed it

The first full-length run — 450 s, 15 cycles, 8 workers, at the default `EVALD_SOAK_SEAL=20`
— **failed**, and the failure was a real defect rather than a flaw in the test:

```
Object Store error: Generic LocalFileSystem error: Unable to open file
  blocks/2023/11/14/22/00000000000000020031-0.parquet: Too many open files (os error 24)
```

`EMFILE`, raised while planning `SELECT COUNT(*) FROM spans` nine cycles in, at 410,109 spans
and roughly 18,600 committed blocks. **No durability property failed** — every completed cycle
reported no loss, no double-count and no regression. What broke was the cold-tier *read* path.

The mechanism, verified rather than inferred:

- Cold blocks were **never merged**. Every sealed segment became a Parquet block and stayed
  one, so the block count only ever grew.
- `cold_spans` is registered as **one explicit listing URL per block**, and the scan opened
  them concurrently, so the open-file working set tracked the total block count.
- The ceiling was therefore the process **file-descriptor limit**, not a fixed number of
  blocks. Measured directly by re-running under a lower limit:

  | `ulimit -n` | blocks at failure |
  |---|---|
  | 512 | 716 |
  | 20,000 | ~18,600 |

Both halves are now fixed, and both are pinned by `tests/fd_ceiling.rs` rather than left to
this gate to rediscover:

- **The scan holds at most 64 blocks open**, whatever the block count, via a
  concurrency-limited object store. Measured: a scan of 2,000 blocks peaked over 1,000
  descriptors and failed under `ulimit -n 256`; it now peaks at exactly 64 and runs under
  `ulimit -n 96`.
- **The block count itself is bounded** by cold-to-cold merging (`--cold-merge-threshold`,
  on by default) — see [FORMAT.md § Merged blocks](./FORMAT.md#merged-blocks). A 600-block
  store collapses to 2 blocks and 4.4 MiB to 0.1 MiB.

One measurement from that run is worth keeping for its own sake: ingest throughput also fell
across it — 69,966 spans in cycle 1 down to 38,174 in cycle 8, on identical cycle lengths.
**Do not read that as a clean measurement of evald.** The version of the harness that
produced it walked the whole blocks tree every 25 ms to decide when compaction had committed,
so as the tree grew the test was loading the same disk it was measuring. The harness now only
walks it once the cycle's budget is spent; the direction is probably real, the magnitude is
not trustworthy, and throughput belongs to `bench/` anyway.

Raising `EVALD_SOAK_SEAL` on long runs (the GA invocation above uses `1000`) is still the
right call — not to dodge a ceiling, but because a 20-span block is nothing like a production
one and a long run at that setting spends its budget on block bookkeeping instead of on the
four durability properties.

## What it does not cover


- **Not a throughput benchmark.** The spans/s figure below is a by-product; it is measured
  with tiny seal thresholds and an unoptimised build. Throughput belongs to `bench/`.
- **Not a disk-full test.** Retention and the disk watermarks have their own guards.
- **Single node.** Multi-node durability is a separate concern from this gate.
- **No fault injection at the filesystem layer.** This kills the process, not the disk. It
  proves evald's use of fsync is correct; it does not prove the kernel or device honours it.
  A store on hardware that lies about flushes will pass this and still lose data.
