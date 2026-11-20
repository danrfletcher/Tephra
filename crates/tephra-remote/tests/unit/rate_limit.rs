//! E1: the classifier over every rule, both providers, plus the
//! header-less fallback, the 1 s floor and the gate, on a fake clock.

use std::time::{Duration, SystemTime};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tephra_remote::clock::{Clock, FakeClock};
use tephra_remote::rate_limit::{
    classify, Api, Backoff, RateGate, Verdict, FALLBACK_CAP, MAX_HONOURED, MIN_DELAY,
};

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    map
}

fn limited(secs: u64) -> Verdict {
    Verdict::RateLimited {
        delay: Some(Duration::from_secs(secs)),
    }
}

const NO_HEADER: Verdict = Verdict::RateLimited { delay: None };

fn now() -> SystemTime {
    FakeClock::at_epoch_2026().now()
}

fn epoch(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
}

struct Case {
    name: &'static str,
    api: Api,
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: &'static str,
    want: Verdict,
}

fn case(
    name: &'static str,
    api: Api,
    status: u16,
    headers: &[(&'static str, &str)],
    body: &'static str,
    want: Verdict,
) -> Case {
    Case {
        name,
        api,
        status,
        headers: headers.iter().map(|(n, v)| (*n, v.to_string())).collect(),
        body,
        want,
    }
}

fn drive_body(reason: &str) -> String {
    format!(r#"{{"error":{{"code":403,"message":"x","errors":[{{"reason":"{reason}"}}]}}}}"#)
}

#[test]
fn github_rules_table() {
    let at = now();
    let reset_in_90 = (epoch(at) + 90).to_string();
    let reset_past = (epoch(at) - 500).to_string();
    let gh = Api::Github;
    let cases = vec![
        case(
            "403 Retry-After",
            gh,
            403,
            &[("retry-after", "30")],
            "",
            limited(30),
        ),
        case(
            "429 Retry-After",
            gh,
            429,
            &[("retry-after", "7")],
            "",
            limited(7),
        ),
        case(
            "Retry-After 0 is zero, floored later",
            gh,
            403,
            &[("retry-after", "0")],
            "",
            limited(0),
        ),
        case(
            "Retry-After 86400 is honoured",
            gh,
            429,
            &[("retry-after", "86400")],
            "",
            limited(86_400),
        ),
        case(
            "Retry-After wins over reset headers",
            gh,
            403,
            &[
                ("retry-after", "5"),
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", &reset_in_90),
            ],
            "",
            limited(5),
        ),
        case(
            "remaining 0 with reset",
            gh,
            403,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", &reset_in_90),
            ],
            "",
            limited(90),
        ),
        case(
            "remaining 0 with reset on a 429",
            gh,
            429,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", &reset_in_90),
            ],
            "",
            limited(90),
        ),
        case(
            "reset in the past is zero, floored later",
            gh,
            403,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", &reset_past),
            ],
            "",
            limited(0),
        ),
        case(
            "remaining 0 and a garbage reset has no usable header",
            gh,
            403,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "tomorrow"),
            ],
            "",
            NO_HEADER,
        ),
        case(
            "remaining 0 and a negative reset has no usable header",
            gh,
            403,
            &[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "-30")],
            "",
            NO_HEADER,
        ),
        case(
            "remaining 0 without reset",
            gh,
            403,
            &[("x-ratelimit-remaining", "0")],
            "",
            NO_HEADER,
        ),
        case(
            "secondary limit body, no headers",
            gh,
            403,
            &[],
            r#"{"message":"You have exceeded a secondary rate limit. Please wait."}"#,
            NO_HEADER,
        ),
        case(
            "abuse detection body, no headers",
            gh,
            403,
            &[],
            r#"{"message":"You have triggered an abuse detection mechanism."}"#,
            NO_HEADER,
        ),
        case(
            "primary limit body, no headers",
            gh,
            403,
            &[],
            r#"{"message":"API rate limit exceeded for user ID 1."}"#,
            NO_HEADER,
        ),
        case("429 with nothing", gh, 429, &[], "", NO_HEADER),
        case(
            "429 with a negative Retry-After",
            gh,
            429,
            &[("retry-after", "-5")],
            "",
            NO_HEADER,
        ),
        case(
            "429 with a non-numeric Retry-After",
            gh,
            429,
            &[("retry-after", "soon")],
            "",
            NO_HEADER,
        ),
        case(
            "429 with a decimal Retry-After",
            gh,
            429,
            &[("retry-after", "1.5")],
            "",
            NO_HEADER,
        ),
        case(
            "429 with an empty Retry-After",
            gh,
            429,
            &[("retry-after", "")],
            "",
            NO_HEADER,
        ),
        case(
            "secondary body with a negative Retry-After",
            gh,
            403,
            &[("retry-after", "-1")],
            "secondary rate limit",
            NO_HEADER,
        ),
        case(
            "Resource not accessible is a permission failure",
            gh,
            403,
            &[],
            r#"{"message":"Resource not accessible by integration"}"#,
            Verdict::PermissionDenied,
        ),
        case(
            "403 with an empty body",
            gh,
            403,
            &[],
            "",
            Verdict::PermissionDenied,
        ),
        case(
            "403 with remaining above 0",
            gh,
            403,
            &[("x-ratelimit-remaining", "12")],
            r#"{"message":"Forbidden"}"#,
            Verdict::PermissionDenied,
        ),
        case(
            "403 with an unusable Retry-After and a permission body",
            gh,
            403,
            &[("retry-after", "soon")],
            r#"{"message":"Resource not accessible by integration"}"#,
            Verdict::PermissionDenied,
        ),
        case(
            "401 is not a rate limit",
            gh,
            401,
            &[("retry-after", "5")],
            "",
            Verdict::NotRateLimit,
        ),
        case(
            "404 is not a rate limit",
            gh,
            404,
            &[],
            "",
            Verdict::NotRateLimit,
        ),
        case(
            "500 is not a rate limit",
            gh,
            500,
            &[("retry-after", "5")],
            "",
            Verdict::NotRateLimit,
        ),
        case(
            "200 is not a rate limit",
            gh,
            200,
            &[],
            "",
            Verdict::NotRateLimit,
        ),
    ];
    run(cases, at);
}

#[test]
fn drive_rules_table() {
    let at = now();
    let dr = Api::Drive;
    let rate = drive_body("rateLimitExceeded");
    let user = drive_body("userRateLimitExceeded");
    let sharing = drive_body("sharingRateLimitExceeded");
    let perms = drive_body("insufficientPermissions");
    let cases = vec![
        case(
            "429 with Retry-After",
            dr,
            429,
            &[("retry-after", "11")],
            "",
            limited(11),
        ),
        case("429 with nothing", dr, 429, &[], "", NO_HEADER),
        case(
            "429 with a Drive body",
            dr,
            429,
            &[],
            leak(&rate),
            NO_HEADER,
        ),
        case(
            "429 Retry-After 0",
            dr,
            429,
            &[("retry-after", "0")],
            "",
            limited(0),
        ),
        case(
            "429 Retry-After 86400",
            dr,
            429,
            &[("retry-after", "86400")],
            "",
            limited(86_400),
        ),
        case(
            "429 negative Retry-After",
            dr,
            429,
            &[("retry-after", "-3")],
            "",
            NO_HEADER,
        ),
        case(
            "429 non-numeric Retry-After",
            dr,
            429,
            &[("retry-after", "later")],
            "",
            NO_HEADER,
        ),
        case(
            "403 rateLimitExceeded",
            dr,
            403,
            &[],
            leak(&rate),
            NO_HEADER,
        ),
        case(
            "403 userRateLimitExceeded",
            dr,
            403,
            &[],
            leak(&user),
            NO_HEADER,
        ),
        case(
            "403 sharingRateLimitExceeded",
            dr,
            403,
            &[],
            leak(&sharing),
            NO_HEADER,
        ),
        case(
            "403 rate reason with Retry-After",
            dr,
            403,
            &[("retry-after", "20")],
            leak(&user),
            limited(20),
        ),
        case(
            "403 insufficientPermissions",
            dr,
            403,
            &[],
            leak(&perms),
            Verdict::PermissionDenied,
        ),
        case(
            "403 insufficientPermissions ignores Retry-After",
            dr,
            403,
            &[("retry-after", "20")],
            leak(&perms),
            Verdict::PermissionDenied,
        ),
        case(
            "403 with an empty body",
            dr,
            403,
            &[],
            "",
            Verdict::PermissionDenied,
        ),
        case(
            "403 with a non-JSON body",
            dr,
            403,
            &[],
            "<html>forbidden</html>",
            Verdict::PermissionDenied,
        ),
        case(
            "403 with JSON of another shape",
            dr,
            403,
            &[],
            r#"{"message":"rate limit"}"#,
            Verdict::PermissionDenied,
        ),
        case(
            "403 with no errors list",
            dr,
            403,
            &[],
            r#"{"error":{"code":403}}"#,
            Verdict::PermissionDenied,
        ),
        case(
            "403 with a reason of another case",
            dr,
            403,
            &[],
            leak(&drive_body("RATELIMITEXCEEDED")),
            Verdict::PermissionDenied,
        ),
        case(
            "401 is not a rate limit",
            dr,
            401,
            &[],
            "",
            Verdict::NotRateLimit,
        ),
        case(
            "500 is not a rate limit",
            dr,
            500,
            &[],
            leak(&rate),
            Verdict::NotRateLimit,
        ),
        case(
            "200 is not a rate limit",
            dr,
            200,
            &[],
            "",
            Verdict::NotRateLimit,
        ),
    ];
    run(cases, at);
}

/// Table bodies are `&'static str`; the generated ones live for the test.
fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

fn run(cases: Vec<Case>, at: SystemTime) {
    for c in cases {
        let pairs: Vec<(&str, &str)> = c.headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
        let got = classify(c.api, c.status, &headers(&pairs), c.body.as_bytes(), at);
        assert_eq!(got, c.want, "{}", c.name);
    }
}

#[test]
fn a_non_utf8_retry_after_is_unusable_not_a_panic() {
    let mut map = HeaderMap::new();
    map.insert(
        "retry-after",
        HeaderValue::from_bytes(&[0xff, 0xfe, b'1']).unwrap(),
    );
    assert_eq!(classify(Api::Github, 429, &map, b"", now()), NO_HEADER);
    assert_eq!(classify(Api::Drive, 429, &map, b"", now()), NO_HEADER);
    // On a 403 with nothing else to go on it is a permission failure.
    assert_eq!(
        classify(Api::Github, 403, &map, b"", now()),
        Verdict::PermissionDenied
    );

    let mut map = HeaderMap::new();
    map.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
    map.insert(
        "x-ratelimit-reset",
        HeaderValue::from_bytes(&[0xc3, 0x28]).unwrap(),
    );
    assert_eq!(classify(Api::Github, 403, &map, b"", now()), NO_HEADER);
}

#[test]
fn a_body_that_is_not_utf8_does_not_panic() {
    let junk = [0xff, 0xfe, 0x00, 0x80];
    assert_eq!(
        classify(Api::Github, 403, &HeaderMap::new(), &junk, now()),
        Verdict::PermissionDenied
    );
    assert_eq!(
        classify(Api::Drive, 403, &HeaderMap::new(), &junk, now()),
        Verdict::PermissionDenied
    );
    assert_eq!(
        classify(Api::Drive, 429, &HeaderMap::new(), &junk, now()),
        NO_HEADER
    );
}

#[test]
fn retry_after_may_be_an_http_date() {
    let at = now();
    let later = httpdate::fmt_http_date(at + Duration::from_secs(120));
    let earlier = httpdate::fmt_http_date(at - Duration::from_secs(120));
    assert_eq!(
        classify(
            Api::Github,
            429,
            &headers(&[("retry-after", &later)]),
            b"",
            at
        ),
        limited(120)
    );
    assert_eq!(
        classify(
            Api::Drive,
            429,
            &headers(&[("retry-after", &later)]),
            b"",
            at
        ),
        limited(120)
    );
    // A date already past is a zero delay (floored to 1 s later), not an error.
    assert_eq!(
        classify(
            Api::Github,
            429,
            &headers(&[("retry-after", &earlier)]),
            b"",
            at
        ),
        limited(0)
    );
}

#[test]
fn an_absurd_reset_or_retry_after_never_panics_and_is_clamped() {
    let at = now();
    for value in [
        "18446744073709551615",
        "9223372036854775807",
        "99999999999999999999",
    ] {
        for api in [Api::Github, Api::Drive] {
            let v = classify(api, 429, &headers(&[("retry-after", value)]), b"", at);
            if let Verdict::RateLimited { delay } = v {
                let mut gate = RateGate::default();
                let retry_at = gate.record_limited(at, delay);
                assert!(retry_at <= at + MAX_HONOURED, "{value}");
                assert!(retry_at >= at + MIN_DELAY, "{value}");
            } else {
                panic!("{value}: {v:?}");
            }
        }
        let v = classify(
            Api::Github,
            403,
            &headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", value)]),
            b"",
            at,
        );
        assert!(matches!(v, Verdict::RateLimited { .. }), "{value}: {v:?}");
        if let Verdict::RateLimited { delay } = v {
            let retry_at = RateGate::default().record_limited(at, delay);
            assert!(retry_at <= at + MAX_HONOURED);
        }
    }
}

#[test]
fn the_fallback_doubles_from_one_second_to_the_cap_and_a_success_resets_it() {
    let mut backoff = Backoff::default();
    let steps: Vec<u64> = (0..9).map(|_| backoff.next_delay(None).as_secs()).collect();
    assert_eq!(steps, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    assert_eq!(FALLBACK_CAP, Duration::from_secs(60));

    // Far past the shift width: still the cap, no overflow.
    for _ in 0..200 {
        assert_eq!(backoff.next_delay(None), FALLBACK_CAP);
    }

    backoff.reset();
    assert_eq!(backoff.next_delay(None), Duration::from_secs(1));
    assert_eq!(backoff.next_delay(None), Duration::from_secs(2));
}

#[test]
fn a_header_delay_is_floored_and_clamped_and_leaves_the_doubling_alone() {
    let mut backoff = Backoff::default();
    assert_eq!(backoff.next_delay(Some(Duration::ZERO)), MIN_DELAY);
    assert_eq!(
        backoff.next_delay(Some(Duration::from_millis(400))),
        MIN_DELAY
    );
    assert_eq!(
        backoff.next_delay(Some(Duration::from_secs(86_400))),
        Duration::from_secs(86_400)
    );
    assert_eq!(
        backoff.next_delay(Some(Duration::from_secs(u64::MAX))),
        MAX_HONOURED
    );
    // None of those advanced the doubling.
    assert_eq!(backoff.next_delay(None), Duration::from_secs(1));
    assert_eq!(backoff.next_delay(None), Duration::from_secs(2));
}

#[test]
fn the_gate_refuses_until_retry_at_and_the_first_call_at_it_goes_out() {
    let clock = FakeClock::at_epoch_2026();
    let mut gate = RateGate::default();
    assert_eq!(gate.check(clock.now()), Ok(()));

    let retry_at = gate.record_limited(clock.now(), Some(Duration::from_secs(30)));
    assert_eq!(retry_at, clock.now() + Duration::from_secs(30));

    assert_eq!(gate.check(clock.now()), Err(retry_at));
    clock.advance(Duration::from_secs(29));
    assert_eq!(gate.check(clock.now()), Err(retry_at));
    clock.advance(Duration::from_secs(1));
    assert_eq!(gate.check(clock.now()), Ok(()));
    clock.advance(Duration::from_secs(1000));
    assert_eq!(gate.check(clock.now()), Ok(()));
}

#[test]
fn retry_at_for_a_zero_or_past_header_is_one_second_ahead_never_now() {
    let clock = FakeClock::at_epoch_2026();
    for header_delay in [Some(Duration::ZERO), Some(Duration::from_millis(1)), None] {
        let mut gate = RateGate::default();
        let retry_at = gate.record_limited(clock.now(), header_delay);
        assert_eq!(
            retry_at,
            clock.now() + Duration::from_secs(1),
            "{header_delay:?}"
        );
        assert!(gate.check(clock.now()).is_err(), "no instant retry loop");
    }
}

#[test]
fn consecutive_header_less_limits_double_through_the_gate_and_a_success_resets() {
    let clock = FakeClock::at_epoch_2026();
    let mut gate = RateGate::default();
    let mut waits = Vec::new();
    for _ in 0..8 {
        let at = gate.record_limited(clock.now(), None);
        waits.push(at.duration_since(clock.now()).unwrap().as_secs());
        clock.set(at);
    }
    assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60]);

    gate.record_success();
    assert_eq!(gate.check(clock.now()), Ok(()));
    let at = gate.record_limited(clock.now(), None);
    assert_eq!(
        at.duration_since(clock.now()).unwrap(),
        Duration::from_secs(1)
    );
}

#[test]
fn a_success_clears_a_running_limit() {
    let clock = FakeClock::at_epoch_2026();
    let mut gate = RateGate::default();
    gate.record_limited(clock.now(), Some(Duration::from_secs(600)));
    assert!(gate.check(clock.now()).is_err());
    gate.record_success();
    assert_eq!(gate.check(clock.now()), Ok(()));
}
