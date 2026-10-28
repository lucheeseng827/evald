//! One call, one span: drop the second description of a call a tool reports on two signals.
//!
//! Claude Code with tracing on describes each API call twice, once as an `api_request` log
//! event and once as a `claude_code.llm_request` span ([`crate::claude_code`]). Both copies are
//! given the same span id and the same **call key** (`evald.call_key`). The [`CallLedger`]
//! remembers the keys this node recently stored, and [`crate::Store::ingest_batch`] drops a span
//! whose key it has already stored, before the append, so the call is written and counted
//! (`/metrics` included) once.
//!
//! A span can instead carry `evald.call_anchor`: it is never dropped, but it registers its key,
//! so a later copy is. That is for a span other spans hang under (a traced tool span), which
//! cannot be dropped without orphaning them.
//!
//! # What this does not catch
//!
//! * **Two copies in flight at once.** A key is remembered only after its span is durable, so
//!   a retry of a refused request is never mistaken for a duplicate; two requests carrying the
//!   same call that are appended concurrently both pass. Copies of an LLM call share a span id,
//!   so the store still keeps one of them; `/metrics` may count both.
//! * **A restart between the copies.** The ledger is in memory; the shared span id is the
//!   backstop there too.
//! * **More than [`CAPACITY`] calls between the copies.** The oldest keys are forgotten first;
//!   the two signals of one call arrive seconds apart, so the window is far larger than needed.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::Value as Json;

use crate::model::NormalizedSpan;

/// The raw attribute naming a droppable copy of a call.
pub const CALL_KEY: &str = "evald.call_key";
/// The raw attribute naming a call a span anchors: registered, never dropped.
pub const CALL_ANCHOR: &str = "evald.call_anchor";
/// Keys remembered before the oldest are forgotten (a few MB at most).
pub const CAPACITY: usize = 16_384;

#[derive(Default)]
struct Recent {
    set: HashSet<String>,
    order: VecDeque<String>,
}

/// The keys of calls this node recently stored. Shared by every ingest door.
pub struct CallLedger {
    recent: Mutex<Recent>,
    capacity: usize,
    dropped: AtomicU64,
}

impl Default for CallLedger {
    fn default() -> Self {
        Self::with_capacity(CAPACITY)
    }
}

impl CallLedger {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            recent: Mutex::new(Recent::default()),
            capacity: capacity.max(1),
            dropped: AtomicU64::new(0),
        }
    }

    /// Can any span in this batch carry a call key? A prefix check per span, so a batch from
    /// any other source costs the ledger nothing (no lock, no attribute lookup).
    pub fn may_carry_keys(spans: &[NormalizedSpan]) -> bool {
        spans.iter().any(|s| s.name.starts_with("claude_code."))
    }

    /// Drop the spans whose call was already stored (or already appears earlier in this batch).
    /// Returns the keys to [`remember`](Self::remember) once the rest is durable.
    pub fn filter(&self, spans: &mut Vec<NormalizedSpan>) -> Vec<String> {
        let recent = self.recent.lock().expect("call ledger lock");
        let mut keys = Vec::new();
        let mut in_batch: HashSet<String> = HashSet::new();
        let mut dropped = 0u64;
        spans.retain(|s| {
            if let Some(anchor) = s.raw_attributes.get(CALL_ANCHOR).and_then(Json::as_str) {
                if in_batch.insert(anchor.to_string()) {
                    keys.push(anchor.to_string());
                }
                return true;
            }
            let Some(key) = s.raw_attributes.get(CALL_KEY).and_then(Json::as_str) else {
                return true;
            };
            if recent.set.contains(key) || in_batch.contains(key) {
                dropped += 1;
                return false;
            }
            in_batch.insert(key.to_string());
            keys.push(key.to_string());
            true
        });
        drop(recent);
        if dropped > 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
        keys
    }

    /// Remember keys whose spans are now durable, forgetting the oldest past capacity.
    pub fn remember(&self, keys: Vec<String>) {
        if keys.is_empty() {
            return;
        }
        let mut r = self.recent.lock().expect("call ledger lock");
        for k in keys {
            if r.set.insert(k.clone()) {
                r.order.push_back(k);
            }
        }
        while r.order.len() > self.capacity {
            if let Some(old) = r.order.pop_front() {
                r.set.remove(&old);
            }
        }
    }

    /// Spans dropped as a second copy of a stored call, since the process started.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(name: &str, attr: Option<(&str, &str)>) -> NormalizedSpan {
        let proto = opentelemetry_proto::tonic::trace::v1::Span {
            name: name.to_string(),
            ..Default::default()
        };
        let mut s = crate::normalize::normalize_span(&proto, None, None, None);
        if let Some((k, v)) = attr {
            s.raw_attributes.insert(k.into(), Json::String(v.into()));
        }
        s
    }

    #[test]
    fn a_stored_call_is_dropped_next_time_and_within_a_batch() {
        let ledger = CallLedger::default();
        let mut first = vec![
            span("claude_code.llm_request", Some((CALL_KEY, "k1"))),
            span("claude_code.llm_request", Some((CALL_KEY, "k1"))),
            span("other", None),
        ];
        let keys = ledger.filter(&mut first);
        assert_eq!(
            first.len(),
            2,
            "the in-batch twin is dropped, the unkeyed span kept"
        );
        ledger.remember(keys);

        let mut second = vec![span("claude_code.llm_request", Some((CALL_KEY, "k1")))];
        ledger.filter(&mut second);
        assert!(second.is_empty());
        assert_eq!(ledger.dropped(), 2);
    }

    #[test]
    fn a_key_is_remembered_only_once_its_batch_is_durable() {
        let ledger = CallLedger::default();
        let mut batch = vec![span("claude_code.llm_request", Some((CALL_KEY, "k")))];
        let _keys = ledger.filter(&mut batch);
        // The append failed, so remember() is never called: the retry must go through.
        let mut retry = vec![span("claude_code.llm_request", Some((CALL_KEY, "k")))];
        ledger.filter(&mut retry);
        assert_eq!(retry.len(), 1);
    }

    #[test]
    fn an_anchor_is_never_dropped_but_drops_a_later_copy() {
        let ledger = CallLedger::default();
        let mut traced = vec![span("claude_code.tool", Some((CALL_ANCHOR, "t")))];
        ledger.remember(ledger.filter(&mut traced));
        assert_eq!(traced.len(), 1);
        let mut again = vec![span("claude_code.tool", Some((CALL_ANCHOR, "t")))];
        ledger.filter(&mut again);
        assert_eq!(again.len(), 1, "an anchor is never dropped");
        let mut event = vec![span("claude_code.tool", Some((CALL_KEY, "t")))];
        ledger.filter(&mut event);
        assert!(event.is_empty());
    }

    #[test]
    fn the_oldest_keys_are_forgotten_past_capacity() {
        let ledger = CallLedger::with_capacity(2);
        for k in ["a", "b", "c"] {
            ledger.remember(vec![k.to_string()]);
        }
        let mut old = vec![span("claude_code.x", Some((CALL_KEY, "a")))];
        ledger.filter(&mut old);
        assert_eq!(old.len(), 1, "forgotten");
        let mut kept = vec![span("claude_code.x", Some((CALL_KEY, "c")))];
        ledger.filter(&mut kept);
        assert!(kept.is_empty());
    }

    #[test]
    fn only_claude_code_batches_reach_the_ledger() {
        assert!(!CallLedger::may_carry_keys(&[span("chat", None)]));
        assert!(CallLedger::may_carry_keys(&[
            span("chat", None),
            span("claude_code.tool", None)
        ]));
    }
}
