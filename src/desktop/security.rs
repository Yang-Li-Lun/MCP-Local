use std::{
    cell::RefCell,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex},
};

use anyhow::{Result, ensure};
use serde_json::json;
use slint::{ComponentHandle, Model, ModelRc, SharedString, StandardListViewItem, VecModel};

use crate::{
    listing::hex,
    security::{
        audit::{Audit, Event, Kind, Row},
        authority::{Authority, Effective, Shared},
        channel::{self, Mode, Status},
        guard::{Guard, Review},
        policy::{Policy, Risk, Rule, RuleKind, SessionExpiry, Snapshot, Store, Strategy},
    },
};

use super::{Desktop, SecurityRule, SecurityState};

const EVENT_KINDS: [(Kind, &str); 15] = [
    (Kind::PolicySaved, "政策已儲存"),
    (Kind::AuthorizationGranted, "主機授權"),
    (Kind::AuthorizationRevoked, "撤銷授權"),
    (Kind::AuthorizationExpired, "授權到期"),
    (Kind::ChannelFailed, "安全狀態異常"),
    (Kind::ChannelRecovered, "通訊已恢復"),
    (Kind::ResponseMasked, "回應已遮罩"),
    (Kind::ResponseBlocked, "回應已阻擋"),
    (Kind::ScanFailed, "檢查失敗"),
    (Kind::CommandDenied, "命令已拒絕"),
    (Kind::SessionStopped, "工作已停止"),
    (Kind::SessionStopFailed, "工作停止未確認"),
    (Kind::SyntheticTest, "合成測試"),
    (Kind::AuditCleared, "事件已清除"),
    (Kind::EventsLost, "事件遺失"),
];

const RISKS: [Risk; 3] = [Risk::Low, Risk::Medium, Risk::High];
const PAGE: usize = 100;
const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";

struct Editor {
    store: Store,
    audit: Audit,
    authority: Shared,
    revision: Option<String>,
    rules: Rc<VecModel<SecurityRule>>,
    events: Vec<Row>,
    event_revision: Option<String>,
    event_page: Vec<usize>,
    kind: Option<Kind>,
    risk: Option<Risk>,
    offset: usize,
}

fn feedback(ui: &Desktop, text: impl ToString) {
    ui.global::<SecurityState>()
        .set_feedback(text.to_string().into());
}

fn rules_model(rules: &[Rule]) -> Vec<SecurityRule> {
    rules
        .iter()
        .map(|rule| SecurityRule {
            id: rule.id.as_str().into(),
            pattern: rule.pattern.as_str().into(),
            kind: i32::from(rule.kind == RuleKind::Literal),
            enabled: rule.enabled,
            risk: RISKS.iter().position(|risk| *risk == rule.risk).unwrap() as i32,
        })
        .collect()
}

fn label(row: &Row) -> SharedString {
    let kind = EVENT_KINDS
        .iter()
        .find(|(kind, _)| *kind == row.event.kind)
        .map_or("安全事件", |(_, label)| *label);
    let risk = match row.event.risk {
        Risk::Low => "低",
        Risk::Medium => "中",
        Risk::High => "高",
    };
    format!(
        "{}　{kind}　風險 {risk}　×{}",
        row.event.time, row.event.count
    )
    .into()
}

impl Editor {
    fn hydrate(&self, ui: &Desktop, policy: &Policy) {
        let state = ui.global::<SecurityState>();
        state.set_enabled(policy.enabled);
        state.set_host(policy.host_authorization);
        state.set_detector(policy.detector_enabled);
        state.set_guard(policy.response_guard);
        state.set_strategy(i32::from(policy.strategy == Strategy::Block));
        state.set_expiry(i32::from(policy.session_expiry == SessionExpiry::Finish));
        state.set_maximum(policy.max_lease_minutes.to_string().into());
        state.set_preview("".into());
        self.rules.set_vec(rules_model(&policy.rules));
    }

    fn stored(&mut self, ui: &Desktop, snapshot: Snapshot) {
        let state = ui.global::<SecurityState>();
        state.set_saved_state(
            if snapshot.revision.is_empty() {
                "預設關閉 · 尚未儲存"
            } else if snapshot.policy.enabled {
                "已儲存 · 安全政策啟用"
            } else {
                "已儲存 · 安全政策停用"
            }
            .into(),
        );
        self.revision = Some(snapshot.revision);
        self.hydrate(ui, &snapshot.policy);
        state.set_ready(true);
    }

    fn reload(&mut self, ui: &Desktop) -> Result<()> {
        self.revision = None;
        let state = ui.global::<SecurityState>();
        state.set_ready(false);
        state.set_saved_state("政策或安全事件尚未驗證".into());
        let snapshot = self.store.load()?;
        self.read_events(ui)?;
        self.stored(ui, snapshot);
        Ok(())
    }

    fn draft(&self, ui: &Desktop) -> Result<Policy> {
        let state = ui.global::<SecurityState>();
        let mut policy = Policy::default();
        policy.enabled = state.get_enabled();
        policy.host_authorization = state.get_host();
        policy.detector_enabled = state.get_detector();
        policy.response_guard = state.get_guard();
        policy.strategy = if state.get_strategy() == 1 {
            Strategy::Block
        } else {
            Strategy::Mask
        };
        policy.session_expiry = if state.get_expiry() == 1 {
            SessionExpiry::Finish
        } else {
            SessionExpiry::Terminate
        };
        policy.max_lease_minutes = state
            .get_maximum()
            .parse()
            .map_err(|_| anyhow::anyhow!("SECURITY_POLICY_DURATION：授權上限為 1 至 60 分鐘。"))?;
        policy.rules = self
            .rules
            .iter()
            .map(|rule| Rule {
                id: rule.id.to_string(),
                pattern: rule.pattern.to_string(),
                kind: if rule.kind == 1 {
                    RuleKind::Literal
                } else {
                    RuleKind::Builtin
                },
                enabled: rule.enabled,
                risk: RISKS[rule.risk.clamp(0, 2) as usize],
            })
            .collect();
        policy.validate()?;
        Ok(policy)
    }

    fn read_events(&mut self, ui: &Desktop) -> Result<()> {
        let state = ui.global::<SecurityState>();
        self.event_revision = None;
        self.event_page.clear();
        state.set_can_clear(false);
        state.set_events(ModelRc::default());
        state.set_event_detail("".into());
        state.set_has_previous(false);
        state.set_has_next(false);
        state.set_event_status("安全事件尚未驗證。".into());
        state.set_recent("安全事件尚未驗證。".into());
        self.events = self.audit.load()?;
        self.event_revision = Some(self.events.last().map_or(ZERO, |row| &row.hash).into());
        let recent = self
            .events
            .iter()
            .rev()
            .take(5)
            .map(|row| label(row).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        state.set_recent(if recent.is_empty() {
            "目前沒有安全事件".into()
        } else {
            recent.into()
        });
        let filtered: Vec<_> = self
            .events
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, row)| {
                self.kind.is_none_or(|kind| row.event.kind == kind)
                    && self.risk.is_none_or(|risk| row.event.risk == risk)
            })
            .map(|(index, _)| index)
            .collect();
        let total = filtered.len();
        self.offset = self.offset.min(total.saturating_sub(1) / PAGE * PAGE);
        let last = (self.offset + PAGE).min(total);
        self.event_page
            .extend_from_slice(&filtered[self.offset..last]);
        state.set_has_previous(self.offset > 0);
        state.set_has_next(last < total);
        state.set_can_clear(!self.events.is_empty());
        state.set_event_status(format!("{total} 筆 · 事件鏈已驗證").into());
        state.set_events(ModelRc::new(VecModel::from(
            self.event_page
                .iter()
                .map(|index| StandardListViewItem::from(label(&self.events[*index])))
                .collect::<Vec<_>>(),
        )));
        Ok(())
    }

    fn test_sample(&mut self, ui: &Desktop, sample: i32) -> Result<()> {
        let policy = self.draft(ui)?;
        let input = match sample {
            0 => {
                json!({"text":"password=synthetic_password_123\nBearer synthetic_token_123456\nsk-synthetic_1234567890\nexample@example.invalid","nested":{"password":"synthetic_only"}})
            }
            1 => json!({"type":"image","data":"c3ludGhldGlj"}),
            2 => json!({"text":"一般文字 · 合成檔案列表"}),
            _ => anyhow::bail!("SECURITY_SYNTHETIC_SAMPLE"),
        };
        let preview = if !policy.enabled || !policy.response_guard {
            json!({"action":"disabled", "payload":input, "checked":false})
        } else {
            match Guard::new(&policy)?.inspect(input) {
                Review::Passed(payload) => {
                    json!({"action":"passed", "payload":payload, "checked":true})
                }
                Review::Masked { payload, findings } => {
                    json!({"action":"masked", "payload":payload, "findings":findings, "checked":true})
                }
                Review::Blocked { findings, checked } => {
                    json!({"action":"blocked", "findings":findings, "checked":checked})
                }
            }
        };
        self.audit
            .append(Event::new(Kind::SyntheticTest, Risk::Low, 1)?)?;
        self.read_events(ui)?;
        ui.global::<SecurityState>()
            .set_preview(serde_json::to_string_pretty(&preview)?.into());
        Ok(())
    }
}

pub(super) fn bind(ui: &Desktop, directory: PathBuf) -> Shared {
    let rules = Rc::new(VecModel::default());
    let authority = Arc::new(Mutex::new(Authority::new(directory.clone())));
    let editor = Rc::new(RefCell::new(Editor {
        store: Store(directory.clone()),
        audit: Audit(directory),
        authority: authority.clone(),
        revision: None,
        rules: rules.clone(),
        events: Vec::new(),
        event_revision: None,
        event_page: Vec::new(),
        kind: None,
        risk: None,
        offset: 0,
    }));
    let state = ui.global::<SecurityState>();
    state.set_rules(ModelRc::from(rules));
    state.set_event_kinds(ModelRc::new(VecModel::from(
        std::iter::once("所有事件")
            .chain(EVENT_KINDS.iter().map(|(_, label)| *label))
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    )));
    if let Err(error) = editor.borrow_mut().reload(ui) {
        feedback(ui, error);
    }
    reflect(ui, &authority);
    let weak = ui.as_weak();
    state.on_changed(move || {
        if let Some(ui) = weak.upgrade() {
            feedback(&ui, "草稿尚未儲存。");
        }
    });

    macro_rules! action {
        ($callback:ident, |$editor:ident, $ui:ident $(,$param:ident)*| $body:block) => {{
            let weak = ui.as_weak();
            let shared = editor.clone();
            state.$callback(move |$($param),*| {
                let Some($ui) = weak.upgrade() else { return; };
                let Ok(mut state) = shared.try_borrow_mut() else { return; };
                let $editor = &mut *state;
                let result: Result<()> = (|| $body)();
                if let Err(error) = result { feedback(&$ui, error); }
            });
        }};
    }
    action!(on_save, |editor, ui| {
        let revision = editor
            .revision
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("請先驗證現行政策。"))?;
        let draft = editor.draft(&ui)?;
        let outcome = editor
            .authority
            .lock()
            .map_err(|_| anyhow::anyhow!("SECURITY_UNCONFIRMED"))?
            .save(draft, revision);
        if outcome.is_err() && editor.store.load().is_err() {
            editor.revision = None;
            ui.global::<SecurityState>().set_ready(false);
            ui.global::<SecurityState>()
                .set_saved_state("政策或安全事件尚未驗證".into());
        }
        let snapshot = outcome?;
        editor.stored(&ui, snapshot);
        editor.read_events(&ui)?;
        reflect(&ui, &editor.authority);
        feedback(&ui, "安全政策已儲存。");
        Ok(())
    });
    action!(on_reload, |editor, ui| {
        editor.reload(&ui)?;
        feedback(&ui, "已載入儲存設定。");
        Ok(())
    });
    action!(on_grant, |editor, ui, minutes| {
        let minutes = minutes
            .trim()
            .parse::<u8>()
            .map_err(|_| anyhow::anyhow!("HOST_AUTH_DURATION：授權上限為 1 至 60 分鐘。"))?;
        ensure!(
            (1..=60).contains(&minutes),
            "HOST_AUTH_DURATION：授權上限為 1 至 60 分鐘。"
        );
        let target = editor
            .authority
            .lock()
            .map_err(|_| anyhow::anyhow!("SECURITY_UNCONFIRMED"))?
            .grant_target()?;
        let confirmed = rfd::MessageDialog::new()
            .set_title("本機主機授權")
            .set_description(format!("授權目前的主機控制連線執行受信任命令 {minutes} 分鐘？主機命令使用目前 Windows 帳戶權限。"))
            .set_level(rfd::MessageLevel::Warning)
            .set_buttons(rfd::MessageButtons::YesNo)
            .show() == rfd::MessageDialogResult::Yes;
        if confirmed {
            editor
                .authority
                .lock()
                .map_err(|_| anyhow::anyhow!("SECURITY_UNCONFIRMED"))?
                .grant(minutes, target)?;
            reflect(&ui, &editor.authority);
            editor.read_events(&ui)?;
        }
        Ok(())
    });
    action!(on_revoke, |editor, ui| {
        editor
            .authority
            .lock()
            .map_err(|_| anyhow::anyhow!("SECURITY_UNCONFIRMED"))?
            .revoke()?;
        reflect(&ui, &editor.authority);
        editor.read_events(&ui)
    });
    action!(on_recover, |editor, ui| {
        editor
            .authority
            .lock()
            .map_err(|_| anyhow::anyhow!("SECURITY_UNCONFIRMED"))?
            .recover()?;
        reflect(&ui, &editor.authority);
        editor.read_events(&ui)?;
        feedback(&ui, "已發佈復原狀態，等待執行層確認。");
        Ok(())
    });
    action!(on_defaults, |editor, ui| {
        editor.hydrate(&ui, &Policy::default());
        feedback(&ui, "預設草稿尚未儲存。");
        Ok(())
    });
    action!(on_edit_rule, |editor, ui, index, field, value| {
        if let Some(mut rule) = editor.rules.row_data(index as usize) {
            match field {
                0 => rule.enabled = value == "1",
                1 => rule.id = value,
                2 => rule.kind = i32::from(value == "1"),
                3 => rule.pattern = value,
                4 => rule.risk = value.parse::<i32>().unwrap_or(0).clamp(0, 2),
                _ => return Ok(()),
            }
            editor.rules.set_row_data(index as usize, rule);
            feedback(&ui, "草稿尚未儲存。");
        }
        Ok(())
    });
    action!(on_add_rule, |editor, ui| {
        ensure!(editor.rules.row_count() < 32, "最多 32 筆規則。");
        let mut random = [0u8; 4];
        getrandom::fill(&mut random)?;
        editor.rules.push(SecurityRule {
            id: format!("custom_{}", hex(&random)).into(),
            kind: 1,
            enabled: true,
            ..Default::default()
        });
        feedback(&ui, "草稿尚未儲存。");
        Ok(())
    });
    action!(on_remove_rule, |editor, ui, index| {
        if editor.rules.row_count() > 1 && (index as usize) < editor.rules.row_count() {
            editor.rules.remove(index as usize);
            feedback(&ui, "草稿尚未儲存。");
        }
        Ok(())
    });
    action!(on_test_sample, |editor, ui, sample| {
        editor.test_sample(&ui, sample)?;
        feedback(&ui, "合成測試完成。");
        Ok(())
    });
    action!(on_filter_events, |editor, ui, kind, risk| {
        editor.kind = EVENT_KINDS.get((kind - 1) as usize).map(|entry| entry.0);
        editor.risk = RISKS.get((risk - 1) as usize).copied();
        editor.offset = 0;
        editor.read_events(&ui)
    });
    action!(on_select_event, |editor, ui, index| {
        let detail = editor
            .event_page
            .get(index as usize)
            .and_then(|index| editor.events.get(*index))
            .map(serde_json::to_string_pretty)
            .transpose()?
            .unwrap_or_default();
        ui.global::<SecurityState>().set_event_detail(detail.into());
        Ok(())
    });
    action!(on_page_events, |editor, ui, step| {
        editor.offset = if step < 0 {
            editor.offset.saturating_sub(PAGE)
        } else {
            editor.offset + PAGE
        };
        editor.read_events(&ui)
    });
    action!(on_clear_events, |editor, ui| {
        let revision = editor
            .event_revision
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("請先驗證安全事件。"))?;
        editor.audit.clear(
            revision,
            &ui.global::<SecurityState>().get_clear_confirmation(),
        )?;
        ui.global::<SecurityState>()
            .set_clear_confirmation("".into());
        editor.offset = 0;
        editor.read_events(&ui)?;
        feedback(&ui, "已清除安全事件並保留清除紀錄。");
        Ok(())
    });
    action!(on_export_events, |editor, ui| {
        let bytes = serde_json::to_vec_pretty(&editor.audit.export()?)?;
        if let Some(path) = rfd::FileDialog::new()
            .set_file_name("security-audit.json")
            .add_filter("JSON", &["json"])
            .save_file()
        {
            let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            feedback(&ui, "已匯出安全事件。");
        }
        Ok(())
    });
    authority
}

pub(super) fn reflect(ui: &Desktop, authority: &Shared) {
    let Ok(authority) = authority.try_lock() else {
        return;
    };
    let view = authority.view();
    drop(authority);
    let state = ui.global::<SecurityState>();
    state.set_effective(
        match view.effective {
            Effective::NoClient => "尚無受管理連線",
            Effective::Waiting => "等待執行層確認",
            Effective::Confirmed => "執行層已確認套用政策",
            Effective::Fault => "安全通訊異常，授權已撤銷",
        }
        .into(),
    );
    state.set_can_grant(view.can_grant);
    state.set_can_recover(view.can_recover);
    let remaining = view.lease.remaining();
    state.set_can_revoke(remaining != 0);
    state.set_lease_status(if remaining == 0 {
        if view
            .reports
            .iter()
            .any(|report| report.host_authorized && report.generation < view.lease.generation)
        {
            "已發佈撤銷，等待執行層確認".into()
        } else {
            "未授權".into()
        }
    } else if view.effective == Effective::Confirmed
        && view.reports.iter().any(|report| report.host_authorized)
    {
        format!("授權已確認，剩餘 {remaining} 秒").into()
    } else {
        format!("等待授權確認，剩餘 {remaining} 秒").into()
    });
    let details = view
        .reports
        .iter()
        .map(|report| {
            let mode = match report.mode {
                Mode::ReadOnly => "唯讀",
                Mode::DeveloperControl => "開發控制",
                Mode::HostControl => "主機控制",
            };
            let status = if report.status != Status::Closed
                && !channel::fresh(report.time_ms, channel::FRESH_MS)
            {
                "逾時"
            } else {
                match report.status {
                    Status::Attaching => "連線中",
                    Status::Ready => "就緒",
                    Status::Fault => "異常",
                    Status::Closed => "已關閉",
                }
            };
            let stats = report.counters;
            format!(
                "{mode} · {status} · 放行 {} · 遮罩 {} · 阻擋 {} · 事件缺口 {}",
                stats.passed, stats.masked, stats.blocked, report.lost_events
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    state.set_runtime_details(details.into());
}
