//! The `Store` conformance suite: what any backend must do (versions, a
//! lost conditional write, scan, expiry). Run here against SQLite; a new
//! backend calls `conformance` with its own store.

use rlord_relay::store::{now, Conflict, SqliteStore, Store, ABSENT};

fn conformance(s: &dyn Store) {
    // Absent records.
    assert_eq!(s.get("host", "a").unwrap(), None);
    assert_eq!(s.delete("host", "a", None).unwrap(), Ok(()));

    // Versions start at 1 and grow per write.
    assert_eq!(s.put("host", "a", b"one", None, None).unwrap(), Ok(1));
    let r = s.get("host", "a").unwrap().unwrap();
    assert_eq!((r.value.as_slice(), r.version), (&b"one"[..], 1));
    assert_eq!(s.put("host", "a", b"two", None, Some(1)).unwrap(), Ok(2));
    assert_eq!(s.get("host", "a").unwrap().unwrap().version, 2);

    // A lost conditional write changes nothing.
    assert_eq!(
        s.put("host", "a", b"stale", None, Some(1)).unwrap(),
        Err(Conflict)
    );
    assert_eq!(
        s.put("host", "a", b"new", None, ABSENT).unwrap(),
        Err(Conflict)
    );
    let r = s.get("host", "a").unwrap().unwrap();
    assert_eq!((r.value.as_slice(), r.version), (&b"two"[..], 2));
    assert_eq!(
        s.put("host", "b", b"x", None, Some(3)).unwrap(),
        Err(Conflict)
    );
    assert_eq!(s.get("host", "b").unwrap(), None);
    // Create-only succeeds once.
    assert_eq!(s.put("host", "b", b"x", None, ABSENT).unwrap(), Ok(1));
    assert_eq!(
        s.put("host", "b", b"y", None, ABSENT).unwrap(),
        Err(Conflict)
    );
    // Two writers from the same version: one wins.
    let v = s.get("host", "b").unwrap().unwrap().version;
    let first = s.put("host", "b", b"w1", None, Some(v)).unwrap();
    let second = s.put("host", "b", b"w2", None, Some(v)).unwrap();
    assert_eq!((first, second), (Ok(2), Err(Conflict)));
    assert_eq!(s.get("host", "b").unwrap().unwrap().value, b"w1");

    // Conditional delete.
    assert_eq!(s.delete("host", "b", Some(1)).unwrap(), Err(Conflict));
    assert!(s.get("host", "b").unwrap().is_some());
    assert_eq!(s.delete("host", "b", Some(2)).unwrap(), Ok(()));
    assert_eq!(s.get("host", "b").unwrap(), None);
    assert_eq!(s.delete("host", "b", Some(2)).unwrap(), Err(Conflict));
    assert_eq!(s.delete("host", "b", ABSENT).unwrap(), Ok(()));

    // Namespaces are separate.
    assert_eq!(s.get("device", "a").unwrap(), None);
    assert_eq!(s.put("device", "a", b"d", None, ABSENT).unwrap(), Ok(1));
    assert_eq!(s.get("host", "a").unwrap().unwrap().value, b"two");

    // Prefix scan, in key order, this namespace only.
    for k in ["h1/d2", "h1/d1", "h2/d1", "h1x"] {
        s.put("device", k, k.as_bytes(), None, None)
            .unwrap()
            .unwrap();
    }
    let keys = |p: &str| -> Vec<String> {
        s.scan("device", p)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    };
    assert_eq!(keys("h1/"), ["h1/d1", "h1/d2"]);
    assert_eq!(keys("h1"), ["h1/d1", "h1/d2", "h1x"]);
    assert_eq!(keys("h3/"), Vec::<String>::new());
    assert_eq!(keys("").len(), 5);
    // Characters that are special in SQL LIKE are plain in a prefix.
    s.put("device", "a%b", b"", None, None).unwrap().unwrap();
    s.put("device", "a_b", b"", None, None).unwrap().unwrap();
    assert_eq!(keys("a%"), ["a%b"]);
    assert_eq!(keys("a_"), ["a_b"]);
    let (_, r) = &s.scan("device", "h2/").unwrap()[0];
    assert_eq!((r.value.as_slice(), r.version), (&b"h2/d1"[..], 1));

    // Expiry: a past `expires_at` reads as absent everywhere.
    let t = now();
    s.put("token", "old", b"o", Some(t - 1), None)
        .unwrap()
        .unwrap();
    s.put("token", "new", b"n", Some(t + 3600), None)
        .unwrap()
        .unwrap();
    assert_eq!(s.get("token", "old").unwrap(), None);
    let r = s.get("token", "new").unwrap().unwrap();
    assert_eq!(r.expires_at, Some(t + 3600));
    assert_eq!(
        s.scan("token", "")
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect::<Vec<_>>(),
        ["new"]
    );
    // An expired record is absent to a conditional write too, and its
    // successor starts again at version 1.
    assert_eq!(
        s.put("token", "old", b"again", None, Some(1)).unwrap(),
        Err(Conflict)
    );
    s.put("token", "gone", b"g", Some(t - 1), None)
        .unwrap()
        .unwrap();
    assert_eq!(s.put("token", "gone", b"g2", None, ABSENT).unwrap(), Ok(1));
    // Cleanup removes only what expired.
    assert_eq!(s.purge_expired().unwrap(), 1);
    assert!(s.get("token", "new").unwrap().is_some());
    assert!(s.get("token", "gone").unwrap().is_some());
}

#[test]
fn sqlite_passes_the_conformance_suite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.db");
    let s = SqliteStore::open(&path).unwrap();
    conformance(&s);
    // 0600, and what was written survives a reopen.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    drop(s);
    let s = SqliteStore::open(&path).unwrap();
    assert_eq!(s.get("host", "a").unwrap().unwrap().version, 2);
}

#[test]
fn two_handles_on_one_file_see_each_others_writes() {
    // `serve` and the CLI open the same database.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.db");
    let a = SqliteStore::open(&path).unwrap();
    let b = SqliteStore::open(&path).unwrap();
    a.put("host", "k", b"1", None, ABSENT).unwrap().unwrap();
    assert_eq!(
        b.put("host", "k", b"2", None, ABSENT).unwrap(),
        Err(Conflict)
    );
    assert_eq!(b.put("host", "k", b"2", None, Some(1)).unwrap(), Ok(2));
    assert_eq!(a.get("host", "k").unwrap().unwrap().value, b"2");
}
