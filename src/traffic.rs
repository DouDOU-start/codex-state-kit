use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountTraffic {
    pub concurrent_requests: usize,
    pub rpm: usize,
    /// Total input + output tokens observed in the rolling 60-second window.
    pub tpm: u64,
}

#[derive(Default)]
struct AccountRequests {
    active: usize,
    starts: VecDeque<Instant>,
    tokens: VecDeque<(Instant, u64)>,
}

#[derive(Clone, Default)]
pub(crate) struct TrafficTracker(Arc<Mutex<HashMap<String, AccountRequests>>>);

impl TrafficTracker {
    pub fn begin(&self, account: &str, now: Instant) -> RequestActivity {
        let mut accounts = self.0.lock().unwrap_or_else(|error| error.into_inner());
        prune(&mut accounts, now);
        let requests = accounts.entry(account.to_owned()).or_default();
        requests.active += 1;
        requests.starts.push_back(now);
        RequestActivity {
            tracker: self.clone(),
            account: account.to_owned(),
        }
    }

    pub fn view(&self, account: Option<&str>, now: Instant) -> AccountTraffic {
        let mut accounts = self.0.lock().unwrap_or_else(|error| error.into_inner());
        prune(&mut accounts, now);
        account
            .and_then(|id| accounts.get(id))
            .map(|requests| AccountTraffic {
                concurrent_requests: requests.active,
                rpm: requests.starts.len(),
                tpm: requests.tokens.iter().map(|(_, tokens)| *tokens).sum(),
            })
            .unwrap_or_default()
    }

    pub fn observe_tokens(&self, account: &str, input: Option<u64>, output: Option<u64>) {
        let tokens = input.unwrap_or(0).saturating_add(output.unwrap_or(0));
        if tokens == 0 {
            return;
        }
        let now = Instant::now();
        let mut accounts = self.0.lock().unwrap_or_else(|error| error.into_inner());
        prune(&mut accounts, now);
        if let Some(requests) = accounts.get_mut(account) {
            requests.tokens.push_back((now, tokens));
        }
    }
}

fn prune(accounts: &mut HashMap<String, AccountRequests>, now: Instant) {
    accounts.retain(|_, requests| {
        while requests.starts.front().is_some_and(|started| {
            now.saturating_duration_since(*started) >= Duration::from_secs(60)
        }) {
            requests.starts.pop_front();
        }
        while requests.tokens.front().is_some_and(|(observed, _)| {
            now.saturating_duration_since(*observed) >= Duration::from_secs(60)
        }) {
            requests.tokens.pop_front();
        }
        requests.active > 0 || !requests.starts.is_empty() || !requests.tokens.is_empty()
    });
}

/// Owned by the pending upstream request, then by its response body. Dropping
/// either releases concurrency synchronously, including task cancellation.
pub(crate) struct RequestActivity {
    tracker: TrafficTracker,
    account: String,
}

impl RequestActivity {
    /// Record provider-reported usage when the request settles. Keeping this
    /// on the activity makes TPM account-scoped and prevents a late account
    /// switch from attributing tokens to the wrong login.
    pub(crate) fn observe_tokens(&mut self, input: Option<u64>, output: Option<u64>) {
        self.tracker.observe_tokens(&self.account, input, output);
    }
}

impl Drop for RequestActivity {
    fn drop(&mut self) {
        let mut accounts = self
            .tracker
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(requests) = accounts.get_mut(&self.account) {
            requests.active = requests.active.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sliding_window_expires_starts_without_losing_long_requests() {
        let tracker = TrafficTracker::default();
        let now = Instant::now();
        let first = tracker.begin("a", now);
        let second = tracker.begin("a", now + Duration::from_secs(30));
        assert_eq!(
            tracker.view(Some("a"), now + Duration::from_secs(59)),
            AccountTraffic {
                concurrent_requests: 2,
                rpm: 2,
                tpm: 0,
            }
        );
        assert_eq!(
            tracker.view(Some("a"), now + Duration::from_secs(60)),
            AccountTraffic {
                concurrent_requests: 2,
                rpm: 1,
                tpm: 0,
            }
        );
        drop(first);
        assert_eq!(
            tracker.view(Some("a"), now + Duration::from_secs(90)),
            AccountTraffic {
                concurrent_requests: 1,
                rpm: 0,
                tpm: 0,
            }
        );
        drop(second);
        assert_eq!(
            tracker.view(Some("a"), now + Duration::from_secs(91)),
            AccountTraffic::default()
        );
        assert!(tracker.0.lock().unwrap().is_empty());
    }

    #[test]
    fn switching_accounts_and_finishing_old_requests_keeps_counts_separate() {
        let tracker = TrafficTracker::default();
        let now = Instant::now();
        let old = tracker.begin("old", now);
        let new = tracker.begin("new", now);
        drop(old);
        assert_eq!(
            tracker.view(Some("old"), now),
            AccountTraffic {
                concurrent_requests: 0,
                rpm: 1,
                tpm: 0,
            }
        );
        assert_eq!(
            tracker.view(Some("new"), now),
            AccountTraffic {
                concurrent_requests: 1,
                rpm: 1,
                tpm: 0,
            }
        );
        assert_eq!(tracker.view(None, now), AccountTraffic::default());
        drop(new);
    }

    #[test]
    fn tpm_counts_observed_input_and_output_tokens_in_the_sliding_window() {
        let tracker = TrafficTracker::default();
        let now = Instant::now();
        let mut activity = tracker.begin("a", now);
        activity.observe_tokens(Some(1_000), Some(250));
        assert_eq!(tracker.view(Some("a"), Instant::now()).tpm, 1_250);
        assert_eq!(
            tracker.view(Some("a"), now + Duration::from_secs(61)).tpm,
            0
        );
        drop(activity);
    }
}
