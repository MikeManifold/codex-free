//! Caller-owned OAuth waits outlive the RPC that returned the authorization URL.
//! No URL, authorization code, token, or provider error text is retained here.
use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::Duration,
};

use serde_json::{Value, json};
use tokio::time::Instant;

const COMPLETION_GRACE: Duration = Duration::from_secs(15);

struct Flow {
    generation: u64,
    deadline: Instant,
    waiting: bool,
    login_id: Option<String>,
    early_completions: HashSet<Option<String>>,
    response_unknown: bool,
}

#[derive(Default)]
struct State {
    flows: HashMap<String, Flow>,
    last_finished: Option<Instant>,
}

#[derive(Default)]
pub(super) struct OAuthWaits(Mutex<State>);

impl OAuthWaits {
    pub fn begin(&self, generation: u64, name: &str, timeout: Duration) -> bool {
        let mut state = self.0.lock().unwrap();
        // Only one explicit authorization per caller; duplicate calls must not
        // create another provider grant while the user is handling the first.
        if !state.flows.is_empty() {
            return false;
        }
        state.flows.insert(
            name.into(),
            Flow {
                generation,
                deadline: Instant::now() + timeout + COMPLETION_GRACE,
                waiting: false,
                login_id: None,
                early_completions: HashSet::new(),
                response_unknown: false,
            },
        );
        true
    }

    pub fn waiting(&self, generation: u64, name: &str, login_id: Option<&str>, timeout: Duration) {
        let mut state = self.0.lock().unwrap();
        let Some(flow) = state
            .flows
            .get_mut(name)
            .filter(|f| f.generation == generation)
        else {
            return;
        };
        flow.waiting = true;
        flow.login_id = login_id.map(str::to_owned);
        flow.deadline = Instant::now() + timeout + COMPLETION_GRACE;
        if flow.early_completions.contains(&flow.login_id) {
            state.flows.remove(name);
            state.last_finished = Some(Instant::now());
        }
    }

    pub fn completed(&self, generation: u64, params: &Value) {
        let Some(name) = params["name"].as_str() else {
            return;
        };
        if !params["success"].is_boolean() {
            return;
        }
        let mut state = self.0.lock().unwrap();
        let Some(flow) = state
            .flows
            .get_mut(name)
            .filter(|f| f.generation == generation)
        else {
            return;
        };
        let login_id = params["loginId"].as_str().map(str::to_owned);
        if !flow.waiting {
            // Notifications may precede the response carrying the loginId.
            if flow.early_completions.len() < 8 {
                flow.early_completions.insert(login_id);
            }
            return;
        }
        if !flow.response_unknown && flow.login_id != login_id {
            return;
        }
        state.flows.remove(name);
        state.last_finished = Some(Instant::now());
    }

    pub fn response_unknown(&self, generation: u64, name: &str) {
        let mut state = self.0.lock().unwrap();
        let Some(flow) = state
            .flows
            .get_mut(name)
            .filter(|f| f.generation == generation)
        else {
            return;
        };
        // Exactly one login may exist on this caller-owned runtime. If its RPC
        // response was lost, a same-server/generation completion is authoritative.
        flow.waiting = true;
        flow.response_unknown = true;
        if !flow.early_completions.is_empty() {
            state.flows.remove(name);
            state.last_finished = Some(Instant::now());
        }
    }

    pub fn release(&self, generation: u64, name: &str) {
        let mut state = self.0.lock().unwrap();
        if state
            .flows
            .get(name)
            .is_some_and(|f| f.generation == generation)
        {
            state.flows.remove(name);
            state.last_finished = Some(Instant::now());
        }
    }

    pub fn runtime_exited(&self, generation: u64) {
        let mut state = self.0.lock().unwrap();
        let before = state.flows.len();
        state.flows.retain(|_, f| f.generation != generation);
        if state.flows.len() != before {
            state.last_finished = Some(Instant::now());
        }
    }

    pub fn has_flows(&self) -> bool {
        !self.0.lock().unwrap().flows.is_empty()
    }

    pub fn idle_state(&self, now: Instant) -> (bool, Option<Instant>) {
        let state = self.0.lock().unwrap();
        (
            state.flows.values().any(|f| f.deadline > now),
            state.last_finished,
        )
    }

    pub fn diagnostics(&self) -> Value {
        let now = Instant::now();
        let state = self.0.lock().unwrap();
        let mut flows = state.flows.iter().map(|(name, f)| json!({
            "server":name, "phase":if f.deadline <= now {"expired"} else if f.waiting {"waiting_for_user"} else {"starting_or_outcome_unknown"},
            "remainingSeconds": f.deadline.saturating_duration_since(now).as_secs(),
        })).collect::<Vec<_>>();
        flows.sort_by(|a, b| a["server"].as_str().cmp(&b["server"].as_str()));
        json!({"pending":!state.flows.is_empty(),"flows":flows})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_before_response_is_correlated_and_does_not_resurrect_wait() {
        let waits = OAuthWaits::default();
        assert!(waits.begin(1, "ibkr", Duration::from_secs(600)));
        assert!(!waits.begin(1, "ibkr", Duration::from_secs(600)));
        waits.completed(1, &json!({"name":"ibkr","loginId":"old","success":true}));
        waits.completed(
            1,
            &json!({"name":"ibkr","loginId":"current","success":true}),
        );
        waits.waiting(1, "ibkr", Some("current"), Duration::from_secs(600));
        assert!(!waits.has_flows());
    }

    #[test]
    fn other_generation_server_or_login_cannot_complete_current_wait() {
        let waits = OAuthWaits::default();
        assert!(waits.begin(2, "ibkr", Duration::from_secs(600)));
        waits.waiting(2, "ibkr", Some("current"), Duration::from_secs(600));
        waits.completed(
            1,
            &json!({"name":"ibkr","loginId":"current","success":true}),
        );
        waits.completed(
            2,
            &json!({"name":"other","loginId":"current","success":true}),
        );
        waits.completed(2, &json!({"name":"ibkr","loginId":"old","success":false}));
        waits.runtime_exited(1);
        assert!(waits.has_flows());
        waits.completed(
            2,
            &json!({"name":"ibkr","loginId":"current","success":false}),
        );
        assert!(!waits.has_flows());
    }

    #[test]
    fn legacy_notification_and_lost_notification_have_bounded_lifetimes() {
        let waits = OAuthWaits::default();
        let now = Instant::now();
        assert!(waits.begin(1, "ibkr", Duration::from_secs(600)));
        waits.waiting(1, "ibkr", None, Duration::from_secs(600));
        assert!(waits.idle_state(now + Duration::from_secs(301)).0);
        assert!(!waits.idle_state(now + Duration::from_secs(616)).0);
        // Expiration does not silently allow a second grant on the old runtime.
        assert!(!waits.begin(1, "ibkr", Duration::from_secs(600)));
        waits.completed(1, &json!({"name":"ibkr","success":true}));
        assert!(!waits.has_flows());
        assert!(waits.idle_state(Instant::now()).1.is_some());
    }

    #[test]
    fn runtime_exit_releases_only_owned_generation_and_diagnostics_omit_secrets() {
        let waits = OAuthWaits::default();
        assert!(waits.begin(3, "ibkr", Duration::from_secs(600)));
        waits.waiting(
            3,
            "ibkr",
            Some("private-login-id"),
            Duration::from_secs(600),
        );
        let diagnostic = waits.diagnostics().to_string();
        assert!(!diagnostic.contains("private-login-id"));
        assert!(!diagnostic.contains("authorizationUrl"));
        waits.runtime_exited(3);
        assert!(!waits.has_flows());
    }
}
