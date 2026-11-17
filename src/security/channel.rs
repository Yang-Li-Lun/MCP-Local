use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};
#[cfg(any(feature = "desktop", test))]
use ed25519_dalek::{Signer, SigningKey};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Sha256;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    listing::hex,
    local_mapping::{Locked, Mapping},
};

use super::{audit::Event, policy::Policy, storage};

pub const ENV_NAME: &str = "MCP_LOCAL_RUST_SECURITY_CHANNEL";
pub const FRESH_MS: u64 = 3000;
pub const EVENTS: usize = 128;
const SLOTS: usize = 9;
const SLOT_BYTES: usize = 64 * 1024;
const SIZE: usize = SLOTS * SLOT_BYTES;
const PREFIX: &str = r"Local\MCP-Local-Rust.Security.";
const INVALID: &str = "SECURITY_CHANNEL_FRAME：本機通訊資料無法驗證。";
const UNAVAILABLE: &str = "SECURITY_CHANNEL_UNCONFIRMED：本機通訊無法確認。";
const STALE: &str = "SECURITY_CHANNEL_STALE：本機通訊逾時或序號倒退。";
pub(super) type Identity = [u8; 16];

pub fn now() -> u64 {
    unsafe { GetTickCount64() }
}

pub fn fresh(time: u64, limit: u64) -> bool {
    now().checked_sub(time).is_some_and(|age| age <= limit)
}

pub fn revision(policy: &Policy) -> Result<String> {
    Ok(storage::digest(&storage::encoded(policy)?))
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    ReadOnly,
    DeveloperControl,
    HostControl,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub generation: u64,
    pub deadline_ms: u64,
}

impl Lease {
    pub fn remaining(self) -> u64 {
        self.deadline_ms.saturating_sub(now()).div_ceil(1000)
    }

    #[cfg(any(feature = "desktop", test))]
    pub fn revoke(&mut self) -> Result<()> {
        self.deadline_ms = 0;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    pub identity: Identity,
    pub events: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    pub sequence: u64,
    pub time_ms: u64,
    pub policy: Policy,
    pub revision: String,
    pub mode: Mode,
    pub lease: Lease,
    pub recovery: u64,
    pub healthy: bool,
    pub acks: Vec<Ack>,
}

impl Control {
    pub(super) fn host_lease_required(&self) -> bool {
        self.policy.enabled && self.policy.host_authorization
    }

    pub(super) fn host_authorized(&self) -> bool {
        self.mode == Mode::HostControl
            && self.healthy
            && (!self.host_lease_required() || self.lease.remaining() != 0)
    }

    fn validate(&self, mode: Mode) -> Result<()> {
        self.policy.validate()?;
        ensure!(
            self.mode == mode
                && self.sequence != 0
                && self.revision == revision(&self.policy)?
                && self.acks.len() < SLOTS
                && self.acks.iter().all(|ack| ack.events.len() <= EVENTS)
                && self.lease.deadline_ms.saturating_sub(self.time_ms)
                    <= u64::from(self.policy.max_lease_minutes) * 60_000
                && (self.healthy || self.lease.deadline_ms == 0)
                && (self.lease.deadline_ms == 0
                    || self.mode == Mode::HostControl
                        && self.policy.enabled
                        && self.policy.host_authorization),
            "{INVALID}"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Attaching,
    Ready,
    Fault,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    pub passed: u64,
    pub masked: u64,
    pub blocked: u64,
    pub disabled: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub identity: Identity,
    pub sequence: u64,
    pub time_ms: u64,
    pub mode: Mode,
    pub status: Status,
    pub revision: String,
    pub generation: u64,
    pub recovery: u64,
    pub counters: Counts,
    pub host_authorized: bool,
    pub events: Vec<Event>,
    pub lost_events: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    version: u8,
    token: [u8; 32],
    public: [u8; 32],
    report: [u8; 32],
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        self.report.zeroize();
    }
}

struct Channel {
    memory: Mapping,
    report_key: Zeroizing<[u8; 32]>,
}

impl Channel {
    fn connect(descriptor: &Descriptor, create: bool) -> Result<Self> {
        let name = format!("{PREFIX}{}", hex(&descriptor.token));
        Ok(Self {
            memory: Mapping::connect(&name, SIZE, create)?,
            report_key: Zeroizing::new(descriptor.report),
        })
    }

    fn lock(&self) -> Result<Locked<'_>> {
        self.memory
            .lock(200, false)
            .ok_or_else(|| anyhow::anyhow!(UNAVAILABLE))
    }

    fn mac(&self, slot: usize, data: &[u8]) -> Hmac<Sha256> {
        let mut mac = Hmac::new_from_slice(&self.report_key[..]).expect("32-byte report key");
        mac.update(&[slot as u8]);
        mac.update(data);
        mac
    }

    fn report_at(&self, lock: &Locked<'_>, slot: usize) -> Result<Option<Report>> {
        let Some(bytes) = frame(lock, slot, 32)? else {
            return Ok(None);
        };
        let (mac, data) = bytes.split_at(32);
        self.mac(slot, data)
            .verify_slice(mac)
            .map_err(|_| anyhow::anyhow!("SECURITY_REPORT_SIGNATURE"))?;
        let report: Report = decode(data)?;
        ensure!(
            report.sequence != 0
                && report.events.len() <= EVENTS
                && report.lost_events <= u32::MAX as u64,
            "{INVALID}"
        );
        Ok(Some(report))
    }

    fn write_report(&self, lock: &Locked<'_>, slot: usize, report: &Report) -> Result<()> {
        let data = storage::encoded(report)?;
        let mac = self.mac(slot, &data).finalize().into_bytes();
        write_frame(lock, slot, &mac, &data)
    }
}

fn decode<T: DeserializeOwned>(data: &[u8]) -> Result<T> {
    serde_json::from_slice(data).map_err(|_| anyhow::anyhow!(INVALID))
}

fn frame(lock: &Locked<'_>, slot: usize, auth_len: usize) -> Result<Option<Vec<u8>>> {
    let mut size = [0; 4];
    let start = slot * SLOT_BYTES;
    lock.read(start, &mut size);
    let size = u32::from_le_bytes(size) as usize;
    if size == 0 {
        return Ok(None);
    }
    ensure!(size > auth_len && size <= SLOT_BYTES - 4, "{INVALID}");
    let mut bytes = vec![0; size];
    lock.read(start + 4, &mut bytes);
    Ok(Some(bytes))
}

fn write_frame(lock: &Locked<'_>, slot: usize, auth: &[u8], data: &[u8]) -> Result<()> {
    let size = auth.len() + data.len();
    ensure!(size <= SLOT_BYTES - 4, "SECURITY_CHANNEL_LIMIT");
    let start = slot * SLOT_BYTES;
    lock.zero(start, SLOT_BYTES);
    lock.write(start + 4, auth);
    lock.write(start + 4 + auth.len(), data);
    lock.write(start, &(size as u32).to_le_bytes());
    Ok(())
}

#[cfg(any(feature = "desktop", test))]
pub struct Owner {
    descriptor: Descriptor,
    signer: SigningKey,
    channel: Channel,
    sequence: u64,
}

#[cfg(any(feature = "desktop", test))]
impl Owner {
    pub fn create() -> Result<Self> {
        let mut seed = Zeroizing::new([0; 32]);
        getrandom::fill(&mut seed[..])?;
        let signer = SigningKey::from_bytes(&seed);
        let mut descriptor = Descriptor {
            version: 1,
            public: signer.verifying_key().to_bytes(),
            token: [0; 32],
            report: [0; 32],
        };
        getrandom::fill(&mut descriptor.token)?;
        getrandom::fill(&mut descriptor.report)?;
        let channel = Channel::connect(&descriptor, true)?;
        Ok(Self {
            descriptor,
            signer,
            channel,
            sequence: 0,
        })
    }

    pub fn descriptor(&self) -> Result<Zeroizing<String>> {
        let bytes = Zeroizing::new(serde_json::to_vec(&self.descriptor)?);
        Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(&bytes[..])))
    }

    pub fn publish(&mut self, value: &Control) -> Result<()> {
        let mut value = value.clone();
        let lock = self.channel.lock()?;
        value.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        value.time_ms = now();
        value.validate(value.mode)?;
        let data = storage::encoded(&value)?;
        let signature = self.signer.sign(&data).to_bytes();
        write_frame(&lock, 0, &signature, &data)?;
        self.sequence = value.sequence;
        Ok(())
    }

    pub fn reports(&self) -> Result<Vec<(usize, Report)>> {
        let lock = self.channel.lock()?;
        let mut reports = Vec::new();
        for slot in 1..SLOTS {
            if let Some(report) = self.channel.report_at(&lock, slot)? {
                reports.push((slot, report));
            }
        }
        Ok(reports)
    }

    pub fn retire(&self, slot: usize, report: &Report) -> Result<bool> {
        let lock = self.channel.lock()?;
        if let Some(current) = self.channel.report_at(&lock, slot)?
            && current.identity == report.identity
            && current.sequence == report.sequence
        {
            lock.zero(slot * SLOT_BYTES, SLOT_BYTES);
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(any(feature = "desktop", test))]
impl Drop for Owner {
    fn drop(&mut self) {
        if let Ok(lock) = self.channel.lock() {
            lock.zero(0, SLOT_BYTES);
        }
    }
}

pub struct Client {
    pub identity: Identity,
    mode: Mode,
    slot: usize,
    sequence: u64,
    control_sequence: u64,
    channel: Channel,
    verifier: VerifyingKey,
}

impl Client {
    pub fn attach(descriptor: &str, mode: Mode) -> Result<(Self, Control)> {
        ensure!(
            (1..=1024).contains(&descriptor.len()),
            "SECURITY_CHANNEL_DESCRIPTOR"
        );
        let bytes = Zeroizing::new(
            URL_SAFE_NO_PAD
                .decode(descriptor)
                .map_err(|_| anyhow::anyhow!("SECURITY_CHANNEL_DESCRIPTOR"))?,
        );
        let descriptor: Descriptor = decode(&bytes)?;
        ensure!(descriptor.version == 1, "SECURITY_CHANNEL_DESCRIPTOR");
        let verifier = VerifyingKey::from_bytes(&descriptor.public)
            .map_err(|_| anyhow::anyhow!("SECURITY_CHANNEL_DESCRIPTOR"))?;
        ensure!(!verifier.is_weak(), "SECURITY_CHANNEL_DESCRIPTOR");
        let channel = Channel::connect(&descriptor, false)?;
        let mut identity = [0; 16];
        getrandom::fill(&mut identity)?;
        let mut client = Self {
            identity,
            mode,
            slot: 0,
            sequence: 1,
            control_sequence: 0,
            channel,
            verifier,
        };
        let control = client.read()?;
        ensure!(control.healthy, "{UNAVAILABLE}");
        let lock = client.channel.lock()?;
        for slot in 1..SLOTS {
            if client.channel.report_at(&lock, slot)?.is_none() {
                let report = Report {
                    identity,
                    mode,
                    sequence: 1,
                    time_ms: now(),
                    status: Status::Attaching,
                    revision: control.revision.clone(),
                    generation: control.lease.generation,
                    recovery: control.recovery,
                    counters: Counts::default(),
                    host_authorized: false,
                    events: Vec::new(),
                    lost_events: 0,
                };
                client.channel.write_report(&lock, slot, &report)?;
                client.slot = slot;
                break;
            }
        }
        drop(lock);
        ensure!(client.slot != 0, "SECURITY_CHANNEL_CAPACITY");
        Ok((client, control))
    }

    pub fn read(&mut self) -> Result<Control> {
        let lock = self.channel.lock()?;
        let raw = frame(&lock, 0, 64)?.ok_or_else(|| anyhow::anyhow!(UNAVAILABLE))?;
        drop(lock);
        let (signature, data) = raw.split_at(64);
        let signature = Signature::from_slice(signature).map_err(|_| anyhow::anyhow!(INVALID))?;
        self.verifier
            .verify_strict(data, &signature)
            .map_err(|_| anyhow::anyhow!("SECURITY_CHANNEL_SIGNATURE"))?;
        let control: Control = decode(data)?;
        ensure!(
            fresh(control.time_ms, FRESH_MS) && control.sequence >= self.control_sequence,
            "{STALE}"
        );
        control.validate(self.mode)?;
        self.control_sequence = control.sequence;
        Ok(control)
    }

    pub fn report(&mut self, report: &mut Report) -> Result<()> {
        let lock = self.channel.lock()?;
        let replaced = self
            .channel
            .report_at(&lock, self.slot)?
            .is_none_or(|old| old.identity != self.identity || old.sequence != self.sequence);
        ensure!(!replaced, "SECURITY_CHANNEL_REPLACED");
        report.identity = self.identity;
        report.mode = self.mode;
        report.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        report.time_ms = now();
        self.channel.write_report(&lock, self.slot, report)?;
        self.sequence = report.sequence;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentic_frames_expire_and_signed_replays_and_tampering_are_rejected() {
        let mut owner = Owner::create().unwrap();
        let policy = Policy::default();
        let mut control = Control {
            sequence: 0,
            time_ms: 0,
            revision: revision(&policy).unwrap(),
            policy,
            mode: Mode::ReadOnly,
            lease: Lease::default(),
            recovery: 0,
            healthy: true,
            acks: Vec::new(),
        };
        owner.publish(&control).unwrap();
        let descriptor = owner.descriptor().unwrap();
        assert!(Client::attach(&descriptor, Mode::HostControl).is_err());
        let (mut client, first) = Client::attach(&descriptor, Mode::ReadOnly).unwrap();
        owner.publish(&control).unwrap();
        assert_eq!(client.read().unwrap().sequence, first.sequence + 1);

        let replace = |value: &Control, corrupt: bool| {
            let lock = owner.channel.lock().unwrap();
            let bytes = storage::encoded(value).unwrap();
            let mut signature = owner.signer.sign(&bytes).to_bytes();
            if corrupt {
                signature[0] ^= 1;
            }
            write_frame(&lock, 0, &signature, &bytes).unwrap();
        };
        replace(&first, false);
        assert!(client.read().unwrap_err().to_string().contains("STALE"));
        control.sequence = first.sequence + 2;
        control.time_ms = now().saturating_sub(FRESH_MS + 1);
        replace(&control, false);
        assert!(client.read().unwrap_err().to_string().contains("STALE"));
        control.time_ms = now();
        replace(&control, true);
        assert!(client.read().unwrap_err().to_string().contains("SIGNATURE"));

        let lock = owner.channel.lock().unwrap();
        let slot = client.slot;
        let mut frame = frame(&lock, slot, 32).unwrap().unwrap();
        frame[31] ^= 1;
        write_frame(&lock, slot, &frame[..32], &frame[32..]).unwrap();
        drop(lock);
        assert!(
            owner
                .reports()
                .unwrap_err()
                .to_string()
                .contains("SIGNATURE")
        );
    }
}
