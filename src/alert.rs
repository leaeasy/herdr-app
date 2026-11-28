//! Alerts outside the window: a system notification. The toast and the sounds
//! stay in window.rs.

use gpui::SystemNotification;
use serde_json::Value;

use crate::model::Alert;
use crate::settings::SystemNotifications;

/// Linux launcher urgency includes completion; preserve other platforms' Dock behavior.
pub fn should_request_attention(alert: Alert, app_active: bool, dock_bounce: bool) -> bool {
    !app_active && dock_bounce && (cfg!(target_os = "linux") || alert.needs_you())
}

/// Extra reply retrieval is opt-in and never replaces an existing status message.
pub fn should_read_agent_output(enabled: bool, app: Option<&str>, alert: Alert, message: &str, address: &str) -> bool {
    cfg!(target_os = "linux") && enabled && app == Some("opencode") && alert == Alert::Done && message.is_empty() && !address.contains('/')
}

/// The adapter owns OpenCode's connection discovery and message format, outside
/// the window model. Only Linux currently opts into this optional enrichment.
pub struct AgentOutput {
    pub text: String,
    pub duration_ms: Option<u64>,
    pub model: Option<String>,
    pub tokens_per_second: Option<f64>,
}

impl AgentOutput {
    fn statistics(&self) -> String {
        let mut parts = Vec::new();
        if let Some(model) = &self.model {
            let display = model.strip_prefix("gpt-").map(|suffix| {
                let mut words = suffix.split('-');
                let version = words.next().unwrap_or_default();
                let rest = words.map(|word| {
                    let mut chars = word.chars();
                    chars.next().map(|first| first.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
                }).collect::<Vec<_>>().join(" ");
                if rest.is_empty() { format!("GPT-{version}") } else { format!("GPT-{version} {rest}") }
            }).unwrap_or_else(|| model.clone());
            parts.push(display);
        }
        if let Some(ms) = self.duration_ms {
            parts.push(format!("{:.1}s", ms as f64 / 1000.0));
        }
        if let Some(rate) = self.tokens_per_second {
            parts.push(format!("{rate:.1} tok/s"));
        }
        parts.join(" · ")
    }
}

pub fn opencode_output(terminal: &str, session: &str) -> Option<AgentOutput> {
    let output = crate::spawn::detached_command("python3")
        .args(["-c", include_str!("../scripts/opencode-output.py"), terminal, "--completed-session", session])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value: Value = serde_json::from_slice(&output.stdout).ok()?;
    if value.get("status")?.as_str()? != "ok" {
        return None;
    }
    let text = value.get("text")?.as_str().filter(|s| !s.trim().is_empty())?.to_owned();
    Some(AgentOutput {
        text,
        duration_ms: value.get("duration_ms").and_then(Value::as_u64),
        model: value.get("model").and_then(Value::as_str)
            .map(|model| model.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|model| !model.is_empty()),
        tokens_per_second: value.get("tokens_per_second").and_then(Value::as_f64)
            .filter(|rate| rate.is_finite() && *rate >= 0.0),
    })
}

/// Exactly one session record is safe to bind to a terminal's completion.
pub fn opencode_session(record: &Value) -> Option<String> {
    let mut sessions = record.get("records")?.as_object()?.iter()
        .filter(|(id, value)| id.starts_with("ses") && value.get("app").and_then(Value::as_str) == Some("opencode"))
        .map(|(id, _)| id.clone());
    let session = sessions.next()?;
    sessions.next().is_none().then_some(session)
}

/// A blocked terminal snapshot, rechecked before sending Enter or Escape.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub struct AgentInput(Value);

impl AgentInput {
    #[cfg(test)]
    pub fn read(_address: &str) -> Option<Self> { None }

    #[cfg(not(test))]
    pub fn read(address: &str) -> Option<Self> {
        Self::read_terminal(address)
    }

    fn read_terminal(address: &str) -> Option<Self> {
        let output = crate::spawn::detached_command("hc")
            .args(["get", address, "--json"])
            .stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .output().ok()?;
        if !output.status.success() { return None; }
        let record: Value = serde_json::from_slice(&output.stdout).ok()?;
        Some(Self(serde_json::json!({"terminal": address, "binding": blocked_binding(&record)?})))
    }

    pub fn notification(&self, mut note: SystemNotification, generation: u64) -> SystemNotification {
        note.actions = ["确认", "取消"].into_iter().enumerate().map(|(index, label)| {
            gpui::SystemNotificationAction {
                id: format!("agent-input:{generation}:{index}").into(),
                label: label.into(),
            }
        }).collect();
        note
    }

    #[cfg(test)]
    pub fn reply(&self, _index: usize) -> bool { true }

    #[cfg(not(test))]
    pub fn reply(&self, index: usize) -> bool {
        self.send_key(index)
    }

    fn send_key(&self, index: usize) -> bool {
        let (Some(address), Some(key)) = (self.0["terminal"].as_str(), input_key(index)) else { return false };
        let Some(current) = Self::read_terminal(address) else { return false };
        if current.0["binding"] != self.0["binding"] { return false; }
        crate::spawn::detached_command("hc")
            .args(["keys", address, key, "--json"])
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().is_ok_and(|status| status.success())
    }
}

fn blocked_binding(record: &Value) -> Option<Value> {
    let status = record.get("status")?;
    if status.get("state").and_then(Value::as_str).or_else(|| status.as_str()) != Some("blocked") {
        return None;
    }
    Some(serde_json::json!({
        "status": status, "turn": record.get("turn"), "records": record.get("records"),
    }))
}

pub fn input_key(index: usize) -> Option<&'static str> {
    match index { 0 => Some("enter"), 1 => Some("esc"), _ => None }
}

pub fn input_action(action: &str) -> Option<(u64, usize)> {
    let (generation, index) = action.strip_prefix("agent-input:")?.split_once(':')?;
    let index = index.parse().ok()?;
    input_key(index)?;
    Some((generation.parse().ok()?, index))
}

/// One alert, as the system notification shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub address: String,
    pub alert: Alert,
    pub name: String,
    /// "space / tab / pane title" (without the last part when unnamed).
    pub place: Option<String>,
    /// The agent's own summary from its 7501 report (`msg`), e.g. "approve
    /// Bash: make"; empty when it said nothing.
    pub message: String,
}

impl Note {
    pub fn title(&self) -> String {
        let place = notification_excerpt(self.place.as_deref().unwrap_or_default(), 1, 160);
        if place.is_empty() { self.status() } else { place }
    }

    fn status(&self) -> String {
        match self.alert {
            Alert::Blocked => format!("{} needs you", self.name),
            Alert::Error => format!("{} hit an error", self.name),
            Alert::Done => format!("{} finished", self.name),
        }
    }

    /// Status first; location is shown in the notification title instead.
    fn body(&self) -> String {
        let message = notification_excerpt(&self.message, 4, 500);
        self.body_with_message(message)
    }

    /// Retrieved replies have their own budget and an untruncated final line.
    #[cfg(test)]
    fn agent_output_body(&self, text: &str) -> String {
        self.agent_output_body_with_statistics(text, "")
    }

    pub fn agent_output_notification_body(&self, output: &AgentOutput) -> String {
        self.agent_output_body_with_statistics(&output.text, &output.statistics())
    }

    fn agent_output_body_with_statistics(&self, text: &str, statistics: &str) -> String {
        // Emit only the small markup subset supported by desktop notifications.
        let message = agent_output_excerpt(text);
        let message = notification_markdown(&message);
        let status = if message.is_empty() { escape_notification_markup(&self.status()) } else { String::new() };
        [status, message, escape_notification_markup(statistics)].into_iter().filter(|line| !line.is_empty()).collect::<Vec<_>>().join("\n")
    }

    fn body_with_message(&self, message: String) -> String {
        [self.status(), message].into_iter().filter(|line| !line.is_empty()).collect::<Vec<_>>().join("\n")
    }
}

fn escape_notification_markup(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Markdown is parsed, never passed through as arbitrary HTML. Notification
/// daemons are not browsers: use b/i and newlines, with code and links as text.
fn notification_markdown(text: &str) -> String {
    use markdown::mdast::Node;
    fn render(node: &Node) -> String {
        let children = || node.children().map(|children| children.iter().map(render).collect::<String>()).unwrap_or_default();
        match node {
            Node::Text(n) => escape_notification_markup(&n.value),
            Node::InlineCode(n) => escape_notification_markup(&n.value),
            Node::Code(n) => format!("{}\n", escape_notification_markup(&n.value)),
            Node::Html(n) => escape_notification_markup(&n.value),
            Node::Image(n) => escape_notification_markup(&n.alt),
            Node::ImageReference(n) => escape_notification_markup(&n.alt),
            Node::Strong(_) => format!("<b>{}</b>", children()),
            Node::Emphasis(_) => format!("<i>{}</i>", children()),
            Node::Heading(_) => format!("<b>{}</b>\n", children()),
            Node::Paragraph(_) | Node::Blockquote(_) => format!("{}\n", children()),
            Node::ListItem(_) => format!("• {}\n", children().trim_end()),
            Node::Break(_) | Node::ThematicBreak(_) => "\n".into(),
            _ => children(),
        }
    }
    markdown::to_mdast(text, &markdown::ParseOptions::default())
        .map(|root| render(&root).trim_end().to_owned())
        .unwrap_or_else(|_| escape_notification_markup(text))
}

/// Summarize a retrieved reply in Herdr, leaving adapters' output untouched.
/// The first three non-empty lines share 200 characters; the last stays whole.
pub fn agent_output_excerpt(text: &str) -> String {
    let lines: Vec<_> = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    let Some(last) = lines.last() else { return String::new() };
    // Do not duplicate the final line when the reply has fewer than four lines.
    let head = lines[..lines.len() - 1].iter().take(3).copied().collect::<Vec<_>>().join("\n");
    let omitted = lines.len() > 4;
    let head_len = head.chars().count();
    let head = if head_len > 200 || omitted {
        format!("{}…", head.chars().take(199).collect::<String>())
    } else {
        head
    };
    if head.is_empty() { (*last).to_owned() } else { format!("{head}\n{last}") }
}

/// Bound plain-text notification content without slicing UTF-8 bytes.
fn notification_excerpt(text: &str, max_lines: usize, max_chars: usize) -> String {
    let mut result = String::new();
    let mut chars = 0;
    let mut lines = 0;
    for line in text.trim().lines().map(str::trim).filter(|line| !line.is_empty()) {
        if lines == max_lines {
            result.push('…');
            return result;
        }
        if lines > 0 {
            result.push('\n');
        }
        for ch in line.chars() {
            if chars == max_chars {
                result.push('…');
                return result;
            }
            result.push(ch);
            chars += 1;
        }
        lines += 1;
    }
    result
}

/// The `msg` of a terminal's 7501 record in the core's listing.
pub fn message_of(report: Option<&Value>) -> String {
    report.and_then(|r| r.get("msg")).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// Whether an alert leaves the window. `app_active`: Herdr's window is key in
/// the app in front. `looking`: that agent is on screen in it.
pub fn should_notify(mode: SystemNotifications, app_active: bool, looking: bool) -> bool {
    match mode {
        SystemNotifications::Off => false,
        _ if looking => false,
        SystemNotifications::Background => !app_active,
        SystemNotifications::Always => true,
    }
}

const TAG_PREFIX: &str = "herdr-agent:";

/// One notification per agent: a newer alert replaces the older one.
pub fn tag(address: &str) -> String {
    format!("{TAG_PREFIX}{address}")
}

pub fn address_of(tag: &str) -> Option<&str> {
    tag.strip_prefix(TAG_PREFIX).filter(|a| !a.is_empty())
}

pub fn system_notification(note: &Note) -> SystemNotification {
    SystemNotification {
        tag: tag(&note.address).into(),
        title: note.title().into(),
        body: note.body().into(),
        icon: None,
        actions: Vec::new(),
    }
}

/// Use the same program table (including personal overrides) as panes and mods.
/// Linux notification daemons read icons from disk, not the mod resource loader.
#[cfg(target_os = "linux")]
pub fn notification_icon(app: Option<&str>) -> Option<gpui::SharedString> {
    let app = app.map(str::trim).filter(|app| !app.is_empty())?;
    let program = crate::programs::of(Some(app), &[]);
    notification_icon_path(&program, &crate::mods::app_dir())
}

#[cfg(target_os = "linux")]
fn notification_icon_path(program: &Value, home: &std::path::Path) -> Option<gpui::SharedString> {
    let icon = program.get("icon").and_then(Value::as_str).filter(|icon| !icon.is_empty())?;
    let path = home.join(icon);
    path.is_file().then(|| path.to_string_lossy().into_owned().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_input_buttons_bind_generation_to_enter_and_escape() {
        let input = AgentInput(serde_json::json!({}));
        let original = system_notification(&note(Alert::Blocked));
        let notification = input.notification(original.clone(), 42);
        assert_eq!(notification.body, original.body);
        assert_eq!(notification.actions.len(), 2);
        assert_eq!(notification.actions[0].label.as_ref(), "确认");
        assert_eq!(notification.actions[1].label.as_ref(), "取消");
        assert_eq!(input_action(&notification.actions[1].id), Some((42, 1)));
        assert_eq!(input_key(0), Some("enter"));
        assert_eq!(input_key(1), Some("esc"));
        for invalid in ["default", "agent-input:42", "agent-input:42:1:2", "agent-input:x:0", "agent-input:1:-1", "agent-input:42:2"] {
            assert_eq!(input_action(invalid), None);
        }
    }

    #[test]
    fn notification_input_binding_rejects_resolved_requests_and_tracks_turns() {
        let record = serde_json::json!({"status": {"state": "blocked", "app": "codex"}, "turn": 1});
        let snapshot = blocked_binding(&record).unwrap();
        let mut changed = record.clone();
        changed["turn"] = 2.into();
        assert_ne!(blocked_binding(&changed).unwrap(), snapshot);
        for state in ["working", "idle", "done", "error"] {
            changed["status"]["state"] = state.into();
            assert!(blocked_binding(&changed).is_none());
        }
    }

    #[test]
    #[ignore = "requires an isolated test core and scratch XDG_STATE_HOME"]
    fn notification_input_delivers_only_enter_and_escape() {
        use std::{process::Command, time::{Duration, Instant}};
        assert!(std::env::var("HC_SESSION").unwrap().starts_with("test-"));
        let state = std::path::PathBuf::from(std::env::var("XDG_STATE_HOME").unwrap());
        assert!(state.starts_with("/tmp"));
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/notification-agent.py");
        for (index, expected) in [(0, "0d"), (1, "1b")] {
            let result = state.join(format!("key-{index}.json"));
            let output = Command::new("hc").args(["run", "--bg", "--no-parent", "--json", "python3"])
                .arg(&fixture).args(["--delay", "0.1", "--timeout", "30", "--result"])
                .arg(&result).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            let started: Value = serde_json::from_slice(&output.stdout).unwrap();
            let address = started["address"].as_str().unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            let input = loop {
                if let Some(input) = AgentInput::read_terminal(address) { break input; }
                assert!(Instant::now() < deadline, "fixture never reported blocked");
                std::thread::sleep(Duration::from_millis(100));
            };
            assert!(!result.exists(), "input was sent without a button click");
            assert!(input.send_key(index));
            let deadline = Instant::now() + Duration::from_secs(10);
            while !result.exists() {
                assert!(Instant::now() < deadline, "fixture received no key");
                std::thread::sleep(Duration::from_millis(100));
            }
            let received: Value = serde_json::from_slice(&std::fs::read(&result).unwrap()).unwrap();
            assert_eq!(received["received_hex"], expected);
            let deadline = Instant::now() + Duration::from_secs(5);
            while AgentInput::read_terminal(address).is_some() {
                assert!(Instant::now() < deadline, "fixture still blocked after reply");
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(!input.send_key(index), "resolved request accepted a stale click");
        }
    }

    #[test]
    fn opencode_session_requires_one_matching_record() {
        let record = serde_json::json!({"records": {
            "ses_exact": {"app": "opencode"},
            "other": {"app": "claude-code"}
        }});
        assert_eq!(opencode_session(&record).as_deref(), Some("ses_exact"));
        assert_eq!(opencode_session(&serde_json::json!({"records": {
            "ses_a": {"app": "opencode"}, "ses_b": {"app": "opencode"}
        }})), None);
        assert_eq!(opencode_session(&serde_json::json!({"records": {}})), None);
        assert_eq!(opencode_session(&Value::Null), None);
    }
    use crate::settings::Person;

    fn note(alert: Alert) -> Note {
        Note { address: "local:t7".into(), alert, name: "Claude".into(), place: Some("herdr / main".into()), message: String::new() }
    }

    #[test]
    fn notifies_only_when_away_unless_always_and_never_while_looking() {
        use SystemNotifications::*;
        let cases = [
            // mode, app_active, looking, notify
            (Background, false, false, true),
            (Background, true, false, false),
            (Background, false, true, false),
            (Always, true, false, true),
            (Always, false, false, true),
            (Always, true, true, false),
            (Off, false, false, false),
            (Off, true, false, false),
        ];
        for (mode, active, looking, want) in cases {
            assert_eq!(should_notify(mode, active, looking), want, "{mode:?} active={active} looking={looking}");
        }
    }

    #[test]
    fn the_setting_parses_and_stays_out_of_settings_json_by_default() {
        let person: Person = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!((person.system_notifications, person.dock_bounce, person.notification_agent_output), (SystemNotifications::Background, false, false));
        let saved = serde_json::to_value(&person).unwrap();
        for key in ["system_notifications", "dock_bounce", "notification_agent_output"] {
            assert!(saved.get(key).is_none(), "{key} written at its default");
        }
        for (word, mode) in [("always", SystemNotifications::Always), ("off", SystemNotifications::Off), ("background", SystemNotifications::Background)] {
            let person: Person = serde_json::from_value(serde_json::json!({ "system_notifications": word, "dock_bounce": true })).unwrap();
            assert_eq!((person.system_notifications, person.dock_bounce), (mode, true));
        }
    }

    #[test]
    fn attention_uses_one_background_only_preference() {
        for active in [false, true] {
            for enabled in [false, true] {
                for alert in [Alert::Done, Alert::Blocked, Alert::Error] {
                    let want = !active && enabled && (cfg!(target_os = "linux") || alert != Alert::Done);
                    assert_eq!(should_request_attention(alert, active, enabled), want);
                }
            }
        }
    }

    #[test]
    fn attention_preference_round_trips_and_composes() {
        assert!(!crate::settings::Settings::default().dock_bounce);
        for enabled in [false, true] {
            let person: Person = serde_json::from_value(serde_json::json!({
                "system_notifications": "off", "dock_bounce": enabled,
            })).unwrap();
            let saved = serde_json::to_value(&person).unwrap();
            assert_eq!(saved.get("dock_bounce").and_then(Value::as_bool), enabled.then_some(true));
            let loaded: Person = serde_json::from_value(saved).unwrap();
            assert_eq!(loaded.dock_bounce, enabled);
            assert_eq!(crate::settings::Settings::compose(&loaded, &crate::layout::Look::default(), true).dock_bounce, enabled);
        }
    }

    #[test]
    fn reply_retrieval_requires_opt_in_and_a_supported_completion_without_a_message() {
        assert_eq!(should_read_agent_output(true, Some("opencode"), Alert::Done, "", "t1"), cfg!(target_os = "linux"));
        for (enabled, app, alert, message, address) in [
            (false, Some("opencode"), Alert::Done, "", "t1"),
            (true, Some("codex"), Alert::Done, "", "t1"),
            (true, None, Alert::Done, "", "t1"),
            (true, Some("opencode"), Alert::Blocked, "", "t1"),
            (true, Some("opencode"), Alert::Error, "", "t1"),
            (true, Some("opencode"), Alert::Done, "already reported", "t1"),
            (true, Some("opencode"), Alert::Done, "", "remote/t1"),
        ] {
            assert!(!should_read_agent_output(enabled, app, alert, message, address));
        }
    }

    #[test]
    fn reply_preference_round_trips_and_survives_disabled_system_notifications() {
        for mode in ["background", "always", "off"] {
            for enabled in [false, true] {
                let person: Person = serde_json::from_value(serde_json::json!({
                    "system_notifications": mode, "notification_agent_output": enabled,
                })).unwrap();
                let saved = serde_json::to_value(&person).unwrap();
                assert_eq!(saved.get("notification_agent_output"), enabled.then_some(&serde_json::Value::Bool(true)));
                let loaded: Person = serde_json::from_value(saved).unwrap();
                assert_eq!(loaded.notification_agent_output, enabled);
                assert_eq!(crate::settings::Settings::compose(&loaded, &crate::layout::Look::default(), true).notification_agent_output, enabled);
            }
        }
        assert!(!crate::settings::Settings::default().notification_agent_output);
    }

    #[test]
    fn the_notification_says_what_the_toast_says_tagged_by_agent() {
        let n = system_notification(&note(Alert::Blocked));
        assert_eq!((n.tag.as_ref(), n.title.as_ref(), n.body.as_ref()), ("herdr-agent:local:t7", "herdr / main", "Claude needs you"));
        let done = system_notification(&Note { place: None, ..note(Alert::Done) });
        assert_eq!((done.title.as_ref(), done.body.as_ref()), ("Claude finished", "Claude finished"));
        assert_eq!(address_of(&n.tag), Some("local:t7"));
        assert_eq!(address_of("herdr-agent:"), None);
        assert_eq!(address_of("other:local:t7"), None);
    }

    #[test]
    fn notifications_have_no_explicit_action_buttons() {
        let n = system_notification(&note(Alert::Blocked));
        assert!(n.actions.is_empty());
    }

    #[test]
    fn notification_body_keeps_a_multiline_summary_and_location() {
        let n = Note { message: "  Approval needed\n\n  npm run deploy  \r\nproduction ".into(), ..note(Alert::Blocked) };
        assert_eq!(n.body(), "Claude needs you\nApproval needed\nnpm run deploy\nproduction");
        let empty = Note { message: " \n ".into(), place: Some("work / main / Login fix".into()), ..note(Alert::Done) };
        assert_eq!(empty.title(), "work / main / Login fix");
        assert_eq!(empty.body(), "Claude finished");
    }

    #[test]
    fn notification_body_bounds_summary_without_losing_location() {
        let n = Note { message: "一".repeat(501), ..note(Alert::Blocked) };
        assert_eq!(n.body(), format!("Claude needs you\n{}…", "一".repeat(500)));
        let n = Note { message: "one\ntwo\nthree\nfour\nfive".into(), ..note(Alert::Done) };
        assert_eq!(n.body(), "Claude finished\none\ntwo\nthree\nfour…");
        let n = Note { place: Some("界".repeat(161)), ..note(Alert::Done) };
        assert_eq!(n.title(), format!("{}…", "界".repeat(160)));
        assert_eq!(n.body(), "Claude finished");
        assert_eq!(notification_excerpt(&"x".repeat(500), 4, 500), "x".repeat(500));
        assert_eq!(notification_excerpt("", 4, 500), "");
    }

    #[test]
    fn retrieved_replies_bound_the_first_three_lines_and_keep_the_last_whole() {
        assert_eq!(agent_output_excerpt(" \n "), "");
        assert_eq!(agent_output_excerpt("  one \r\n"), "one");
        assert_eq!(agent_output_excerpt("first\n\nlast"), "first\nlast");
        assert_eq!(agent_output_excerpt(" first \n middle\nother\n last "), "first\nmiddle\nother\nlast");
        assert_eq!(agent_output_excerpt("one\ntwo\nthree\nomitted\nlast"), "one\ntwo\nthree…\nlast");
        let long = agent_output_excerpt(&format!("{}\nmiddle\nthird\n{}", "首".repeat(600), "尾".repeat(600)));
        let (head, last) = long.rsplit_once('\n').unwrap();
        assert_eq!(head.chars().count(), 200);
        assert!(head.ends_with('…'));
        assert_eq!(last, "尾".repeat(600));
        let single = agent_output_excerpt(&"界".repeat(501));
        assert_eq!(single, "界".repeat(501));
        let short_end = agent_output_excerpt(&format!("{}\nlast", "x".repeat(600)));
        assert_eq!(short_end.chars().count(), 205);
        assert!(short_end.ends_with("\nlast"));
        let short_start = agent_output_excerpt(&format!("first\n{}", "y".repeat(600)));
        assert_eq!(short_start, format!("first\n{}", "y".repeat(600)));
        assert_eq!(agent_output_excerpt(&format!("{}\nlast", "x".repeat(200))), format!("{}\nlast", "x".repeat(200)));
    }

    #[test]
    fn retrieved_reply_body_does_not_retruncate_the_final_line() {
        let n = note(Alert::Done);
        let text = format!("one\ntwo\nthree\nskipped\n{}", "尾".repeat(700));
        assert_eq!(n.agent_output_body(&text), format!("one\ntwo\nthree…\n{}", "尾".repeat(700)));
    }

    #[test]
    fn retrieved_reply_renders_markdown_and_escapes_raw_html() {
        let n = Note { place: Some("space <main> & pane".into()), ..note(Alert::Done) };
        assert_eq!(n.title(), "space <main> & pane");
        assert_eq!(n.agent_output_body("<b>done</b> & safe"), "&lt;b&gt;done&lt;/b&gt; &amp; safe");
        assert_eq!(n.agent_output_body("**done** and *safe*"), "<b>done</b> and <i>safe</i>");
        assert_eq!(notification_markdown("# Heading\n\n- `code`\n- [label](https://example.com)"), "<b>Heading</b>\n• code\n• label");
        assert_eq!(n.agent_output_body(" \n"), "Claude finished");
    }

    #[test]
    fn retrieved_reply_statistics_are_plain_and_optional() {
        let n = note(Alert::Done);
        let mut output = AgentOutput { text: "done".into(), duration_ms: Some(31_900), model: Some("gpt-6.1-sol".into()), tokens_per_second: Some(50.7) };
        assert_eq!(n.agent_output_notification_body(&output), "done\nGPT-6.1 Sol · 31.9s · 50.7 tok/s");
        output.model = Some("test<&>".into());
        assert!(n.agent_output_notification_body(&output).contains("test&lt;&amp;&gt; ·"));
        output.model = None;
        output.duration_ms = None;
        assert_eq!(output.statistics(), "50.7 tok/s");
        output.tokens_per_second = None;
        assert_eq!(n.agent_output_notification_body(&output), "done");
        output.duration_ms = Some(450);
        assert_eq!(output.statistics(), "0.5s");
    }

    #[test]
    fn the_agents_own_message_goes_over_the_place() {
        let quoted = "approve Bash: echo \"$HOME\" 'x' $(id); `id`";
        let n = Note { message: quoted.into(), ..note(Alert::Blocked) };
        assert_eq!(system_notification(&n).body.as_ref(), format!("Claude needs you\n{quoted}"));
        let alone = Note { place: None, message: "  done: 3 files  ".into(), ..note(Alert::Done) };
        assert_eq!(system_notification(&alone).body.as_ref(), "Claude finished\ndone: 3 files");
        let report = serde_json::json!({ "state": "blocked", "kind": "question", "title": "t", "msg": "which one?" });
        assert_eq!(message_of(Some(&report)), "which one?");
        assert_eq!(message_of(Some(&serde_json::json!("blocked"))), "", "a bare word says nothing more");
        assert_eq!(message_of(None), "");
    }

    #[test]
    fn an_error_is_told_apart_from_blocked() {
        assert_eq!(note(Alert::Error).status(), "Claude hit an error");
        assert!(Alert::Error.needs_you() && Alert::Blocked.needs_you() && !Alert::Done.needs_you());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_icons_follow_the_program_table_and_require_a_local_file() {
        let programs = crate::programs::startable();
        assert!(!programs.is_empty());
        for program in programs {
            let app = program["key"].as_str().unwrap();
            let expected = program["icon"].as_str().filter(|icon| !icon.is_empty()).and_then(|icon| {
                let path = crate::mods::app_dir().join(icon);
                path.is_file().then(|| gpui::SharedString::from(path.to_string_lossy().into_owned()))
            });
            assert_eq!(notification_icon(Some(app)), expected, "{app}");
        }
        assert_eq!(notification_icon(Some("claude-code")), notification_icon(Some("claude")), "program aliases use the same mapping");
        assert_eq!(notification_icon(None), None);
        assert_eq!(notification_icon(Some("  ")), None);
        assert_eq!(system_notification(&note(Alert::Blocked)).icon, None, "other platforms retain the default");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_icon_paths_support_custom_files_and_skip_missing_files() {
        use serde_json::json;

        let home = std::env::temp_dir().join(format!(
            "herdr-notification-icons-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let icon = home.join("custom agent.svg");
        std::fs::write(&icon, r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#).unwrap();
        let expected = Some(gpui::SharedString::from(icon.to_string_lossy().into_owned()));
        assert_eq!(notification_icon_path(&json!({ "icon": "custom agent.svg" }), &home), expected);
        assert_eq!(notification_icon_path(&json!({ "icon": icon }), &home), expected);
        for program in [json!({}), json!({ "icon": null }), json!({ "icon": "" }), json!({ "icon": "missing.svg" }), json!({ "icon": "." })] {
            assert_eq!(notification_icon_path(&program, &home), None, "{program}");
        }
        std::fs::remove_file(&icon).unwrap();
        assert_eq!(notification_icon_path(&json!({ "icon": "custom agent.svg" }), &home), None);
        std::fs::remove_dir(&home).unwrap();
    }
}
