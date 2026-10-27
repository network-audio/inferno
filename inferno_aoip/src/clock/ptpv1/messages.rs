//! PTPv1 (IEEE 1588-2002) messages, as far as a follower and a leader need them.
//! Layout as in Statime's PTPv1 support, which works with Dante devices.

pub const HEADER_LEN: usize = 40;
pub const SYNC_LEN: usize = HEADER_LEN + 84;
pub const FOLLOW_UP_LEN: usize = HEADER_LEN + 12;
pub const DELAY_RESP_LEN: usize = HEADER_LEN + 20;

const SUBDOMAIN_DEFAULT: [u8; 16] = *b"_DFLT\0\0\0\0\0\0\0\0\0\0\0";
/// header flags (byte 35): the precise origin timestamp of a Sync comes in a Follow_Up
const FLAG_ASSIST: u8 = 1 << 3;
const MESSAGE_TYPE_EVENT: u8 = 1;
const MESSAGE_TYPE_GENERAL: u8 = 2;
const COMMUNICATION_TECHNOLOGY_ETHERNET: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
  Sync = 0,
  DelayReq = 1,
  FollowUp = 2,
  DelayResp = 3,
}

/// Identity of a PTP port: clock UUID (usually a MAC address) and port number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortId {
  pub uuid: [u8; 6],
  pub port: u16,
}

/// Nanoseconds since the PTP epoch.
pub type PtpNs = i64;

fn timestamp_to_ns(buf: &[u8]) -> PtpNs {
  let seconds = u32::from_be_bytes(buf[0..4].try_into().unwrap());
  let nanos = u32::from_be_bytes(buf[4..8].try_into().unwrap());
  (seconds as i64) * 1_000_000_000 + nanos as i64
}

fn ns_to_timestamp(ns: PtpNs, buf: &mut [u8]) {
  let ns = ns.max(0);
  buf[0..4].copy_from_slice(&((ns / 1_000_000_000) as u32).to_be_bytes());
  buf[4..8].copy_from_slice(&((ns % 1_000_000_000) as u32).to_be_bytes());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
  pub source: PortId,
  pub sequence_id: u16,
  pub control: Control,
  pub assist: bool,
}

/// The leader's properties a Sync announces, compared to choose the best leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grandmaster {
  pub uuid: [u8; 6],
  pub port: u16,
  pub stratum: u8,
  pub identifier: [u8; 4],
  pub variance: i16,
  pub preferred: bool,
}

impl Grandmaster {
  /// Ordering key: smaller is better (preferred first, then stratum, variance, identity).
  pub fn rank(&self) -> (bool, u8, i16, [u8; 6], u16) {
    (!self.preferred, self.stratum, self.variance, self.uuid, self.port)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
  /// `origin` is precise unless the header's `assist` flag is set (then it comes in a Follow_Up)
  Sync {
    origin: PtpNs,
    grandmaster: Grandmaster,
  },
  DelayReq,
  FollowUp {
    associated_sequence_id: u16,
    precise_origin: PtpNs,
  },
  DelayResp {
    receipt: PtpNs,
    requester: PortId,
    requester_sequence_id: u16,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message {
  pub header: Header,
  pub body: Body,
}

/// Parses a PTPv1 message. `None` for other PTP versions, other subdomains, other message types and
/// malformed packets.
pub fn parse(buf: &[u8]) -> Option<Message> {
  if buf.len() < HEADER_LEN || buf[0..2] != [0, 1] || buf[4..20] != SUBDOMAIN_DEFAULT {
    return None;
  }
  let control = match buf[32] {
    0 => Control::Sync,
    1 => Control::DelayReq,
    2 => Control::FollowUp,
    3 => Control::DelayResp,
    _ => return None,
  };
  let header = Header {
    source: PortId {
      uuid: buf[22..28].try_into().unwrap(),
      port: u16::from_be_bytes(buf[28..30].try_into().unwrap()),
    },
    sequence_id: u16::from_be_bytes(buf[30..32].try_into().unwrap()),
    control,
    assist: buf[35] & FLAG_ASSIST != 0,
  };
  let b = &buf[HEADER_LEN..];
  let body = match control {
    Control::Sync if buf.len() >= SYNC_LEN => {
      let gm = &b[12..40];
      Body::Sync {
        origin: timestamp_to_ns(&b[0..8]),
        grandmaster: Grandmaster {
          uuid: gm[2..8].try_into().unwrap(),
          port: u16::from_be_bytes(gm[8..10].try_into().unwrap()),
          stratum: gm[15],
          identifier: gm[16..20].try_into().unwrap(),
          variance: i16::from_be_bytes(gm[22..24].try_into().unwrap()),
          preferred: gm[25] != 0,
        },
      }
    }
    Control::DelayReq if buf.len() >= SYNC_LEN => Body::DelayReq,
    Control::FollowUp if buf.len() >= FOLLOW_UP_LEN => Body::FollowUp {
      associated_sequence_id: u16::from_be_bytes(b[2..4].try_into().unwrap()),
      precise_origin: timestamp_to_ns(&b[4..12]),
    },
    Control::DelayResp if buf.len() >= DELAY_RESP_LEN => Body::DelayResp {
      receipt: timestamp_to_ns(&b[0..8]),
      requester: PortId {
        uuid: b[10..16].try_into().unwrap(),
        port: u16::from_be_bytes(b[16..18].try_into().unwrap()),
      },
      requester_sequence_id: u16::from_be_bytes(b[18..20].try_into().unwrap()),
    },
    _ => return None,
  };
  Some(Message { header, body })
}

fn write_header(buf: &mut [u8], source: PortId, sequence_id: u16, control: Control, assist: bool) {
  buf[0..2].copy_from_slice(&1u16.to_be_bytes());
  buf[2..4].copy_from_slice(&1u16.to_be_bytes());
  buf[4..20].copy_from_slice(&SUBDOMAIN_DEFAULT);
  buf[20] = match control {
    Control::Sync | Control::DelayReq => MESSAGE_TYPE_EVENT,
    _ => MESSAGE_TYPE_GENERAL,
  };
  buf[21] = COMMUNICATION_TECHNOLOGY_ETHERNET;
  buf[22..28].copy_from_slice(&source.uuid);
  buf[28..30].copy_from_slice(&source.port.to_be_bytes());
  buf[30..32].copy_from_slice(&sequence_id.to_be_bytes());
  buf[32] = control as u8;
  buf[35] = if assist { FLAG_ASSIST } else { 0 };
}

/// A Delay_Req of a follower which follows `parent`.
pub fn delay_req(source: PortId, sequence_id: u16, parent: &Grandmaster) -> [u8; SYNC_LEN] {
  let mut buf = [0u8; SYNC_LEN];
  write_header(&mut buf, source, sequence_id, Control::DelayReq, false);
  let b = &mut buf[HEADER_LEN..];
  write_grandmaster(&mut b[12..40], parent);
  b[43] = 0x7f; // syncInterval: not a leader
  b[55] = 255; // localClockStratum: follower only
  b[56..60].copy_from_slice(b"DFLT");
  b[61] = COMMUNICATION_TECHNOLOGY_ETHERNET;
  b[62..68].copy_from_slice(&parent.uuid);
  b[70..72].copy_from_slice(&parent.port.to_be_bytes());
  buf
}

fn write_grandmaster(buf: &mut [u8], gm: &Grandmaster) {
  buf[1] = COMMUNICATION_TECHNOLOGY_ETHERNET;
  buf[2..8].copy_from_slice(&gm.uuid);
  buf[8..10].copy_from_slice(&gm.port.to_be_bytes());
  buf[15] = gm.stratum;
  buf[16..20].copy_from_slice(&gm.identifier);
  buf[22..24].copy_from_slice(&gm.variance.to_be_bytes());
  buf[25] = gm.preferred as u8;
}

/// A Sync of a leader which is its own grandmaster, sending one every 2^`log_sync_interval` seconds.
pub fn sync(
  source: PortId,
  sequence_id: u16,
  origin: PtpNs,
  assist: bool,
  gm: &Grandmaster,
  log_sync_interval: i8,
) -> [u8; SYNC_LEN] {
  let mut buf = [0u8; SYNC_LEN];
  write_header(&mut buf, source, sequence_id, Control::Sync, assist);
  let b = &mut buf[HEADER_LEN..];
  ns_to_timestamp(origin, &mut b[0..8]);
  write_grandmaster(&mut b[12..40], gm);
  b[43] = log_sync_interval as u8;
  b[55] = gm.stratum;
  b[56..60].copy_from_slice(&gm.identifier);
  b[61] = COMMUNICATION_TECHNOLOGY_ETHERNET;
  b[62..68].copy_from_slice(&source.uuid);
  b[70..72].copy_from_slice(&source.port.to_be_bytes());
  buf
}

pub fn follow_up(
  source: PortId,
  sequence_id: u16,
  associated_sequence_id: u16,
  precise_origin: PtpNs,
) -> [u8; FOLLOW_UP_LEN] {
  let mut buf = [0u8; FOLLOW_UP_LEN];
  write_header(&mut buf, source, sequence_id, Control::FollowUp, false);
  let b = &mut buf[HEADER_LEN..];
  b[2..4].copy_from_slice(&associated_sequence_id.to_be_bytes());
  ns_to_timestamp(precise_origin, &mut b[4..12]);
  buf
}

pub fn delay_resp(
  source: PortId,
  sequence_id: u16,
  receipt: PtpNs,
  requester: PortId,
  requester_sequence_id: u16,
) -> [u8; DELAY_RESP_LEN] {
  let mut buf = [0u8; DELAY_RESP_LEN];
  write_header(&mut buf, source, sequence_id, Control::DelayResp, false);
  let b = &mut buf[HEADER_LEN..];
  ns_to_timestamp(receipt, &mut b[0..8]);
  b[9] = COMMUNICATION_TECHNOLOGY_ETHERNET;
  b[10..16].copy_from_slice(&requester.uuid);
  b[16..18].copy_from_slice(&requester.port.to_be_bytes());
  b[18..20].copy_from_slice(&requester_sequence_id.to_be_bytes());
  buf
}

#[cfg(test)]
mod tests {
  use super::*;

  const LEADER: PortId = PortId { uuid: [0x00, 0x1d, 0xc1, 1, 2, 3], port: 1 };
  const FOLLOWER: PortId = PortId { uuid: [0x02, 0, 0, 4, 5, 6], port: 7 };
  const GM: Grandmaster = Grandmaster {
    uuid: [0x00, 0x1d, 0xc1, 1, 2, 3],
    port: 1,
    stratum: 4,
    identifier: *b"DFLT",
    variance: -4000,
    preferred: true,
  };

  #[test]
  fn sync_round_trip() {
    let buf = sync(LEADER, 42, 1_700_000_000_123_456_789, true, &GM, -3);
    let m = parse(&buf).unwrap();
    assert_eq!(
      m.header,
      Header { source: LEADER, sequence_id: 42, control: Control::Sync, assist: true }
    );
    assert_eq!(m.body, Body::Sync { origin: 1_700_000_000_123_456_789, grandmaster: GM });
  }

  #[test]
  fn follow_up_round_trip() {
    let m = parse(&follow_up(LEADER, 43, 42, 5_000_000_001)).unwrap();
    assert_eq!(m.header.control, Control::FollowUp);
    assert_eq!(m.body, Body::FollowUp { associated_sequence_id: 42, precise_origin: 5_000_000_001 });
  }

  #[test]
  fn delay_req_and_resp_round_trip() {
    let m = parse(&delay_req(FOLLOWER, 9, &GM)).unwrap();
    assert_eq!(m.header.source, FOLLOWER);
    assert_eq!(m.body, Body::DelayReq);
    let m = parse(&delay_resp(LEADER, 10, 77, FOLLOWER, 9)).unwrap();
    assert_eq!(m.body, Body::DelayResp { receipt: 77, requester: FOLLOWER, requester_sequence_id: 9 });
  }

  #[test]
  fn wire_layout_matches_ieee_1588_2002() {
    let buf = follow_up(LEADER, 43, 0x1234, 1_000_000_002);
    assert_eq!(&buf[0..4], &[0, 1, 0, 1], "versionPTP, versionNetwork");
    assert_eq!(&buf[4..9], b"_DFLT");
    assert_eq!(buf[20], 2, "general message");
    assert_eq!(&buf[22..28], &LEADER.uuid);
    assert_eq!(buf[32], 2, "control: Follow_Up");
    assert_eq!(&buf[42..44], &[0x12, 0x34], "associatedSequenceId after 2 reserved bytes");
    assert_eq!(&buf[44..52], &[0, 0, 0, 1, 0, 0, 0, 2], "preciseOriginTimestamp");
    let buf = sync(LEADER, 1, 0, true, &GM, -3);
    assert_eq!(buf[35], 0x08, "PTP_ASSIST flag");
    assert_eq!(buf[83], (-3i8) as u8, "syncInterval");
    assert_eq!(buf[20], 1, "event message");
  }

  #[test]
  fn rejects_other_versions_and_short_packets() {
    let mut buf = sync(LEADER, 1, 0, false, &GM, -3);
    assert!(parse(&buf[..SYNC_LEN - 1]).is_none());
    buf[1] = 2; // PTPv2
    assert!(parse(&buf).is_none());
  }

  #[test]
  fn preferred_leader_ranks_first() {
    let other = Grandmaster { preferred: false, stratum: 1, ..GM };
    assert!(GM.rank() < other.rank());
    let worse_stratum = Grandmaster { stratum: 5, ..GM };
    assert!(GM.rank() < worse_stratum.rank());
  }
}
