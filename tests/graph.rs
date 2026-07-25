use nochange::graph::{GraphError, GraphUrl, RetryPolicy};
use std::time::{Duration, SystemTime};

#[test]
fn builds_v1_graph_urls_and_preserves_opaque_delta_links() {
    let relative = GraphUrl::build("/me/mailFolders/delta?$select=id,displayName")
        .expect("relative Graph endpoint should be accepted");
    let opaque = "https://graph.microsoft.com/v1.0/me/mailFolders/delta?$skiptoken=A%2fb%2Bz&x=1";
    let pagination = GraphUrl::build(opaque).expect("Graph pagination link should be accepted");

    assert_eq!(
        relative.as_str(),
        "https://graph.microsoft.com/v1.0/me/mailFolders/delta?$select=id,displayName"
    );
    assert_eq!(pagination.as_str(), opaque);
}

#[test]
fn rejects_pagination_links_outside_the_fixed_graph_origin() {
    for link in [
        "http://graph.microsoft.com/v1.0/me",
        "https://evil.example/v1.0/me",
        "https://graph.microsoft.com.evil.example/v1.0/me",
        "https://graph.microsoft.com:444/v1.0/me",
        "https://user@graph.microsoft.com/v1.0/me",
        "https://graph.microsoft.com/beta/me",
        "//evil.example/v1.0/me",
        "me",
    ] {
        assert!(
            matches!(GraphUrl::build(link), Err(GraphError::UnexpectedUrl)),
            "link should be rejected: {link}"
        );
    }
}

#[test]
fn calculates_retry_after_and_exponential_delays() {
    let policy = RetryPolicy::default();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let http_date = httpdate::fmt_http_date(now + Duration::from_secs(12));

    assert_eq!(
        policy
            .get_retry_delay(429, Some("7"), 0, Duration::ZERO, now)
            .expect("valid retry should be calculated"),
        Some(Duration::from_secs(7))
    );
    assert_eq!(
        policy
            .get_retry_delay(503, Some(&http_date), 1, Duration::ZERO, now)
            .expect("HTTP date should be honored"),
        Some(Duration::from_secs(12))
    );
    assert_eq!(
        policy
            .get_retry_delay(502, None, 2, Duration::ZERO, now)
            .expect("missing header should use exponential backoff"),
        Some(Duration::from_secs(4))
    );
    assert_eq!(
        policy
            .get_retry_delay(400, None, 0, Duration::ZERO, now)
            .expect("permanent status should not be retried"),
        None
    );
}

#[test]
fn stops_before_exceeding_the_retry_budget_or_attempt_limit() {
    let policy = RetryPolicy::default();
    let now = SystemTime::UNIX_EPOCH;

    assert!(matches!(
        policy.get_retry_delay(429, Some("301"), 0, Duration::ZERO, now),
        Err(GraphError::RetryExhausted)
    ));
    assert!(matches!(
        policy.get_retry_delay(504, None, policy.max_attempts, Duration::ZERO, now),
        Err(GraphError::RetryExhausted)
    ));

    let overflow_policy = RetryPolicy {
        max_attempts: 1,
        max_total_delay: Duration::MAX,
    };
    assert!(matches!(
        overflow_policy.get_retry_delay(429, Some("1"), 0, Duration::MAX, now),
        Err(GraphError::RetryExhausted)
    ));
}
