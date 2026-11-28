//! System notifications over the XDG notifications D-Bus interface.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    ForegroundExecutor, SharedString, SystemNotification, SystemNotificationResponse, Task,
};

/// The XDG action key invoked when the user activates the notification body
/// rather than a specific action button.
const DEFAULT_ACTION: &str = "default";

type ResponseCallback = Rc<RefCell<Option<Box<dyn FnMut(SystemNotificationResponse)>>>>;

struct Response {
    notification: SystemNotificationResponse,
    activation_token: Option<String>,
}

enum InteractiveState {
    Pending,
    Shown(zbus::blocking::Connection, u32),
    Cancelled,
}

struct InteractiveNotification(Mutex<InteractiveState>);

impl InteractiveNotification {
    fn cancel(&self) {
        let old = std::mem::replace(&mut *self.0.lock().unwrap(), InteractiveState::Cancelled);
        if let InteractiveState::Shown(connection, id) = old {
            // D-Bus must stay off the foreground thread, including dismissal.
            let _ = std::thread::Builder::new().name("close-notification".into())
                .spawn(move || close_notification(&connection, id));
        }
    }

    fn cancelled(&self) -> bool {
        matches!(*self.0.lock().unwrap(), InteractiveState::Cancelled)
    }

    fn shown(&self, connection: &zbus::blocking::Connection, id: u32) -> bool {
        let mut state = self.0.lock().unwrap();
        if matches!(*state, InteractiveState::Cancelled) {
            drop(state);
            close_notification(connection, id);
            return false;
        }
        *state = InteractiveState::Shown(connection.clone(), id);
        true
    }
}

fn close_notification(connection: &zbus::blocking::Connection, id: u32) {
    if let Ok(proxy) = zbus::blocking::Proxy::new(connection, "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications", "org.freedesktop.Notifications") {
        // The daemon may already have closed it after an action or manual dismissal.
        let _ = proxy.call::<_, _, ()>("CloseNotification", &(id,));
    }
}

pub(crate) struct SystemNotificationState {
    response_sender: mpsc::UnboundedSender<Response>,
    response_receiver: Option<mpsc::UnboundedReceiver<Response>>,
    activation_token: Rc<RefCell<Option<String>>>,
    callback: ResponseCallback,
    interactive: Arc<Mutex<HashMap<SharedString, Arc<InteractiveNotification>>>>,
    _response_task: Option<Task<()>>,
}

impl SystemNotificationState {
    pub(crate) fn new() -> Self {
        let (response_sender, response_receiver) = mpsc::unbounded();
        Self {
            response_sender,
            response_receiver: Some(response_receiver),
            activation_token: Rc::new(RefCell::new(None)),
            callback: Rc::new(RefCell::new(None)),
            interactive: Arc::new(Mutex::new(HashMap::new())),
            _response_task: None,
        }
    }

    pub(crate) fn show(
        &self,
        app_name: Option<&str>,
        app_identifier: Option<&str>,
        notification: SystemNotification,
    ) {
        let mut builder = notify_rust::Notification::new();
        if let Some(app_name) = app_name {
            builder.appname(app_name);
        }
        if let Some(icon) = &notification.icon {
            builder.icon(icon);
        }
        builder
            .summary(&notification.title)
            .body(&notification.body)
            .action(DEFAULT_ACTION, DEFAULT_ACTION);
        let mut action_ids = HashMap::new();
        for (index, action) in notification.actions.iter().enumerate() {
            let transport_id = format!("gpui-action-{index}");
            builder.action(&transport_id, &action.label);
            action_ids.insert(transport_id, action.id.clone());
        }
        let built = builder.finalize();
        let app_identifier = app_identifier.map(str::to_owned);
        // Reply buttons belong to the current prompt, not the desktop archive.
        let transient = !notification.actions.is_empty();
        self.dismiss(&notification.tag);
        let active = self.interactive.clone();
        let interactive = transient.then(|| Arc::new(InteractiveNotification(Mutex::new(InteractiveState::Pending))));
        if let Some(entry) = &interactive {
            active.lock().unwrap().insert(notification.tag.clone(), entry.clone());
        }

        let sender = self.response_sender.clone();
        let tag = notification.tag.clone();
        // Connecting and waiting for desktop responses must not block the UI.
        let spawn_result = std::thread::Builder::new()
            .name("system-notification".to_string())
            .spawn(move || {
                let result = (|| -> anyhow::Result<()> {
                    let connection = zbus::blocking::Connection::session()?;
                    let proxy = zbus::blocking::Proxy::new(
                        &connection,
                        "org.freedesktop.Notifications",
                        "/org/freedesktop/Notifications",
                        "org.freedesktop.Notifications",
                    )?;
                    // Subscribe before showing: fast clicks must not race the listener.
                    // notify-rust's action waiter discards ActivationToken signals.
                    let signals = proxy.receive_all_signals()?;
                    // Show on the same connection: daemons may unicast action
                    // and activation signals to the sender of Notify.
                    let hints = notification_hints(app_identifier.as_deref(), transient);
                    if interactive.as_ref().is_some_and(|entry| entry.cancelled()) {
                        return Ok(());
                    }
                    let notification_id: u32 = proxy.call(
                        "Notify",
                        &(
                            &built.appname,
                            0u32,
                            &built.icon,
                            &built.summary,
                            &built.body,
                            &built.actions,
                            hints,
                            if transient { 0i32 } else { -1i32 },
                        ),
                    )?;
                    if interactive.as_ref().is_some_and(|entry| !entry.shown(&connection, notification_id)) {
                        return Ok(());
                    }
                    let mut activation_token = None;
                    for signal in signals {
                        let header = signal.header();
                        match header.member().map(|member| member.as_str()) {
                            Some("ActivationToken") => {
                                let (id, token): (u32, String) = signal.body().deserialize()?;
                                if id == notification_id {
                                    activation_token = Some(token);
                                }
                            }
                            Some("ActionInvoked") => {
                                let (id, action): (u32, String) = signal.body().deserialize()?;
                                if id != notification_id {
                                    continue;
                                }
                                let Some(action_id) = response_action_id(&action, &action_ids)
                                else {
                                    log::warn!(
                                        "system notification returned unknown action {action:?}"
                                    );
                                    continue;
                                };
                                if let Some(entry) = &interactive {
                                    if entry.cancelled() { break; }
                                    entry.cancel();
                                }
                                sender
                                    .unbounded_send(Response {
                                        notification: SystemNotificationResponse { tag: tag.clone(), action_id },
                                        activation_token,
                                    })
                                    .ok();
                                break;
                            }
                            Some("NotificationClosed") => {
                                let (id, _reason): (u32, u32) = signal.body().deserialize()?;
                                // A hidden banner can remain actionable in the
                                // desktop's history. NotificationClosed means
                                // the server has ended its lifecycle, so release
                                // the connection rather than leaking a waiter.
                                if id == notification_id {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                })();
                if let Some(entry) = &interactive {
                    let mut active = active.lock().unwrap();
                    if active.get(&tag).is_some_and(|current| Arc::ptr_eq(current, entry)) {
                        active.remove(&tag);
                    }
                }
                if let Err(error) = result {
                    log::warn!("system notification failed: {error:#}");
                }
            });
        if let Err(error) = spawn_result {
            self.dismiss(&notification.tag);
            log::warn!("failed to spawn system notification thread: {error}");
        }
    }

    pub(crate) fn dismiss(&self, tag: &str) {
        if let Some(entry) = self.interactive.lock().unwrap().remove(tag) {
            entry.cancel();
        }
    }

    pub(crate) fn take_activation_token(&self) -> Option<String> {
        self.activation_token.borrow_mut().take()
    }

    pub(crate) fn on_response(
        &mut self,
        executor: &ForegroundExecutor,
        callback: Box<dyn FnMut(SystemNotificationResponse)>,
    ) {
        *self.callback.borrow_mut() = Some(callback);

        // Responses arrive from per-notification threads; this task hands
        // them to the registered callback on the main thread.
        if let Some(mut receiver) = self.response_receiver.take() {
            let callback = self.callback.clone();
            let activation_token = self.activation_token.clone();
            self._response_task = Some(executor.spawn(async move {
                while let Some(response) = receiver.next().await {
                    // Take the callback out for the call: it may re-enter the
                    // platform or replace itself.
                    let taken = callback.borrow_mut().take();
                    if let Some(mut taken) = taken {
                        *activation_token.borrow_mut() = response.activation_token;
                        taken(response.notification);
                        // Never reuse a click's token for an unrelated activation.
                        activation_token.borrow_mut().take();
                        callback.borrow_mut().get_or_insert(taken);
                    }
                }
            }));
        }
    }
}

impl Drop for SystemNotificationState {
    fn drop(&mut self) {
        for (_, entry) in self.interactive.lock().unwrap().drain() {
            entry.cancel();
        }
    }
}

fn notification_hints(
    app_identifier: Option<&str>,
    transient: bool,
) -> HashMap<&'static str, zbus::zvariant::Value<'static>> {
    // Interactive prompts bypass desktop history. Ordinary notifications may
    // still be archived, with banner expiry controlled by the desktop.
    let mut hints = HashMap::from([
        ("transient", zbus::zvariant::Value::from(transient)),
        // XDG urgency is a D-Bus byte: 0 low, 1 normal, 2 critical.
        ("urgency", zbus::zvariant::Value::from(1u8)),
    ]);
    if let Some(identifier) = app_identifier.filter(|identifier| !identifier.trim().is_empty()) {
        hints.insert(
            "desktop-entry",
            zbus::zvariant::Value::from(identifier.to_owned()),
        );
    }
    hints
}

fn response_action_id(
    transport_id: &str,
    action_ids: &HashMap<String, SharedString>,
) -> Option<Option<SharedString>> {
    if transport_id == DEFAULT_ACTION {
        Some(None)
    } else {
        action_ids.get(transport_id).cloned().map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DeliveredNotification {
        id: u32,
        title: String,
        actions: Vec<String>,
        hints: HashMap<String, zbus::zvariant::OwnedValue>,
        expire_timeout: i32,
    }

    struct TestNotificationServer {
        delivered: std::sync::mpsc::Sender<DeliveredNotification>,
        closed: std::sync::mpsc::Sender<u32>,
        release_reply: Mutex<std::sync::mpsc::Receiver<()>>,
        next_id: u32,
    }

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl TestNotificationServer {
        fn notify(
            &mut self,
            _app_name: String,
            _replaces_id: u32,
            _icon: String,
            summary: String,
            _body: String,
            actions: Vec<String>,
            hints: HashMap<String, zbus::zvariant::OwnedValue>,
            expire_timeout: i32,
        ) -> u32 {
            self.next_id += 1;
            let hold_reply = summary == "Hold notification reply";
            self.delivered.send(DeliveredNotification {
                id: self.next_id, title: summary, actions, hints, expire_timeout,
            }).unwrap();
            if hold_reply {
                self.release_reply.lock().unwrap().recv_timeout(std::time::Duration::from_secs(3)).unwrap();
            }
            self.next_id
        }

        fn close_notification(&self, id: u32) {
            self.closed.send(id).unwrap();
        }
    }

    /// Exercises the real backend over an isolated bus, without a GUI or host
    /// notification service. Run with dbus-run-session and the explicit guard.
    #[test]
    #[ignore = "requires dbus-run-session and HERDR_TEST_NOTIFICATION_BUS=1"]
    fn private_bus_delivers_buttons_transient_hints_and_single_use_responses() {
        use std::time::{Duration, Instant};
        assert_eq!(std::env::var("HERDR_TEST_NOTIFICATION_BUS").as_deref(), Ok("1"));
        let (delivered, received) = std::sync::mpsc::channel();
        let (closed, closed_notifications) = std::sync::mpsc::channel();
        let (release, release_reply) = std::sync::mpsc::channel();
        let bus = zbus::blocking::connection::Builder::session().unwrap()
            .name("org.freedesktop.Notifications").unwrap()
            .serve_at("/org/freedesktop/Notifications", TestNotificationServer {
                delivered, closed, release_reply: Mutex::new(release_reply), next_id: 0,
            }).unwrap()
            .build().unwrap();
        let mut state = SystemNotificationState::new();
        let mut responses = state.response_receiver.take().unwrap();
        let note = |actions| SystemNotification {
            tag: "herdr-agent:test".into(), title: "Choose an environment".into(),
            body: "1. Staging\n2. Production".into(), icon: None, actions,
        };
        let signal = |member, id, action: &str| {
            bus.emit_signal(None::<&str>, "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications", member, &(id, action)).unwrap();
        };
        let receive = |responses: &mut mpsc::UnboundedReceiver<Response>| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Ok(response) = responses.try_recv() { return response; }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("notification response timed out");
        };

        state.show(Some("Herdr test"), Some("herdr-app"), note(vec![
            gpui::SystemNotificationAction { id: "agent-input:7:0".into(), label: "Staging".into() },
            gpui::SystemNotificationAction { id: "agent-input:7:1".into(), label: "Production".into() },
        ]));
        let interactive = received.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(interactive.title, "Choose an environment");
        assert_eq!(interactive.actions, ["default", "default", "gpui-action-0", "Staging", "gpui-action-1", "Production"]);
        assert!(bool::try_from(&interactive.hints["transient"]).unwrap());
        assert_eq!(interactive.expire_timeout, 0, "interactive prompts must not expire");
        assert_eq!(u8::try_from(&interactive.hints["urgency"]).unwrap(), 1);
        assert_eq!(<&str>::try_from(&interactive.hints["desktop-entry"]).unwrap(), "herdr-app");
        assert!(responses.try_recv().is_err(), "showing a banner does not select an option");
        signal("ActionInvoked", interactive.id, "unknown-button");
        signal("ActionInvoked", interactive.id + 100, "gpui-action-0");
        signal("ActivationToken", interactive.id, "test-activation-token");
        signal("ActionInvoked", interactive.id, "gpui-action-1");
        let response = receive(&mut responses);
        assert_eq!(response.notification.tag.as_ref(), "herdr-agent:test");
        assert_eq!(response.notification.action_id.as_deref(), Some("agent-input:7:1"));
        assert_eq!(response.activation_token.as_deref(), Some("test-activation-token"));
        assert_eq!(closed_notifications.recv_timeout(Duration::from_secs(3)).unwrap(), interactive.id);
        signal("ActionInvoked", interactive.id, "gpui-action-0");
        std::thread::sleep(Duration::from_millis(100));
        assert!(responses.try_recv().is_err(), "a second click cannot reply again");

        state.show(Some("Herdr test"), Some("herdr-app"), note(Vec::new()));
        let ordinary = received.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!bool::try_from(&ordinary.hints["transient"]).unwrap());
        assert_eq!(ordinary.expire_timeout, -1, "ordinary alerts keep desktop expiry");
        signal("ActionInvoked", ordinary.id, "default");
        assert_eq!(receive(&mut responses).notification.action_id, None);

        state.show(Some("Herdr test"), None, note(vec![gpui::SystemNotificationAction {
            id: "agent-input:8:0".into(), label: "Choose first (1 + Enter)".into(),
        }]));
        let expired = received.recv_timeout(Duration::from_secs(3)).unwrap();
        bus.emit_signal(None::<&str>, "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications", "NotificationClosed", &(expired.id, 1u32)).unwrap();
        signal("ActionInvoked", expired.id, "gpui-action-0");
        std::thread::sleep(Duration::from_millis(100));
        assert!(responses.try_recv().is_err(), "an expired banner cannot select an option");

        // Looking at the agent or resolving its prompt withdraws a non-expiring
        // banner, without sending a reply or deleting ordinary desktop history.
        let choice = || vec![gpui::SystemNotificationAction {
            id: "agent-input:9:0".into(), label: "Allow once".into(),
        }];
        state.show(Some("Herdr test"), None, note(choice()));
        let dismissed = received.recv_timeout(Duration::from_secs(3)).unwrap();
        state.dismiss("herdr-agent:test");
        assert_eq!(closed_notifications.recv_timeout(Duration::from_secs(3)).unwrap(), dismissed.id);
        signal("ActionInvoked", dismissed.id, "gpui-action-0");
        bus.emit_signal(None::<&str>, "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications", "NotificationClosed", &(dismissed.id, 3u32)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(responses.try_recv().is_err(), "withdrawn prompts cannot reply");

        // Dismiss while Notify is still awaiting its response/ID. The backend
        // must close the banner as soon as the daemon returns its ID.
        let mut pending = note(choice());
        pending.title = "Hold notification reply".into();
        state.show(Some("Herdr test"), None, pending);
        let pending = received.recv_timeout(Duration::from_secs(3)).unwrap();
        state.dismiss("herdr-agent:test");
        release.send(()).unwrap();
        assert_eq!(closed_notifications.recv_timeout(Duration::from_secs(3)).unwrap(), pending.id);
        assert!(state.interactive.lock().unwrap().is_empty());

        // A late close from the previous prompt must not erase the latest one.
        state.show(Some("Herdr test"), None, note(choice()));
        let previous = received.recv_timeout(Duration::from_secs(3)).unwrap();
        state.show(Some("Herdr test"), None, note(choice()));
        let latest = received.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(closed_notifications.recv_timeout(Duration::from_secs(3)).unwrap(), previous.id);
        bus.emit_signal(None::<&str>, "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications", "NotificationClosed", &(previous.id, 3u32)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        state.dismiss("herdr-agent:test");
        assert_eq!(closed_notifications.recv_timeout(Duration::from_secs(3)).unwrap(), latest.id);
        bus.emit_signal(None::<&str>, "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications", "NotificationClosed", &(latest.id, 3u32)).unwrap();
    }

    #[test]
    fn notifications_allow_desktop_history() {
        let hints = notification_hints(None, false);
        assert_eq!(
            hints.get("transient").and_then(|v| bool::try_from(v).ok()),
            Some(false)
        );
        assert_eq!(
            hints.get("urgency").and_then(|v| u8::try_from(v).ok()),
            Some(1)
        );
        assert!(
            !hints.contains_key("resident"),
            "keep normal desktop dismissal behavior"
        );
    }

    #[test]
    fn interactive_notifications_bypass_desktop_history() {
        let hints = notification_hints(Some("herdr-app"), true);
        assert_eq!(
            hints.get("transient").and_then(|v| bool::try_from(v).ok()),
            Some(true)
        );
        assert!(!hints.contains_key("resident"));
    }

    #[test]
    fn notifications_identify_the_desktop_entry_when_set() {
        let hints = notification_hints(Some("herdr-app"), false);
        assert_eq!(
            hints
                .get("desktop-entry")
                .and_then(|v| <&str>::try_from(v).ok()),
            Some("herdr-app")
        );
        for identifier in [None, Some(""), Some("  ")] {
            assert!(!notification_hints(identifier, false).contains_key("desktop-entry"));
        }
    }

    #[test]
    fn activation_tokens_are_consumed_once() {
        let state = SystemNotificationState::new();
        assert_eq!(state.take_activation_token(), None);
        *state.activation_token.borrow_mut() = Some("desktop-click-token".into());
        assert_eq!(
            state.take_activation_token().as_deref(),
            Some("desktop-click-token")
        );
        assert_eq!(state.take_activation_token(), None);
    }

    #[test]
    fn caller_action_ids_do_not_collide_with_transport_action_ids() {
        let action_ids = HashMap::from([
            ("gpui-action-0".to_string(), SharedString::from("default")),
            ("gpui-action-1".to_string(), SharedString::from("__closed")),
        ]);

        assert_eq!(response_action_id(DEFAULT_ACTION, &action_ids), Some(None));
        assert_eq!(
            response_action_id("gpui-action-0", &action_ids),
            Some(Some("default".into()))
        );
        assert_eq!(
            response_action_id("gpui-action-1", &action_ids),
            Some(Some("__closed".into()))
        );
        assert_eq!(response_action_id("unknown", &action_ids), None);
    }
}
