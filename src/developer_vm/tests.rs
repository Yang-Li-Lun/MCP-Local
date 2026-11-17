use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    List,
    Start(Uuid),
    Connect(Uuid),
    Stop(Uuid),
    Close(Uuid),
}

#[derive(Default)]
enum Start {
    #[default]
    Confirmed,
    NotIssued,
    WrongAcknowledgement,
    TimedOut,
}

#[derive(Default)]
struct Fixture {
    inventory: BTreeSet<Uuid>,
    start: Start,
    stop_still_running: bool,
    list_unavailable: bool,
    connector_unavailable: bool,
    cancel_on_connect: Option<CancellationToken>,
    events: Vec<Event>,
}

impl Backend for Fixture {
    type Connector = Uuid;

    fn list(&mut self) -> Result<BTreeSet<Uuid>> {
        self.events.push(Event::List);
        ensure!(!self.list_unavailable, "synthetic list failure");
        Ok(self.inventory.clone())
    }

    fn start(&mut self, id: Uuid, _: &str) -> Result<Uuid, InvocationFailure> {
        self.events.push(Event::Start(id));
        match self.start {
            Start::Confirmed => {
                self.inventory.insert(id);
                Ok(id)
            }
            Start::NotIssued => Err(InvocationFailure::BeforeSpawn(anyhow::anyhow!(
                "synthetic spawn failure"
            ))),
            Start::WrongAcknowledgement => {
                self.inventory.insert(id);
                Ok(other())
            }
            Start::TimedOut => Err(InvocationFailure::AfterSpawn(anyhow::anyhow!(
                "synthetic start timeout"
            ))),
        }
    }

    fn connect(&mut self, id: Uuid) -> Result<Self::Connector> {
        self.events.push(Event::Connect(id));
        if let Some(cancel) = &self.cancel_on_connect {
            cancel.cancel();
        }
        Ok(id)
    }

    fn stop(&mut self, id: Uuid) -> Result<()> {
        self.events.push(Event::Stop(id));
        if !self.stop_still_running {
            self.inventory.remove(&id);
        }
        Ok(())
    }

    fn close_connector(&mut self, connector: &mut Self::Connector) -> Result<()> {
        self.events.push(Event::Close(*connector));
        ensure!(
            !self.connector_unavailable,
            "synthetic connector cleanup failure"
        );
        ensure!(
            !self.inventory.contains(connector),
            "connector closed before absence"
        );
        Ok(())
    }
}

fn other() -> Uuid {
    Uuid::from_u128(42)
}

const CONFIG: &str = "<Configuration/>";

#[test]
fn foreign_sandbox_and_unavailable_inventory_prevent_start() {
    let mut lifecycle = Lifecycle::new(Fixture {
        inventory: BTreeSet::from([other()]),
        ..Fixture::default()
    });
    let token = CancellationToken::new();
    assert!(
        lifecycle
            .start(CONFIG, &token)
            .unwrap_err()
            .to_string()
            .contains("VM_BUSY")
    );
    lifecycle.stop().unwrap();
    assert_eq!(lifecycle.backend.events, [Event::List]);
    lifecycle.backend.inventory.clear();
    lifecycle.backend.list_unavailable = true;
    assert!(lifecycle.start(CONFIG, &token).is_err());
    assert_eq!(lifecycle.backend.events, [Event::List, Event::List]);
    assert!(lifecycle.owned.is_none());
}

#[test]
fn stop_retries_only_owned_id_and_requires_absence_and_connector_cleanup() {
    let mut lifecycle = Lifecycle::new(Fixture::default());
    let token = CancellationToken::new();
    let id = lifecycle.start(CONFIG, &token).unwrap();
    assert_eq!(
        lifecycle.backend.events,
        [Event::List, Event::Start(id), Event::Connect(id)]
    );
    assert_eq!(id.get_version_num(), 4);
    lifecycle.backend.inventory.insert(other());
    lifecycle.backend.stop_still_running = true;
    assert!(
        lifecycle
            .stop()
            .unwrap_err()
            .to_string()
            .contains("VM_STOP_UNCONFIRMED")
    );
    assert_eq!(lifecycle.owned.as_ref().unwrap().id, id);
    assert!(
        lifecycle
            .start(CONFIG, &token)
            .unwrap_err()
            .to_string()
            .contains("VM_BUSY")
    );
    assert_eq!(
        lifecycle.backend.events[3..],
        [Event::List, Event::Stop(id), Event::List]
    );
    lifecycle.backend.stop_still_running = false;
    lifecycle.backend.connector_unavailable = true;
    assert!(
        lifecycle
            .stop()
            .unwrap_err()
            .to_string()
            .contains("VM_STOP_UNCONFIRMED")
    );
    assert_eq!(lifecycle.backend.inventory, BTreeSet::from([other()]));
    assert!(lifecycle.owned.is_some());
    lifecycle.backend.connector_unavailable = false;
    lifecycle.stop().unwrap();
    assert!(lifecycle.owned.is_none());
    assert_eq!(lifecycle.backend.inventory, BTreeSet::from([other()]));
    assert_eq!(
        lifecycle.backend.events[6..],
        [
            Event::List,
            Event::Stop(id),
            Event::List,
            Event::Close(id),
            Event::List,
            Event::List,
            Event::Close(id)
        ]
    );
}

#[test]
fn cancelling_after_connect_cleans_up_before_reporting_cancellation() {
    let token = CancellationToken::new();
    let mut lifecycle = Lifecycle::new(Fixture {
        cancel_on_connect: Some(token.clone()),
        ..Fixture::default()
    });
    assert!(
        lifecycle
            .start(CONFIG, &token)
            .unwrap_err()
            .to_string()
            .contains("VM_START_CANCELLED")
    );
    assert!(lifecycle.owned.is_none());
    let Event::Start(id) = lifecycle.backend.events[1] else {
        panic!("missing owned start");
    };
    assert_eq!(
        lifecycle.backend.events,
        [
            Event::List,
            Event::Start(id),
            Event::Connect(id),
            Event::List,
            Event::Stop(id),
            Event::List,
            Event::Close(id)
        ]
    );
}

#[test]
fn timeout_without_acknowledged_id_keeps_ownership_until_seen_and_stopped() {
    let token = CancellationToken::new();
    let mut lifecycle = Lifecycle::new(Fixture {
        start: Start::TimedOut,
        ..Fixture::default()
    });
    assert!(
        lifecycle
            .start(CONFIG, &token)
            .unwrap_err()
            .to_string()
            .contains("VM_STOP_UNCONFIRMED")
    );
    let id = lifecycle.owned.as_ref().unwrap().id;
    assert_eq!(
        lifecycle.backend.events,
        [Event::List, Event::Start(id), Event::List]
    );
    assert!(
        lifecycle
            .stop()
            .unwrap_err()
            .to_string()
            .contains("VM_STOP_UNCONFIRMED")
    );
    lifecycle.backend.inventory.extend([id, other()]);
    lifecycle.stop().unwrap();
    assert_eq!(lifecycle.backend.inventory, BTreeSet::from([other()]));
    assert!(lifecycle.owned.is_none());
}

#[test]
fn wrong_acknowledgement_is_never_adopted_and_unissued_request_keeps_no_owner() {
    let token = CancellationToken::new();
    let mut lifecycle = Lifecycle::new(Fixture {
        start: Start::WrongAcknowledgement,
        ..Fixture::default()
    });
    assert!(
        lifecycle
            .start(CONFIG, &token)
            .unwrap_err()
            .to_string()
            .contains("VM_CLI_PROTOCOL")
    );
    let Event::Start(id) = lifecycle.backend.events[1] else {
        panic!("missing owned start");
    };
    assert_eq!(
        lifecycle.backend.events,
        [
            Event::List,
            Event::Start(id),
            Event::List,
            Event::Stop(id),
            Event::List
        ]
    );
    assert_ne!(id, other());
    assert!(lifecycle.owned.is_none());

    lifecycle.backend.start = Start::NotIssued;
    assert!(
        lifecycle
            .start(CONFIG, &token)
            .unwrap_err()
            .to_string()
            .contains("synthetic spawn failure")
    );
    assert!(lifecycle.owned.is_none());
    assert_eq!(lifecycle.backend.events.len(), 7);
}

#[test]
fn cancellation_and_utf16_configuration_limit_win_before_accessing_the_cli() {
    let token = CancellationToken::new();
    let mut lifecycle = Lifecycle::new(Fixture::default());
    assert!(
        lifecycle
            .start(&"😀".repeat(14_001), &token)
            .unwrap_err()
            .to_string()
            .contains("VM_CONFIG_LIMIT")
    );
    token.cancel();
    assert!(
        lifecycle
            .start(CONFIG, &token)
            .unwrap_err()
            .to_string()
            .contains("VM_START_CANCELLED")
    );
    assert!(lifecycle.backend.events.is_empty());
}
pub(super) const BRIDGE_FIXTURE_SCRIPT: &str = r#"
$seed = [IO.File]::ReadAllText((Join-Path $env:MCP_WORKSPACE 'seed.txt'))
[Console]::WriteLine('sandbox-ready:' + $seed)
$inputLine = [Console]::ReadLine()
if ($null -eq $inputLine -or $null -ne [Console]::ReadLine()) { exit 13 }
[IO.File]::WriteAllText((Join-Path $env:MCP_WORKSPACE 'proof.txt'), $inputLine, [Text.UTF8Encoding]::new($false))
[Console]::Error.WriteLine('sandbox-stderr')
[Console]::WriteLine('sandbox-output:' + $inputLine)
exit 7
"#;
