//! Synchronization engine (spec §5): bounded batches, duplicate detection,
//! history checkpoints for recovery after downtime, and acknowledgment
//! tracking. No local trading database — batches are transient and dropped
//! once acknowledged.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::mt5::{
    AccountSnapshot, HistoryOrdersResponse, HistoryPositionsResponse, Mt5Deal,
    OpenPositionsResponse,
};

#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Maximum unacknowledged batches kept in memory. Oldest are evicted
    /// with a warning when the bound is hit (bounded memory beats unbounded
    /// queues; eviction is logged, never silent).
    pub max_pending_batches: usize,
    /// Bound on the deduplication cache (approximate LRU).
    pub dedup_capacity: usize,
    /// Page size used for history range queries.
    pub history_page_limit: u32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            max_pending_batches: 32,
            dedup_capacity: 50_000,
            history_page_limit: 500,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchKind {
    Account,
    Positions,
    HistoryOrders,
    HistoryPositions,
}

impl BatchKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BatchKind::Account => "account",
            BatchKind::Positions => "positions",
            BatchKind::HistoryOrders => "history_orders",
            BatchKind::HistoryPositions => "history_positions",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncBatch {
    pub batch_id: String,
    pub kind: BatchKind,
    pub items: Value,
    pub count: usize,
    /// Connector-side production time, RFC 3339.
    pub produced_at: String,
}

/// One synchronization step: build batches from MT5 payloads, keep them in a
/// bounded pending queue until NSE Core acknowledges them, deduplicate deals,
/// and maintain the history checkpoint used for gap recovery.
pub struct SyncEngine {
    cfg: SyncConfig,
    checkpoint: Option<String>,
    seen_deals: DedupSet,
    seen_orders: DedupSet,
    pending: VecDeque<SyncBatch>,
}

impl SyncEngine {
    pub fn new(cfg: SyncConfig) -> Self {
        let dedup_capacity = cfg.dedup_capacity;
        Self {
            cfg,
            checkpoint: None,
            seen_deals: DedupSet::new(dedup_capacity),
            seen_orders: DedupSet::new(dedup_capacity),
            pending: VecDeque::new(),
        }
    }

    pub fn checkpoint(&self) -> Option<&str> {
        self.checkpoint.as_deref()
    }

    pub fn pending(&self) -> impl Iterator<Item = &SyncBatch> {
        self.pending.iter()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Builds an account snapshot batch.
    pub fn build_account_batch(&mut self, snap: &AccountSnapshot) -> SyncBatch {
        let items = serde_json::to_value(snap).unwrap_or(Value::Null);
        self.make_batch(
            BatchKind::Account,
            items,
            snap.account.login.as_deref().map_or(0, |_| 1),
        )
    }

    /// Builds a positions batch from the current open positions/orders.
    pub fn build_positions_batch(&mut self, pos: &OpenPositionsResponse) -> SyncBatch {
        let count = pos.positions.len() + pos.orders.len();
        let items = serde_json::to_value(pos).unwrap_or(Value::Null);
        self.make_batch(BatchKind::Positions, items, count)
    }

    /// Builds a history batch: deduplicates deals AND orders by ticket
    /// (history windows overlap between pages), updates the checkpoint to
    /// the newest deal time seen (broker time, verbatim). Records without
    /// tickets are dropped — they cannot be reconciled server-side and
    /// would corrupt accounting.
    pub fn build_history_batch(&mut self, hist: &HistoryOrdersResponse) -> SyncBatch {
        let fresh_deals: Vec<&Mt5Deal> = hist
            .deals
            .iter()
            .filter(|d| d.is_reconcilable())
            .filter(|d| {
                if let Some(t) = &d.deal_id {
                    self.seen_deals.insert(t)
                } else {
                    false
                }
            })
            .collect();
        let orders = hist
            .orders
            .iter()
            .filter(|o| o.is_reconcilable())
            .filter(|o| {
                if let Some(t) = &o.order_id {
                    self.seen_orders.insert(t)
                } else {
                    false
                }
            })
            .collect::<Vec<_>>();
        for d in &fresh_deals {
            self.advance_checkpoint(d.open_time.as_deref());
        }
        let items = serde_json::json!({
            "orders": orders,
            "deals": fresh_deals,
        });
        self.make_batch(
            BatchKind::HistoryOrders,
            items,
            orders.len() + fresh_deals.len(),
        )
    }

    /// Builds a closed-positions batch (realized P/L source).
    pub fn build_history_positions_batch(&mut self, hist: &HistoryPositionsResponse) -> SyncBatch {
        let positions = hist
            .positions
            .iter()
            .filter(|p| p.is_reconcilable())
            .collect::<Vec<_>>();
        for p in &positions {
            self.advance_checkpoint(p.closed_at.as_deref());
        }
        let items = serde_json::json!({ "positions": positions });
        self.make_batch(BatchKind::HistoryPositions, items, positions.len())
    }

    /// The query range for the next history sync: from the checkpoint (or
    /// the beginning of time on first sync) to now. After downtime this
    /// yields the gap that must be recovered (spec §5C/§15).
    pub fn history_query_range(&self, now: &str) -> (String, String) {
        (
            self.checkpoint
                .clone()
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string()),
            now.to_string(),
        )
    }

    /// Standard page-size parameter for history queries.
    pub fn history_page_limit(&self) -> u32 {
        self.cfg.history_page_limit
    }

    /// Queues a batch for delivery. The pending queue is bounded; the oldest
    /// unacknowledged batch is evicted with a warning when full.
    pub fn queue(&mut self, batch: SyncBatch) {
        while self.pending.len() >= self.cfg.max_pending_batches {
            if let Some(dropped) = self.pending.pop_front() {
                tracing::warn!(
                    batch_id = %dropped.batch_id,
                    kind = %dropped.kind.as_str(),
                    "pending queue full; evicting oldest unacknowledged batch"
                );
            }
        }
        self.pending.push_back(batch);
    }

    /// Removes a batch from the pending queue after server acknowledgment.
    /// Returns true if it was pending.
    pub fn mark_acked(&mut self, batch_id: &str) -> bool {
        let before = self.pending.len();
        self.pending.retain(|b| b.batch_id != batch_id);
        self.pending.len() != before
    }

    /// Convenience: formats "now" as RFC 3339 for query ranges.
    pub fn now_rfc3339() -> String {
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
    }

    /// Small helper for tests/integrations that want a default page wait.
    pub fn default_page_pause() -> Duration {
        Duration::from_millis(250)
    }

    fn make_batch(&self, kind: BatchKind, items: Value, count: usize) -> SyncBatch {
        SyncBatch {
            batch_id: uuid::Uuid::now_v7().to_string(),
            kind,
            items,
            count,
            produced_at: Self::now_rfc3339(),
        }
    }

    /// Checkpoint advances to the MAXIMUM observed broker time — delayed or
    /// out-of-order records must never move it backwards. Ordering uses
    /// `parse_broker_time` (RFC 3339 or timezone-less ISO, both verified
    /// live); unparseable values never move the checkpoint.
    fn advance_checkpoint(&mut self, broker_time: Option<&str>) {
        let Some(t) = broker_time else { return };
        let Some(parsed) = crate::mt5::parse_broker_time(t) else {
            return;
        };
        let is_newer = self
            .checkpoint
            .as_deref()
            .and_then(crate::mt5::parse_broker_time)
            .map(|cur| parsed > cur)
            .unwrap_or(true);
        if is_newer {
            self.checkpoint = Some(t.to_string());
        }
    }
}

/// Bounded deduplication set with approximate FIFO/LRU eviction.
pub struct DedupSet {
    capacity: usize,
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl DedupSet {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            seen: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// Returns true when the id has not been seen before.
    pub fn insert(&mut self, id: &str) -> bool {
        if self.seen.contains(id) {
            return false;
        }
        while self.seen.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.seen.insert(id.to_string());
        self.order.push_back(id.to_string());
        true
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}
