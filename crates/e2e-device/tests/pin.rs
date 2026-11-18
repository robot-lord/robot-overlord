//! The device's side of "one pinned relay": against hostile relays (no
//! real relay or host needed), the pin check fails before anything else
//! is sent, and no other address is tried.

use rlord_e2e_device::testing;
use rlord_e2e_device::{connect, ConnectError, Opts};
use rlord_relay_proto::{Identity, RelayAddress, Role, WrongRelay};

fn id(b: u8) -> Identity {
    Identity::from_seed([b; 32]).unwrap()
}

#[tokio::test]
async fn the_pinned_key_claimed_without_its_private_key_fails() {
    let real = id(1);
    let impostor = id(2);
    let h = testing::claiming("http://127.0.0.1:9", real.public().to_vec(), impostor).await;
    let pin = RelayAddress {
        origin: "http://127.0.0.1:9".into(),
        fingerprint: real.fingerprint(),
    };
    let opts = Opts {
        url: Some(h.url()),
        ..Opts::default()
    };
    let r = connect(&pin, &id(3), Role::Device, &[5; 32], &opts).await;
    assert_eq!(
        r.err(),
        Some(ConnectError::WrongRelay(WrongRelay::Signature))
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(h.connections(), 1, "no second attempt");
    assert_eq!(
        h.after_challenge(),
        0,
        "nothing sent after the failed check"
    );
}

#[tokio::test]
async fn another_key_or_origin_fails() {
    let real = id(1);
    let other = id(2);
    let pin = RelayAddress {
        origin: "http://127.0.0.1:9".into(),
        fingerprint: real.fingerprint(),
    };
    // Its own key, honestly signed, at the pinned origin.
    let h = testing::claiming(&pin.origin, other.public().to_vec(), other.clone()).await;
    let opts = Opts {
        url: Some(h.url()),
        ..Opts::default()
    };
    let r = connect(&pin, &id(3), Role::Pair, &[5; 16], &opts).await;
    assert_eq!(r.err(), Some(ConnectError::WrongRelay(WrongRelay::Key)));
    assert_eq!(h.after_challenge(), 0);
    // The real key, signed, but at another origin.
    let h = testing::claiming("http://127.0.0.1:8", real.public().to_vec(), real.clone()).await;
    let opts = Opts {
        url: Some(h.url()),
        ..Opts::default()
    };
    let r = connect(&pin, &id(3), Role::Pair, &[5; 16], &opts).await;
    assert_eq!(r.err(), Some(ConnectError::WrongRelay(WrongRelay::Origin)));
    assert_eq!(h.after_challenge(), 0);
}

#[tokio::test]
async fn a_redirect_is_never_followed() {
    let target = testing::claiming("http://127.0.0.1:9", id(1).public().to_vec(), id(1)).await;
    let r = testing::redirect(&target.url()).await;
    let pin = RelayAddress {
        origin: "http://127.0.0.1:9".into(),
        fingerprint: id(1).fingerprint(),
    };
    let opts = Opts {
        url: Some(r.url()),
        ..Opts::default()
    };
    let res = connect(&pin, &id(3), Role::Device, &[5; 32], &opts).await;
    assert_eq!(res.err(), Some(ConnectError::Http(302)));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(r.connections(), 1);
    assert_eq!(
        target.connections(),
        0,
        "the redirect's target was never dialled"
    );
}
