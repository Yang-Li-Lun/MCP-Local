use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Result, ensure};
use zeroize::Zeroizing;

use super::{
    audit::{Audit, Event, Kind},
    channel::{
        Ack, Control, FRESH_MS, Identity, Lease, Mode, Owner, Report, Status, fresh, now, revision,
    },
    guard::Guard,
    policy::{Policy, Risk, Snapshot, Store},
};

pub type Shared = Arc<Mutex<Authority>>;
const UNCONFIRMED: &str = "SECURITY_UNCONFIRMED：安全政策、事件或本機通訊異常，授權已撤銷。";
const PAGE_ACKS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effective {
    NoClient,
    Waiting,
    Confirmed,
    Fault,
}

pub struct View {
    pub effective: Effective,
    pub reports: Vec<Report>,
    pub lease: Lease,
    pub can_grant: bool,
    pub can_recover: bool,
}

pub struct Authority {
    store: Store,
    audit: Audit,
    active: Option<Active>,
    epoch: u64,
    approved: Option<Snapshot>,
    cold_fault: bool,
}

#[derive(PartialEq, Eq)]
pub struct GrantTarget {
    epoch: u64,
    revision: String,
    generation: u64,
}

struct Received {
    identity: Identity,
    sequence: u64,
    lost: u64,
    events: HashSet<String>,
}

struct Active {
    owner: Owner,
    saved: Snapshot,
    control: Control,
    reports: Vec<Report>,
    received: HashMap<usize, Received>,
    verified_ms: u64,
    ack_offset: usize,
}

impl Authority {
    pub fn new(directory: PathBuf) -> Self {
        let store = Store(directory.clone());
        let audit = Audit(directory);
        let approved = store.load().ok();
        let cold_fault = approved.is_none() || audit.load().is_err();
        Self {
            store,
            audit,
            active: None,
            epoch: 0,
            approved,
            cold_fault,
        }
    }

    pub fn begin(&mut self, mode: Mode) -> Result<Zeroizing<String>> {
        ensure!(
            self.active.is_none(),
            "SECURITY_CHANNEL_BUSY：已有本機安全連線。"
        );
        ensure!(!self.cold_fault, "{UNCONFIRMED}");
        let saved = self
            .store
            .load()
            .and_then(|saved| {
                ensure!(
                    self.approved
                        .as_ref()
                        .is_some_and(|prior| prior.revision == saved.revision),
                    "SECURITY_EXTERNAL_CHANGE：本機政策已外部變更。"
                );
                self.audit.load()?;
                Ok(saved)
            })
            .map_err(|error| {
                self.cold_fault = true;
                error
            })?;
        Guard::new(&saved.policy)?;
        let owner = Owner::create()?;
        let mut active = Active {
            control: Control {
                sequence: 0,
                time_ms: 0,
                mode,
                revision: revision(&saved.policy)?,
                policy: saved.policy.clone(),
                lease: Lease::default(),
                recovery: 0,
                healthy: true,
                acks: Vec::new(),
            },
            owner,
            saved,
            reports: Vec::new(),
            received: HashMap::new(),
            verified_ms: now(),
            ack_offset: 0,
        };
        active.publish()?;
        let descriptor = active.owner.descriptor()?;
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!(UNCONFIRMED))?;
        self.active = Some(active);
        Ok(descriptor)
    }

    pub fn tick(&mut self) {
        let Some(active) = &mut self.active else {
            return;
        };
        if active.update(&self.store, &self.audit).is_err() {
            active.trip(&self.audit);
        }
    }

    pub fn close(&mut self) {
        if let Some(active) = &mut self.active {
            let revoked = active.revoke(&self.audit, Kind::AuthorizationRevoked);
            let collected = active.collect(&self.audit);
            self.cold_fault |=
                !active.control.healthy || revoked.is_err() || !matches!(collected, Ok(false));
        }
        self.active = None;
    }

    pub fn save(&mut self, policy: Policy, expected: &str) -> Result<Snapshot> {
        Guard::new(&policy)?;
        let Some(active) = &mut self.active else {
            ensure!(!self.cold_fault, "{UNCONFIRMED}");
            ensure!(
                self.approved
                    .as_ref()
                    .is_some_and(|prior| prior.revision == expected),
                "SECURITY_POLICY_CONFLICT：請重新確認本機政策。"
            );
            match self.store.save(policy, expected) {
                Ok(saved) => {
                    self.approved = Some(saved.clone());
                    return Ok(saved);
                }
                Err(error) => {
                    self.cold_fault = !self
                        .store
                        .load()
                        .is_ok_and(|current| current.revision == expected);
                    return Err(error);
                }
            }
        };
        ensure!(active.control.healthy, "{UNCONFIRMED}");
        ensure!(
            active.saved.revision == expected,
            "SECURITY_POLICY_CONFLICT：請重新讀取已儲存政策。"
        );
        // Withdraw permission before touching disk, including when a later write fails.
        active.control.healthy = false;
        active.control.lease.revoke()?;
        if active.publish().is_err() {
            active.trip(&self.audit);
            anyhow::bail!("{UNCONFIRMED}");
        }
        let saved = match self.store.save(policy, expected) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                active.trip(&self.audit);
                return Err(error);
            }
        };
        active.saved = saved.clone();
        self.approved = Some(saved.clone());
        active.control.policy = saved.policy.clone();
        active.control.revision = revision(&saved.policy)?;
        active.resume()?;
        Ok(saved)
    }

    pub fn view(&self) -> View {
        let Some(active) = &self.active else {
            return View {
                effective: if self.cold_fault {
                    Effective::Fault
                } else {
                    Effective::NoClient
                },
                reports: Vec::new(),
                lease: Lease::default(),
                can_grant: false,
                can_recover: self.cold_fault,
            };
        };
        let effective = active.effective();
        View {
            effective,
            reports: active.reports.clone(),
            lease: active.control.lease,
            can_grant: active.can_grant(),
            can_recover: effective == Effective::Fault,
        }
    }

    pub fn grant_target(&self) -> Result<GrantTarget> {
        let active = self
            .active
            .as_ref()
            .filter(|active| active.can_grant())
            .ok_or_else(|| anyhow::anyhow!("HOST_AUTH_UNCONFIRMED：需要已確認的主機控制連線。"))?;
        Ok(GrantTarget {
            epoch: self.epoch,
            revision: active.control.revision.clone(),
            generation: active.control.lease.generation,
        })
    }

    pub fn grant(&mut self, minutes: u8, target: GrantTarget) -> Result<()> {
        self.tick();
        ensure!(
            self.grant_target()? == target,
            "HOST_AUTH_CHANGED：連線或政策已變更，請重新確認。"
        );
        let active = self
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("HOST_AUTH_UNCONFIRMED"))?;
        ensure!(
            active.can_grant(),
            "HOST_AUTH_UNCONFIRMED：需要已確認的主機控制連線。"
        );
        ensure!(
            (1..=active.control.policy.max_lease_minutes).contains(&minutes),
            "HOST_AUTH_DURATION：授權時間超過上限。"
        );
        self.audit
            .append(Event::new(Kind::AuthorizationGranted, Risk::High, 1)?)?;
        active.control.lease.revoke()?;
        active.control.lease.deadline_ms = now()
            .checked_add(u64::from(minutes) * 60_000)
            .ok_or_else(|| anyhow::anyhow!(UNCONFIRMED))?;
        if active.publish().is_err() {
            active.trip(&self.audit);
            anyhow::bail!("{UNCONFIRMED}");
        }
        Ok(())
    }

    pub fn revoke(&mut self) -> Result<()> {
        if let Some(active) = &mut self.active {
            if let Err(error) = active.revoke(&self.audit, Kind::AuthorizationRevoked) {
                active.trip(&self.audit);
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn recover(&mut self) -> Result<()> {
        ensure!(
            self.active
                .as_ref()
                .map_or(self.cold_fault, |active| !active.control.healthy),
            "SECURITY_RECOVERY：目前無待復原的通訊異常。"
        );
        let saved = self.store.load()?;
        ensure!(
            self.approved.as_ref().is_none_or(|prior| {
                (prior.revision.is_empty() || !saved.revision.is_empty())
                    && saved.policy == prior.policy
            }),
            "SECURITY_POLICY_CHANGED：請還原已確認的政策內容後再復原。"
        );
        Guard::new(&saved.policy)?;
        self.audit.load()?;
        if let Some(active) = &mut self.active {
            ensure!(
                !active.collect(&self.audit)?,
                "SECURITY_AUDIT_LOST：發現新的事件缺口，請檢視後復原。"
            );
        }
        self.audit
            .append(Event::new(Kind::ChannelRecovered, Risk::Low, 1)?)?;
        if let Some(active) = &mut self.active {
            active.saved = saved.clone();
            active.resume()?;
        }
        self.approved = Some(saved);
        self.cold_fault = false;
        Ok(())
    }
}

impl Active {
    fn publish(&mut self) -> Result<()> {
        self.owner.publish(&self.control)
    }

    fn effective(&self) -> Effective {
        if !self.control.healthy {
            return Effective::Fault;
        }
        let mut live = self
            .reports
            .iter()
            .filter(|report| report.status != Status::Closed);
        let ready = |report: &Report| {
            report.status == Status::Ready
                && fresh(report.time_ms, FRESH_MS)
                && report.revision == self.control.revision
                && report.generation == self.control.lease.generation
                && report.recovery == self.control.recovery
        };
        match live.next() {
            None => Effective::NoClient,
            Some(first) if ready(first) && live.all(ready) => Effective::Confirmed,
            Some(_) => Effective::Waiting,
        }
    }

    fn can_grant(&self) -> bool {
        self.control.mode == Mode::HostControl
            && self.control.policy.enabled
            && self.control.policy.host_authorization
            && self.effective() == Effective::Confirmed
    }

    fn update(&mut self, store: &Store, audit: &Audit) -> Result<()> {
        if !fresh(self.verified_ms, 1000) {
            ensure!(
                store.load()?.revision == self.saved.revision,
                "SECURITY_EXTERNAL_CHANGE"
            );
            self.verified_ms = now();
        }
        let lost = self.collect(audit)?;
        ensure!(!lost, "SECURITY_AUDIT_LOST");
        ensure!(
            !self.control.healthy
                || !self.reports.iter().any(|report| {
                    report.status == Status::Fault && report.recovery == self.control.recovery
                }),
            "SECURITY_RUNTIME_FAULT"
        );
        if self.control.lease.deadline_ms != 0 {
            if self.control.lease.remaining() == 0 {
                self.revoke(audit, Kind::AuthorizationExpired)?;
            } else {
                let mut live = self
                    .reports
                    .iter()
                    .filter(|report| report.status != Status::Closed);
                let ready = |report: &Report| {
                    report.status == Status::Ready && fresh(report.time_ms, FRESH_MS)
                };
                ensure!(
                    live.next().is_some_and(ready) && live.all(ready),
                    "SECURITY_CHANNEL_STALE"
                );
            }
        }
        self.publish()
    }

    fn collect(&mut self, audit: &Audit) -> Result<bool> {
        let reports = self.owner.reports()?;
        let mut records = Vec::new();
        let mut reported_loss = false;
        let mut observed = HashSet::new();
        for (slot, report) in &reports {
            ensure!(
                report.mode == self.control.mode
                    && report.time_ms <= now()
                    && observed.insert(report.identity),
                "SECURITY_REPORT_REPLACED"
            );
            let previous = self
                .received
                .get(slot)
                .filter(|entry| entry.identity == report.identity);
            let seen = |event: &str| previous.is_some_and(|entry| entry.events.contains(event));
            if let Some(entry) = previous {
                ensure!(
                    report.sequence >= entry.sequence && report.lost_events >= entry.lost,
                    "SECURITY_REPORT_REPLAY"
                );
            }
            records.extend(
                report
                    .events
                    .iter()
                    .filter(|event| !seen(&event.id))
                    .cloned(),
            );
            let mut delta = report.lost_events - previous.map_or(0, |entry| entry.lost);
            reported_loss |= delta != 0;
            while delta != 0 {
                let count = delta.min(1_000_000);
                records.push(Event::new(Kind::EventsLost, Risk::High, count as u32)?);
                delta -= count;
            }
        }
        audit.append_batch(records)?;
        for (slot, report) in &reports {
            self.received.insert(
                *slot,
                Received {
                    identity: report.identity,
                    sequence: report.sequence,
                    lost: report.lost_events,
                    events: report.events.iter().map(|event| event.id.clone()).collect(),
                },
            );
        }
        self.control.acks.clear();
        let mut remaining = PAGE_ACKS;
        for (_, row) in reports
            .iter()
            .cycle()
            .skip(self.ack_offset)
            .take(reports.len())
        {
            if row.status == Status::Closed || remaining == 0 || row.events.is_empty() {
                continue;
            }
            let events: Vec<_> = row
                .events
                .iter()
                .take(remaining)
                .map(|event| event.id.clone())
                .collect();
            remaining -= events.len();
            self.control.acks.push(Ack {
                identity: row.identity,
                events,
            });
        }
        if !reports.is_empty() {
            self.ack_offset = (self.ack_offset + 1) % reports.len();
        }
        self.reports = reports.iter().map(|(_, row)| row.clone()).collect();
        for (slot, report) in &reports {
            if (report.status == Status::Closed || !fresh(report.time_ms, FRESH_MS * 2))
                && self.owner.retire(*slot, report)?
            {
                // A retired run should not cause a stale diagnostic after its events are persisted.
                self.reports.retain(|row| row.identity != report.identity);
            }
        }
        Ok(reported_loss)
    }

    fn revoke(&mut self, audit: &Audit, kind: Kind) -> Result<()> {
        if self.control.lease.deadline_ms != 0 {
            self.control.lease.revoke()?;
            self.publish()?;
            audit.append(Event::new(kind, Risk::Low, 1)?)?;
        }
        Ok(())
    }

    fn trip(&mut self, audit: &Audit) {
        let was_healthy = self.control.healthy;
        self.control.healthy = false;
        if self.control.lease.deadline_ms != 0 {
            let _ = self.control.lease.revoke();
        }
        let _ = self.publish();
        if was_healthy && let Ok(event) = Event::new(Kind::ChannelFailed, Risk::High, 1) {
            let _ = audit.append(event);
        }
    }

    fn resume(&mut self) -> Result<()> {
        self.control.lease.revoke()?;
        self.control.recovery = self
            .control
            .recovery
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!(UNCONFIRMED))?;
        self.control.healthy = true;
        if let Err(error) = self.publish() {
            self.control.healthy = false;
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_cannot_adopt_an_external_policy_change_while_the_desktop_is_open() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().to_owned();
        let mut authority = Authority::new(path.clone());
        let mut active_policy = Policy::default();
        active_policy.enabled = true;
        let approved = authority.save(active_policy.clone(), "").unwrap();
        authority.begin(Mode::ReadOnly).unwrap();
        authority.close();
        let disk = Store(path);
        let outside = disk.save(Policy::default(), &approved.revision).unwrap();
        assert!(authority.begin(Mode::ReadOnly).is_err());
        assert_eq!(authority.view().effective, Effective::Fault);
        assert!(authority.recover().is_err());
        disk.save(active_policy, &outside.revision).unwrap();
        authority.recover().unwrap();
        let descriptor = authority.begin(Mode::ReadOnly).unwrap();
        let (_, policy) =
            super::super::channel::Client::attach(&descriptor, Mode::ReadOnly).unwrap();
        assert!(policy.policy.enabled);
    }

    #[test]
    fn elapsed_grant_is_revoked_and_audited_once() {
        let directory = tempfile::tempdir().unwrap();
        let mut authority = Authority::new(directory.path().to_owned());
        let mut policy = Policy::default();
        policy.enabled = true;
        authority.save(policy, "").unwrap();
        let descriptor = authority.begin(Mode::HostControl).unwrap();
        let mut runtime = super::super::Runtime::attach(
            directory.path().to_owned(),
            &descriptor,
            Mode::HostControl,
        )
        .unwrap();
        authority.tick();
        let target = authority.grant_target().unwrap();
        authority.grant(1, target).unwrap();
        runtime.heartbeat();
        authority.active.as_mut().unwrap().control.lease.deadline_ms = now().saturating_sub(1);
        authority.tick();
        runtime.heartbeat();
        authority.tick();
        assert_eq!(authority.view().lease.remaining(), 0);
        assert_eq!(authority.view().effective, Effective::Confirmed);
        assert!(!authority.view().reports[0].host_authorized);
        let events = authority.audit.load().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|row| row.event.kind == Kind::AuthorizationExpired)
                .count(),
            1
        );
    }
}
