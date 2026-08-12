use std::time::Duration;

pub(super) fn periodic_reconcile_interval() -> Option<Duration> {
    match std::env::var("UNEARTH_WATCH_RECONCILE_SECS") {
        Ok(value) => periodic_reconcile_interval_from(Some(&value)),
        Err(_) => None,
    }
}

pub(super) fn periodic_reconcile_interval_from(value: Option<&str>) -> Option<Duration> {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|seconds| (seconds > 0).then_some(Duration::from_secs(seconds)))
}
