//! Multi-frame reassembly for diagnostic transports.
//!
//! CAN and CAN FD use ISO-TP (ISO 15765-2); LIN uses the transport protocol of
//! ISO 17987-2, whose diagnostic frames carry the node address first and the
//! protocol control information in the second byte.  Both split one diagnostic
//! message over several frames, so the payload only appears once the
//! consecutive frames have been collected.
//!
//! The reassembler is transport agnostic: a caller classifies each frame with
//! [`TransportFrame`] and feeds it with a key that identifies the session.  A
//! session is a bus, channel and frame identifier (plus a direction, because a
//! request and a response share the frame identifiers on LIN).

use std::collections::HashMap;

use crate::uds::IsoTpFrame;

/// Which transport carried a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transport {
    Can,
    Lin,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Can => "iso-tp",
            Self::Lin => "lin-tp",
        }
    }
}

/// One transport frame, whatever bus carried it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportFrame {
    /// The whole message fits in this frame.
    Single { data: Vec<u8> },
    /// First frame of a segmented message.
    First { length: usize, data: Vec<u8> },
    /// Consecutive frame with its 4-bit sequence number.
    Consecutive { sequence: u8, data: Vec<u8> },
    /// Flow control frame; carries no payload.
    FlowControl,
}

/// Session key: one transport session per bus, channel, identifier and direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub transport: Transport,
    pub channel: u16,
    /// CAN frame identifier, or the LIN frame identifier (0x3c/0x3d for diagnostics).
    pub frame_id: u32,
    /// 0 for a request or a receive, 1 for a response or a transmit.
    pub direction: u8,
    /// LIN node address, so two slaves on one diagnostic frame stay apart.
    pub node: u8,
}

impl SessionKey {
    pub fn new(transport: Transport, channel: u16, frame_id: u32, direction: u8) -> Self {
        Self {
            transport,
            channel,
            frame_id,
            direction,
            node: 0,
        }
    }

    /// Attach the node address of a LIN diagnostic frame.
    pub fn with_node(mut self, node: u8) -> Self {
        self.node = node;
        self
    }
}

/// What [`Reassembler::push`] learned from one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reassembly {
    /// A complete message, ready for the service layer.
    Complete { payload: Vec<u8>, frames: usize },
    /// More frames are needed; `received` counts the collected payload octets.
    Incomplete { expected: usize, received: usize },
    /// The frame carried no payload, such as a flow control frame.
    Control,
    /// The frame broke the sequence and its session was dropped.
    Desync {
        expected_sequence: u8,
        actual_sequence: u8,
    },
}

#[derive(Debug, Clone)]
struct Pending {
    payload: Vec<u8>,
    /// Total message length declared by the first frame.
    length: usize,
    /// Sequence number the next consecutive frame must carry.
    next_sequence: u8,
    /// How many frames belong to this message so far.
    frames: usize,
}

/// Stateful reassembler for CAN ISO-TP and LIN transport frames.
#[derive(Debug, Default)]
pub struct Reassembler {
    pending: HashMap<SessionKey, Pending>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget a session, for example when the trace rewinds.
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// Feed one classified frame and return what it completed.
    pub fn push(&mut self, key: SessionKey, frame: TransportFrame) -> Reassembly {
        match frame {
            TransportFrame::Single { data } => {
                self.pending.remove(&key);
                Reassembly::Complete {
                    payload: data,
                    frames: 1,
                }
            }
            TransportFrame::FlowControl => Reassembly::Control,
            TransportFrame::First { length, data } => {
                if length == 0 {
                    self.pending.remove(&key);
                    return Reassembly::Complete {
                        payload: data,
                        frames: 1,
                    };
                }
                let mut payload = data;
                payload.truncate(length);
                if payload.len() >= length {
                    self.pending.remove(&key);
                    return Reassembly::Complete { payload, frames: 1 };
                }
                let received = payload.len();
                self.pending.insert(
                    key,
                    Pending {
                        payload,
                        length,
                        next_sequence: 1,
                        frames: 1,
                    },
                );
                Reassembly::Incomplete {
                    expected: length,
                    received,
                }
            }
            TransportFrame::Consecutive { sequence, data } => {
                let Some(mut pending) = self.pending.remove(&key) else {
                    // Without a first frame there is nothing to append to.
                    return Reassembly::Control;
                };
                if sequence != pending.next_sequence {
                    return Reassembly::Desync {
                        expected_sequence: pending.next_sequence,
                        actual_sequence: sequence,
                    };
                }
                pending.payload.extend_from_slice(&data);
                pending.frames += 1;
                let remaining = pending.length.saturating_sub(pending.payload.len());
                if remaining == 0 {
                    pending.payload.truncate(pending.length);
                    return Reassembly::Complete {
                        payload: pending.payload,
                        frames: pending.frames,
                    };
                }
                pending.next_sequence = (sequence + 1) & 0x0F;
                let received = pending.payload.len();
                let expected = pending.length;
                self.pending.insert(key, pending);
                Reassembly::Incomplete { expected, received }
            }
        }
    }
}

/// Classify a CAN payload as ISO-TP framing.
pub fn classify_can(payload: &[u8]) -> Option<TransportFrame> {
    Some(match crate::uds::decode_isotp(payload)? {
        IsoTpFrame::Single { data } => TransportFrame::Single { data },
        IsoTpFrame::First { length, data } => TransportFrame::First {
            length: length as usize,
            data,
        },
        IsoTpFrame::Consecutive { sequence, data } => {
            TransportFrame::Consecutive { sequence, data }
        }
        IsoTpFrame::FlowControl { .. } => TransportFrame::FlowControl,
    })
}

/// LIN diagnostic frame identifiers: master request and slave response.
pub const LIN_MASTER_REQUEST: u32 = 0x3C;
pub const LIN_SLAVE_RESPONSE: u32 = 0x3D;

/// Classify a LIN diagnostic frame payload.
///
/// A diagnostic frame carries the node address in its first data octet and the
/// protocol control information in the second (ISO 17987-2).  Returns the node
/// address beside the frame so callers can report which ECU answered.
pub fn classify_lin(payload: &[u8]) -> Option<(u8, TransportFrame)> {
    let (&node, rest) = payload.split_first()?;
    let (&pci, data) = rest.split_first()?;
    let frame = match pci >> 4 {
        // Single frame: the low nibble counts the payload octets.
        0 => {
            let mut data = data.to_vec();
            let length = usize::from(pci & 0x0F);
            if length > 0 {
                data.truncate(length.min(data.len()));
            } else {
                // A zero length means "rest of the frame".
                while data.last() == Some(&0xFF) || data.last() == Some(&0x00) {
                    data.pop();
                }
            }
            TransportFrame::Single { data }
        }
        // First frame: a 12-bit length, high nibble first.
        1 => {
            let (&low, data) = data.split_first()?;
            let length = usize::from(pci & 0x0F) << 8 | usize::from(low);
            TransportFrame::First {
                length,
                data: data.to_vec(),
            }
        }
        // Consecutive frame: the low nibble is the sequence number.
        2 => TransportFrame::Consecutive {
            sequence: pci & 0x0F,
            data: data.to_vec(),
        },
        // Flow control and everything else carry no diagnostic payload.
        _ => TransportFrame::FlowControl,
    };
    Some((node, frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn can_key() -> SessionKey {
        SessionKey::new(Transport::Can, 1, 0x7E0, 0)
    }

    #[test]
    fn reassembles_a_segmented_can_response() {
        let mut reassembler = Reassembler::new();
        // 0x62 0xF1 0x90 followed by 17 octets: 20 in total, three frames of
        // six, seven and seven octets.
        let mut message = vec![0x62, 0xF1, 0x90];
        message.extend((0u8..17).collect::<Vec<_>>());
        let length = message.len();
        let first = format!("10{:02X}", length);
        let mut payload = hex(&first);
        payload.extend_from_slice(&message[..6]);
        let outcome = reassembler.push(can_key(), classify_can(&payload).expect("first"));
        assert_eq!(
            outcome,
            Reassembly::Incomplete {
                expected: length,
                received: 6
            }
        );

        let mut payload = vec![0x21];
        payload.extend_from_slice(&message[6..13]);
        let outcome = reassembler.push(can_key(), classify_can(&payload).expect("second"));
        assert_eq!(
            outcome,
            Reassembly::Incomplete {
                expected: length,
                received: 13
            }
        );

        let mut payload = vec![0x22];
        payload.extend_from_slice(&message[13..20]);
        let outcome = reassembler.push(can_key(), classify_can(&payload).expect("third"));
        assert_eq!(
            outcome,
            Reassembly::Complete {
                payload: message.clone(),
                frames: 3
            }
        );

        // A single frame needs no state at all.
        let outcome = reassembler.push(can_key(), classify_can(&[0x03, 0x22, 0xF1, 0x90]).unwrap());
        assert_eq!(
            outcome,
            Reassembly::Complete {
                payload: vec![0x22, 0xF1, 0x90],
                frames: 1
            }
        );
    }

    #[test]
    fn a_sequence_gap_drops_the_session() {
        let mut reassembler = Reassembler::new();
        reassembler.push(
            can_key(),
            classify_can(&[0x10, 0x0A, 0x62, 0xF1]).expect("first"),
        );
        let outcome = reassembler.push(
            can_key(),
            classify_can(&[0x25, 0x90, 0x00]).expect("out of order"),
        );
        assert_eq!(
            outcome,
            Reassembly::Desync {
                expected_sequence: 1,
                actual_sequence: 5
            }
        );
    }

    #[test]
    fn keeps_two_lin_nodes_apart() {
        let mut reassembler = Reassembler::new();
        let first = SessionKey::new(Transport::Lin, 1, LIN_SLAVE_RESPONSE, 1).with_node(0x0A);
        let second = SessionKey::new(Transport::Lin, 1, LIN_SLAVE_RESPONSE, 1).with_node(0x0B);
        reassembler.push(
            first,
            classify_lin(&[0x0A, 0x10, 0x0A, 0x62, 0xF1, 0x90, 0x01, 0x02])
                .unwrap()
                .1,
        );
        reassembler.push(
            second,
            classify_lin(&[0x0B, 0x10, 0x0A, 0x62, 0xF1, 0x90, 0x03, 0x04])
                .unwrap()
                .1,
        );
        let outcome = reassembler.push(
            second,
            classify_lin(&[0x0B, 0x21, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A])
                .unwrap()
                .1,
        );
        assert_eq!(
            outcome,
            Reassembly::Complete {
                payload: vec![0x62, 0xF1, 0x90, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09],
                frames: 2
            }
        );
    }

    #[test]
    fn keeps_requests_and_responses_apart() {
        let mut reassembler = Reassembler::new();
        let request = SessionKey::new(Transport::Can, 1, 0x7E0, 0);
        let response = SessionKey::new(Transport::Can, 1, 0x7E8, 1);
        reassembler.push(request, classify_can(&[0x10, 0x0A, 0x22, 0xF1]).unwrap());
        reassembler.push(response, classify_can(&[0x10, 0x0A, 0x62, 0xF1]).unwrap());
        let mut payload = vec![0x21];
        payload.extend_from_slice(&[0x90, 0x01, 0x02, 0x03, 0x04, 0x05]);
        let outcome = reassembler.push(response, classify_can(&payload).unwrap());
        assert!(matches!(outcome, Reassembly::Incomplete { .. }));
    }

    #[test]
    fn reassembles_a_segmented_lin_response() {
        // LIN diagnostic frames: NAD, PCI, then 6 data octets per frame.
        let mut reassembler = Reassembler::new();
        let key = SessionKey::new(Transport::Lin, 1, LIN_SLAVE_RESPONSE, 1);
        let message = vec![0x62, 0xF1, 0x90, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        let (node, frame) =
            classify_lin(&[0x0A, 0x10, 0x0A, 0x62, 0xF1, 0x90, 0x01, 0x02]).expect("lin");
        assert_eq!(node, 0x0A);
        assert_eq!(
            frame,
            TransportFrame::First {
                length: 10,
                data: vec![0x62, 0xF1, 0x90, 0x01, 0x02]
            }
        );
        let outcome = reassembler.push(key, frame);
        assert_eq!(
            outcome,
            Reassembly::Incomplete {
                expected: 10,
                received: 5
            }
        );

        let (_, frame) =
            classify_lin(&[0x0A, 0x21, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00]).expect("lin");
        let outcome = reassembler.push(key, frame);
        assert_eq!(
            outcome,
            Reassembly::Complete {
                payload: message,
                frames: 2
            }
        );
    }

    #[test]
    fn a_lin_single_frame_carries_the_whole_message() {
        let (node, frame) =
            classify_lin(&[0x0A, 0x03, 0x22, 0xF1, 0x90, 0x00, 0x00, 0x00]).expect("lin");
        assert_eq!(node, 0x0A);
        assert_eq!(
            frame,
            TransportFrame::Single {
                data: vec![0x22, 0xF1, 0x90]
            }
        );
    }

    #[test]
    fn a_lin_flow_control_carries_no_payload() {
        let (_, frame) =
            classify_lin(&[0x0A, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]).expect("lin");
        assert_eq!(frame, TransportFrame::FlowControl);
    }

    fn hex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
}
