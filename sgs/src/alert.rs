//! Alert push → forced subscription (spec §3.2 "Alerts" trigger, Phase 3).
//!
//! Alerts are a **narrow push path** that should wake interest without polling:
//! an incoming alert force-activates the implicated prims (and could raise their
//! telemetry priority) regardless of any camera AOI. This module is the
//! in-process seam for that flow — an [`AlertSource`] the runtime drains, and a
//! helper that turns drained events into a high-priority forced [`Subscription`]
//! (reusing the origin-agnostic subscription design in [`crate::interest`]).
//!
//! Scope (locked, Phase 3): seam + [`StubAlertSource`] only. A real MQTT /
//! webhook receiver is a thin adapter that implements [`AlertSource`] behind
//! this same seam — it does not change the forced-subscription mechanism.

use crate::interest::{
    Interaction, InterestManager, SubscriberKind, Subscription, SubscriptionId,
};
use crate::lsg::{EntityId, Lsg};

/// A single alert / exception event naming the affected asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertEvent {
    /// Asset tag (e.g. `PUMP-01`) or prim path (e.g. `/PumpStation01/...`).
    pub selector: String,
    /// Human-readable cause (e.g. `high_vibration`).
    pub reason: String,
}

impl AlertEvent {
    pub fn new(selector: impl Into<String>, reason: impl Into<String>) -> Self {
        AlertEvent {
            selector: selector.into(),
            reason: reason.into(),
        }
    }
}

/// A source of alert events the runtime drains each tick. Implemented by the
/// stub below now; by an MQTT/webhook adapter later (same seam).
pub trait AlertSource {
    fn drain(&mut self) -> Vec<AlertEvent>;
}

/// In-memory alert source for demos and tests: `push` queues events, `drain`
/// hands them over (and empties the queue).
#[derive(Debug, Default)]
pub struct StubAlertSource {
    queued: Vec<AlertEvent>,
}

impl StubAlertSource {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, selector: impl Into<String>, reason: impl Into<String>) {
        self.queued.push(AlertEvent::new(selector, reason));
    }
}

impl AlertSource for StubAlertSource {
    fn drain(&mut self) -> Vec<AlertEvent> {
        std::mem::take(&mut self.queued)
    }
}

/// Turn alert events into a forced subscription: resolve each selector against
/// the LSG and upsert a single `AlertRule` [`Subscription`] whose `entity_ids`
/// force those prims into the RSG regardless of the camera AOI (spec §3.2).
///
/// Returns the entity ids that were force-activated. If none of the selectors
/// resolve, the subscription is not created and an empty vec is returned.
pub fn force_subscription(
    events: &[AlertEvent],
    lsg: &Lsg,
    im: &mut InterestManager,
    sub_id: SubscriptionId,
    budget: usize,
) -> Vec<EntityId> {
    let mut ids = Vec::new();
    for ev in events {
        if let Some(e) = lsg.resolve_selector(&ev.selector) {
            if !ids.contains(&e.id) {
                ids.push(e.id);
            }
        }
    }
    if ids.is_empty() {
        return ids;
    }

    let sub = Subscription {
        id: sub_id,
        kind: SubscriberKind::AlertRule,
        // Not a viewer profile; no interaction gating applies.
        viewer_profile: None,
        // Alerts are not spatial — pure explicit selection that bypasses the AOI.
        region: None,
        entity_ids: ids.clone(),
        tags_filter: Vec::new(),
        budget,
        interaction: Interaction::ReadOnly,
    };
    // AlertRule subscriptions carry no viewer profile, so validation is a no-op.
    let _ = im.upsert(sub);
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::build_from_ndjson;
    use crate::spatial::SpatialIndex;

    fn lsg() -> Lsg {
        let nd = concat!(
            r#"{"primPath":"/W/Pump_01","kind":"component","transform":{"translate":[0,0,0]},"extentsHint":[[-1,-1,-1],[1,1,1]],"vf":{"assetTag":"PUMP-01"}}"#,
            "\n",
            r#"{"primPath":"/W/Pump_02","kind":"component","transform":{"translate":[100,0,0]},"extentsHint":[[99,-1,-1],[101,1,1]],"vf":{"assetTag":"PUMP-02"}}"#,
        );
        build_from_ndjson(nd.as_bytes()).unwrap()
    }

    #[test]
    fn alert_force_activates_regardless_of_aoi() {
        let lsg = lsg();
        let idx = SpatialIndex::build(&lsg);
        let mut im = InterestManager::new();
        // No spatial subscription at all: only the alert drives activation.
        let alerts = [AlertEvent::new("PUMP-02", "high_vibration")];
        let ids = force_subscription(&alerts, &lsg, &mut im, 900, 16);
        assert_eq!(ids, vec![EntityId::from_prim_path("/W/Pump_02")]);

        let t = im.evaluate(&lsg, &idx);
        let active: Vec<EntityId> = t.activated.iter().map(|(_, e)| *e).collect();
        assert!(active.contains(&EntityId::from_prim_path("/W/Pump_02")));
    }

    #[test]
    fn unresolved_selectors_create_no_subscription() {
        let lsg = lsg();
        let mut im = InterestManager::new();
        let alerts = [AlertEvent::new("NOPE-99", "x")];
        let ids = force_subscription(&alerts, &lsg, &mut im, 900, 16);
        assert!(ids.is_empty());
        assert_eq!(im.subscription_count(), 0);
    }
}
