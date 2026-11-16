//! QEMU's SHUTDOWN reason tells a reboot from inside the guest from a power-off.

use clnx::{GuestShutdown, parse_shutdown_line};

#[test]
fn a_guest_reset_is_a_reboot() {
    assert_eq!(
        parse_shutdown_line(
            r#"event SHUTDOWN at 1731700000.123456 for domain 'guest-a': {"guest":true,"reason":"guest-reset"}"#
        ),
        Some(GuestShutdown {
            domain: "guest-a".to_owned(),
            reset: true,
            guest: true,
        })
    );
}

#[test]
fn a_guest_shutdown_is_a_power_off() {
    let event = parse_shutdown_line(
        r#"event SHUTDOWN at 1731700000.000001 for domain 'guest-a': {"guest":true,"reason":"guest-shutdown"}"#,
    )
    .expect("a SHUTDOWN line parses");
    assert!(!event.reset);
    assert!(event.guest);
}

#[test]
fn a_shutdown_without_details_is_never_a_reset_and_other_lines_are_ignored() {
    let bare = parse_shutdown_line("event SHUTDOWN at 1.000000 for domain 'guest-a': <null>")
        .expect("a bare SHUTDOWN parses");
    assert!(!bare.reset && !bare.guest);
    assert_eq!(
        parse_shutdown_line(r#"event RESET at 1.000000 for domain 'guest-a': {"guest":true}"#),
        None
    );
    assert_eq!(parse_shutdown_line("events received: 1"), None);
}
