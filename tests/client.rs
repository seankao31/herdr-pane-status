//! The client against a fake herdr on a real socket or named pipe.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use common::FakeHerdr;
use herdr_pane_status::client::{Api, EventSource, HerdrClient, HerdrError};
use serde_json::json;

fn client(server: &FakeHerdr, timeout: Duration) -> HerdrClient {
    HerdrClient::new(server.path.clone(), timeout)
}

#[test]
fn a_request_returns_the_result_and_sends_method_and_params() {
    let server = FakeHerdr::start();
    server.on("pane.get", |params| json!({"pane": params}));
    let mut client = client(&server, Duration::from_secs(2));
    let result = client
        .request("pane.get", json!({"pane_id": "w1:p1"}))
        .unwrap();
    assert_eq!(result, json!({"pane": {"pane_id": "w1:p1"}}));
    assert_eq!(server.requests("pane.get"), [json!({"pane_id": "w1:p1"})]);
}

#[test]
fn an_error_response_is_an_api_error() {
    let server = FakeHerdr::start();
    server.on(
        "pane.get",
        |_| json!({"__error__": {"code": "not_found", "message": "no such pane"}}),
    );
    let err = client(&server, Duration::from_secs(2))
        .request("pane.get", json!({}))
        .unwrap_err();
    assert!(
        matches!(err, HerdrError::Api { ref code, .. } if code == "not_found"),
        "{err:?}"
    );
}

#[test]
fn every_request_is_its_own_connection() {
    let server = FakeHerdr::start();
    let mut client = client(&server, Duration::from_secs(2));
    client.request("ping", json!({})).unwrap();
    client.request("ping", json!({})).unwrap();
    assert_eq!(server.requests("ping").len(), 2);
}

#[test]
fn a_request_times_out_when_herdr_never_answers() {
    let server = FakeHerdr::start();
    server.on("slow", |_| {
        thread::sleep(Duration::from_secs(3));
        json!({})
    });
    let start = Instant::now();
    let err = client(&server, Duration::from_millis(300))
        .request("slow", json!({}))
        .unwrap_err();
    let elapsed = start.elapsed();
    assert!(matches!(err, HerdrError::Timeout), "{err:?}");
    assert!(
        elapsed >= Duration::from_millis(250) && elapsed < Duration::from_millis(1500),
        "{elapsed:?}"
    );
}

#[test]
fn connecting_without_herdr_fails() {
    let mut server = FakeHerdr::start();
    server.stop();
    let err = client(&server, Duration::from_millis(300))
        .request("ping", json!({}))
        .unwrap_err();
    assert!(matches!(err, HerdrError::Io(_)), "{err:?}");
}

#[test]
fn concurrent_requests_all_get_answers() {
    // On Windows most back-to-back connects find every pipe instance busy.
    let server = FakeHerdr::start();
    server.on("ping", |_| json!({"pong": true}));
    let path = server.path.clone();
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            thread::spawn(move || {
                let mut client = HerdrClient::new(path, Duration::from_secs(5));
                (0..25)
                    .filter(|_| client.request("ping", json!({})).is_err())
                    .count()
            })
        })
        .collect();
    let failures: usize = workers.into_iter().map(|w| w.join().unwrap()).sum();
    assert_eq!(failures, 0);
}

#[test]
fn subscription_events_arrive_in_order() {
    let server = FakeHerdr::start();
    let mut sub = client(&server, Duration::from_secs(2))
        .subscribe(json!([{"type": "pane.created"}]))
        .unwrap();
    assert_eq!(
        server.requests("events.subscribe"),
        [json!({"subscriptions": [{"type": "pane.created"}]})]
    );
    server.push(0, json!({"event": "pane.created", "data": {"n": 1}}));
    server.push(0, json!({"event": "pane.created", "data": {"n": 2}}));
    let deadline = Instant::now() + Duration::from_secs(2);
    assert_eq!(sub.next_event(deadline).unwrap().unwrap()["data"]["n"], 1);
    assert_eq!(sub.next_event(deadline).unwrap().unwrap()["data"]["n"], 2);
}

#[test]
fn an_event_sent_with_the_ack_is_not_lost() {
    let server = FakeHerdr::start();
    server
        .send_with_ack(json!({"event": "pane.agent_status_changed", "data": {"pane_id": "w1:p1"}}));
    let mut sub = client(&server, Duration::from_secs(2))
        .subscribe(json!([]))
        .unwrap();
    let event = sub
        .next_event(Instant::now() + Duration::from_secs(1))
        .unwrap()
        .unwrap();
    assert_eq!(event["data"]["pane_id"], "w1:p1");
}

#[test]
fn next_event_waits_until_the_deadline() {
    let server = FakeHerdr::start();
    let mut sub = client(&server, Duration::from_secs(2))
        .subscribe(json!([]))
        .unwrap();
    let start = Instant::now();
    assert!(
        sub.next_event(start + Duration::from_millis(300))
            .unwrap()
            .is_none()
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(280) && elapsed < Duration::from_millis(1000),
        "{elapsed:?}"
    );

    let start = Instant::now();
    assert!(sub.next_event(start).unwrap().is_none());
    assert!(start.elapsed() < Duration::from_millis(100));
}

#[test]
fn an_event_pushed_while_waiting_arrives_at_once() {
    let server = FakeHerdr::start();
    let mut sub = client(&server, Duration::from_secs(2))
        .subscribe(json!([]))
        .unwrap();
    // A wait that times out leaves its read pending; the next wait reuses it.
    assert!(
        sub.next_event(Instant::now() + Duration::from_millis(50))
            .unwrap()
            .is_none()
    );
    let start = Instant::now();
    thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(100));
            server.push(
                0,
                json!({"event": "pane.focused", "data": {"pane_id": "w1:p2"}}),
            );
        });
        let event = sub
            .next_event(start + Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert_eq!(event["data"]["pane_id"], "w1:p2");
    });
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn a_closed_subscription_reports_closed() {
    let server = FakeHerdr::start();
    let mut sub = client(&server, Duration::from_secs(2))
        .subscribe(json!([]))
        .unwrap();
    server.close_subscription(0);
    assert!(
        sub.next_event(Instant::now() + Duration::from_secs(2))
            .is_err()
    );
    assert!(
        sub.next_event(Instant::now() + Duration::from_secs(2))
            .is_err()
    );
}

#[test]
fn dropping_a_subscription_while_a_read_is_pending_is_prompt() {
    let server = FakeHerdr::start();
    let mut sub = client(&server, Duration::from_secs(2))
        .subscribe(json!([]))
        .unwrap();
    assert!(
        sub.next_event(Instant::now() + Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    let start = Instant::now();
    drop(sub);
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "{:?}",
        start.elapsed()
    );
}
