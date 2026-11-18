//! The relay link's binary frames (at most [`MAX_BINARY`] bytes). The first
//! byte is the lane; the payload is the content core's bytes, which the
//! relay forwards untouched and never parses.
//!
//! | Direction | Lane 1 (live) | Lane 2 (mailbox) |
//! | --- | --- | --- |
//! | device → relay | `1 \| payload` | `2 \| payload` |
//! | relay → device | `1 \| payload` | `2 \| seq u64 \| payload` |
//! | host → relay | `1 \| link u32 \| payload` | `2 \| device fp (32) \| payload` |
//! | relay → host | `1 \| link u32 \| payload` | `2 \| device fp (32) \| seq u64 \| payload` |
//!
//! Integers are big-endian. A live frame goes at once to the other end of
//! its link (`undeliverable` when there is none); a mailbox frame is queued
//! for the device (from the host) or the host (from a device), numbered,
//! and kept until acknowledged.

use crate::{Error, Result};

/// Largest binary frame on the relay link.
pub const MAX_BINARY: usize = 20 * 1024;

pub const LANE_LIVE: u8 = 1;
pub const LANE_MAILBOX: u8 = 2;

fn check(b: &[u8]) -> Result<()> {
    if b.len() > MAX_BINARY {
        return Err(Error::Frame("frame too large"));
    }
    Ok(())
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes(s.try_into().unwrap()))
        .ok_or(Error::Frame("frame too short"))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64> {
    b.get(at..at + 8)
        .map(|s| u64::from_be_bytes(s.try_into().unwrap()))
        .ok_or(Error::Frame("frame too short"))
}

fn fp_at(b: &[u8], at: usize) -> Result<[u8; 32]> {
    b.get(at..at + 32)
        .map(|s| s.try_into().unwrap())
        .ok_or(Error::Frame("frame too short"))
}

/// Device to relay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FromDevice {
    Live(Vec<u8>),
    Mailbox(Vec<u8>),
}

impl FromDevice {
    pub fn encode(&self) -> Vec<u8> {
        let (lane, p) = match self {
            FromDevice::Live(p) => (LANE_LIVE, p),
            FromDevice::Mailbox(p) => (LANE_MAILBOX, p),
        };
        let mut b = Vec::with_capacity(1 + p.len());
        b.push(lane);
        b.extend_from_slice(p);
        b
    }

    pub fn decode(b: &[u8]) -> Result<FromDevice> {
        check(b)?;
        match b.first() {
            Some(&LANE_LIVE) => Ok(FromDevice::Live(b[1..].to_vec())),
            Some(&LANE_MAILBOX) => Ok(FromDevice::Mailbox(b[1..].to_vec())),
            _ => Err(Error::Frame("unknown lane")),
        }
    }
}

/// Relay to device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToDevice {
    Live(Vec<u8>),
    Mailbox { seq: u64, payload: Vec<u8> },
}

impl ToDevice {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ToDevice::Live(p) => [&[LANE_LIVE][..], p].concat(),
            ToDevice::Mailbox { seq, payload } => {
                [&[LANE_MAILBOX][..], &seq.to_be_bytes(), payload].concat()
            }
        }
    }

    pub fn decode(b: &[u8]) -> Result<ToDevice> {
        check(b)?;
        match b.first() {
            Some(&LANE_LIVE) => Ok(ToDevice::Live(b[1..].to_vec())),
            Some(&LANE_MAILBOX) => Ok(ToDevice::Mailbox {
                seq: u64_at(b, 1)?,
                payload: b[9..].to_vec(),
            }),
            _ => Err(Error::Frame("unknown lane")),
        }
    }
}

/// Host to relay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FromHost {
    Live { link: u32, payload: Vec<u8> },
    Mailbox { device: [u8; 32], payload: Vec<u8> },
}

impl FromHost {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            FromHost::Live { link, payload } => {
                [&[LANE_LIVE][..], &link.to_be_bytes(), payload].concat()
            }
            FromHost::Mailbox { device, payload } => {
                [&[LANE_MAILBOX][..], device, payload].concat()
            }
        }
    }

    pub fn decode(b: &[u8]) -> Result<FromHost> {
        check(b)?;
        match b.first() {
            Some(&LANE_LIVE) => Ok(FromHost::Live {
                link: u32_at(b, 1)?,
                payload: b[5..].to_vec(),
            }),
            Some(&LANE_MAILBOX) => Ok(FromHost::Mailbox {
                device: fp_at(b, 1)?,
                payload: b[33..].to_vec(),
            }),
            _ => Err(Error::Frame("unknown lane")),
        }
    }
}

/// Relay to host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToHost {
    Live {
        link: u32,
        payload: Vec<u8>,
    },
    Mailbox {
        device: [u8; 32],
        seq: u64,
        payload: Vec<u8>,
    },
}

impl ToHost {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ToHost::Live { link, payload } => {
                [&[LANE_LIVE][..], &link.to_be_bytes(), payload].concat()
            }
            ToHost::Mailbox {
                device,
                seq,
                payload,
            } => [&[LANE_MAILBOX][..], device, &seq.to_be_bytes(), payload].concat(),
        }
    }

    pub fn decode(b: &[u8]) -> Result<ToHost> {
        check(b)?;
        match b.first() {
            Some(&LANE_LIVE) => Ok(ToHost::Live {
                link: u32_at(b, 1)?,
                payload: b[5..].to_vec(),
            }),
            Some(&LANE_MAILBOX) => Ok(ToHost::Mailbox {
                device: fp_at(b, 1)?,
                seq: u64_at(b, 33)?,
                payload: b[41..].to_vec(),
            }),
            _ => Err(Error::Frame("unknown lane")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for f in [FromDevice::Live(vec![1, 2]), FromDevice::Mailbox(vec![])] {
            assert_eq!(FromDevice::decode(&f.encode()).unwrap(), f);
        }
        for f in [
            ToDevice::Live(vec![3]),
            ToDevice::Mailbox {
                seq: 9,
                payload: vec![4, 5],
            },
        ] {
            assert_eq!(ToDevice::decode(&f.encode()).unwrap(), f);
        }
        for f in [
            FromHost::Live {
                link: 0x01020304,
                payload: vec![6],
            },
            FromHost::Mailbox {
                device: [7; 32],
                payload: vec![8],
            },
        ] {
            assert_eq!(FromHost::decode(&f.encode()).unwrap(), f);
        }
        let f = ToHost::Mailbox {
            device: [7; 32],
            seq: 3,
            payload: vec![1],
        };
        assert_eq!(f.encode()[..2], [2, 7]);
        assert_eq!(ToHost::decode(&f.encode()).unwrap(), f);
        assert_eq!(
            FromHost::Live {
                link: 1,
                payload: vec![9]
            }
            .encode(),
            vec![1, 0, 0, 0, 1, 9]
        );
    }

    #[test]
    fn rejects() {
        assert!(FromDevice::decode(&[]).is_err());
        assert!(FromDevice::decode(&[3, 1]).is_err());
        assert!(FromDevice::decode(&vec![1; MAX_BINARY + 1]).is_err());
        assert!(FromHost::decode(&[1, 0, 0]).is_err());
        assert!(FromHost::decode(&[2; 20]).is_err());
        assert!(ToHost::decode(&[2; 40]).is_err());
        assert!(ToDevice::decode(&[2, 0, 0]).is_err());
    }
}
