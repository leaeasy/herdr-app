//! Unity LauncherEntry counts and urgency, understood by Plasma's task manager too.
//! Keep bus traffic off the UI thread and ordered, including quit cleanup.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, mpsc};
use zbus::{blocking::Connection, zvariant::Value};

const APP_URI: &str = "application://herdr-app.desktop";
const OBJECT_PATH: &str = "/com/canonical/unity/launcherentry/1";
const INTERFACE: &str = "com.canonical.Unity.LauncherEntry";

enum Update {
    State(usize, bool),
    Clear(mpsc::Sender<()>),
}

struct Launcher {
    count: Option<usize>,
    urgent: bool,
    sender: mpsc::Sender<Update>,
}

static LAUNCHER: OnceLock<Mutex<Launcher>> = OnceLock::new();

fn properties(count: usize, urgent: bool) -> HashMap<&'static str, Value<'static>> {
    HashMap::from([
        ("count", Value::from(i64::try_from(count).unwrap_or(i64::MAX))),
        ("count-visible", Value::from(count > 0)),
        ("urgent", Value::from(urgent)),
    ])
}

fn emit(connection: &mut Option<Connection>, count: usize, urgent: bool) {
    if connection.is_none() {
        *connection = Connection::session().ok();
    }
    let Some(bus) = connection.as_ref() else { return };
    if bus.emit_signal(None::<&str>, OBJECT_PATH, INTERFACE, "Update", &(APP_URI, properties(count, urgent))).is_err() {
        // Reconnect on the next update if the session bus went away.
        *connection = None;
    }
}

fn launcher() -> &'static Mutex<Launcher> {
    LAUNCHER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut connection = None;
            while let Ok(mut update) = receiver.recv() {
                // Coalesce bursts, but never lose the final clear or its acknowledgement.
                while matches!(update, Update::State(..)) {
                    match receiver.try_recv() {
                        Ok(next) => update = next,
                        Err(_) => break,
                    }
                }
                match update {
                    Update::State(count, urgent) => emit(&mut connection, count, urgent),
                    Update::Clear(done) => {
                        emit(&mut connection, 0, false);
                        let _ = done.send(());
                        break;
                    }
                }
            }
        });
        Mutex::new(Launcher { count: None, urgent: false, sender })
    })
}

pub fn update<'a>(states: impl IntoIterator<Item = Option<&'a str>>) {
    let count = working_count(states);
    let mut launcher = launcher().lock().unwrap();
    if launcher.count != Some(count) {
        launcher.count = Some(count);
        let _ = launcher.sender.send(Update::State(count, launcher.urgent));
    }
}

/// Urgency and count travel together so queued updates cannot overwrite either.
pub fn set_urgent(urgent: bool) {
    let mut launcher = launcher().lock().unwrap();
    if launcher.urgent != urgent {
        launcher.urgent = urgent;
        let _ = launcher.sender.send(Update::State(launcher.count.unwrap_or(0), urgent));
    }
}

fn working_count<'a>(states: impl IntoIterator<Item = Option<&'a str>>) -> usize {
    states.into_iter().filter(|state| *state == Some("working")).count()
}

/// The caller waits off the UI thread, while the session bus is still available.
pub fn clear() -> Option<mpsc::Receiver<()>> {
    let launcher = LAUNCHER.get()?;
    let (done, receiver) = mpsc::channel();
    launcher.lock().unwrap().sender.send(Update::Clear(done)).ok()?;
    Some(receiver)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_working_agents_contribute_to_the_count() {
        assert_eq!(working_count([None, Some("working"), Some("blocked"), Some("idle"), Some("done"), Some("error"), Some("working")]), 2);
        assert_eq!(working_count([]), 0);
    }

    #[test]
    fn updates_identify_herdr_and_hide_zero_counts() {
        assert_eq!(APP_URI, "application://herdr-app.desktop");
        for count in [0, 5] {
            let props = properties(count, false);
            assert_eq!(i64::try_from(&props["count"]).unwrap(), count as i64);
            assert_eq!(bool::try_from(&props["count-visible"]).unwrap(), count > 0);
            assert!(!bool::try_from(&props["urgent"]).unwrap());
        }
    }

    /// Run only on an isolated bus: `dbus-run-session -- ... --ignored`.
    #[test]
    #[ignore = "requires a private session bus; emits launcher signals"]
    fn private_bus_receives_typed_counts_and_shutdown_clear() {
        use zbus::{MatchRule, blocking::MessageIterator, zvariant::OwnedValue};
        use std::time::Duration;

        let observer = Connection::session().unwrap();
        let rule = MatchRule::builder().msg_type(zbus::message::Type::Signal)
            .path(OBJECT_PATH).unwrap().interface(INTERFACE).unwrap()
            .member("Update").unwrap().build();
        let mut messages = MessageIterator::for_match_rule(rule, &observer, Some(8)).unwrap();
        update([Some("working"), Some("blocked"), Some("working")]);
        let first = messages.next().unwrap().unwrap();
        let sender = first.header().sender().unwrap().to_string();
        let (uri, props): (String, HashMap<String, OwnedValue>) = first.body().deserialize().unwrap();
        assert_eq!(uri, APP_URI);
        assert_eq!(i64::try_from(&props["count"]).unwrap(), 2);
        assert!(bool::try_from(&props["count-visible"]).unwrap());
        // An unchanged count is suppressed. Urgency must preserve the count,
        // and count changes must preserve urgency until acknowledged.
        update([Some("working"), Some("working")]);
        set_urgent(true);
        let urgent = messages.next().unwrap().unwrap();
        let (_, props): (String, HashMap<String, OwnedValue>) = urgent.body().deserialize().unwrap();
        assert!(bool::try_from(&props["urgent"]).unwrap());
        assert_eq!(i64::try_from(&props["count"]).unwrap(), 2);
        update([Some("working")]);
        let counted = messages.next().unwrap().unwrap();
        let (_, props): (String, HashMap<String, OwnedValue>) = counted.body().deserialize().unwrap();
        assert!(bool::try_from(&props["urgent"]).unwrap());
        assert_eq!(i64::try_from(&props["count"]).unwrap(), 1);
        set_urgent(false);
        let acknowledged = messages.next().unwrap().unwrap();
        let (_, props): (String, HashMap<String, OwnedValue>) = acknowledged.body().deserialize().unwrap();
        assert!(!bool::try_from(&props["urgent"]).unwrap());
        assert_eq!(i64::try_from(&props["count"]).unwrap(), 1);
        clear().unwrap().recv_timeout(Duration::from_secs(3)).unwrap();
        let last = messages.next().unwrap().unwrap();
        assert_eq!(last.header().sender().unwrap().as_str(), sender);
        let (uri, props): (String, HashMap<String, OwnedValue>) = last.body().deserialize().unwrap();
        assert_eq!(uri, APP_URI);
        assert_eq!(i64::try_from(&props["count"]).unwrap(), 0);
        assert!(!bool::try_from(&props["count-visible"]).unwrap());
        assert!(!bool::try_from(&props["urgent"]).unwrap());
    }
}
