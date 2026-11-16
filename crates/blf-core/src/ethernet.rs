//! Ethernet frame dissection above the BLF object layer.
//!
//! BLF stores an `EthernetFrame` as its L2 addresses plus the upper-layer
//! payload, and an `EthernetFrameEx`/`EthernetFrameForwarded` as the complete
//! frame bytes.  This module turns either form into a layer chain with named
//! fields, so callers can show, filter or export what is inside the frame
//! instead of only the link-layer header.
//!
//! The dissector is deliberately dependency-free and never trusts the frame:
//! every read is bounded, a short or inconsistent frame stops the dissection at
//! the last layer it could read, and no field is invented.

use crate::types::{FieldValue, Object, ObjectType, StructuredObject};

/// One field of a dissected frame, tagged with the layer that carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthernetField {
    pub layer: &'static str,
    pub name: String,
    pub value: String,
}

/// The result of dissecting one Ethernet frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthernetDissection {
    /// Protocol chain, outermost first, such as `["ethernet", "ipv4", "udp"]`.
    pub layers: Vec<&'static str>,
    /// One line for a list row, such as
    /// `02:00:00:00:00:01 > ff:ff:ff:ff:ff:ff ARP 10.0.0.1`.
    pub summary: String,
    pub fields: Vec<EthernetField>,
    /// EtherType of the outermost Ethernet header, after any VLAN tag.
    pub ether_type: u16,
    /// VLAN identifiers, outermost first; empty without a VLAN tag.
    pub vlan_ids: Vec<u16>,
    /// Highest-layer protocol name, such as `SOME/IP` or `IPv4`.
    pub protocol: &'static str,
    pub destination: String,
    pub source: String,
    /// Tag text (VLAN, EDSA, R-TAG) that follows the addresses in `summary`.
    link_text: String,
    /// Protocol text that follows the protocol name in `summary`.
    detail: String,
    /// Compact searchable text: layer names followed by their values.
    search: String,
    /// Layers already written to `search`, so each one appears once.
    search_layers: Vec<&'static str>,
    /// Only record `fields` when a caller is going to show them.
    keep_fields: bool,
}

impl EthernetDissection {
    /// Compact searchable text: each layer name once, then the values.
    ///
    /// It holds the same information as [`Self::field_text`] without repeating
    /// the field names, which keeps a whole trace affordable to hold in memory.
    pub fn search_text(&self) -> &str {
        &self.search
    }

    /// Flatten the fields to `layer.name=value` pairs for search and export.
    pub fn field_text(&self) -> String {
        self.fields
            .iter()
            .map(|field| format!("{}.{}={}", field.layer, field.name, field.value))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn describe(&mut self, layer: &'static str, name: impl Into<String>, value: impl Into<String>) {
        let value = value.into();
        if !self.search_layers.contains(&layer) {
            self.search_layers.push(layer);
            if !self.search.is_empty() {
                self.search.push(' ');
            }
            self.search.push_str(layer);
        }
        self.search.push(' ');
        self.search.push_str(&value);
        if self.keep_fields {
            self.fields.push(EthernetField {
                layer,
                name: name.into(),
                value,
            });
        }
    }
}

/// The link-layer part of a frame, as BLF stores it for `EthernetFrame`.
#[derive(Debug, Clone, Copy)]
pub struct EthernetHeader<'a> {
    pub destination: [u8; 6],
    pub source: [u8; 6],
    /// EtherType after the outermost VLAN tag, e.g. `0x0800` for IPv4.
    pub ether_type: u16,
    /// VLAN tag control information values, outermost first.
    pub vlan_tci: &'a [u16],
    /// Upper-layer payload, that is everything after the link-layer header.
    pub payload: &'a [u8],
}

/// Dissect a complete Ethernet II frame, VLAN tags included.
pub fn dissect_frame(frame: &[u8]) -> Option<EthernetDissection> {
    dissect_frame_with(frame, true)
}

/// Dissect a complete frame for a list row: summary and search text only.
pub fn dissect_frame_summary(frame: &[u8]) -> Option<EthernetDissection> {
    dissect_frame_with(frame, false)
}

fn dissect_frame_with(frame: &[u8], keep_fields: bool) -> Option<EthernetDissection> {
    if frame.len() < 14 {
        return None;
    }
    let mut destination = [0_u8; 6];
    destination.copy_from_slice(&frame[..6]);
    let mut source = [0_u8; 6];
    source.copy_from_slice(&frame[6..12]);
    let ether_type = u16::from_be_bytes([frame[12], frame[13]]);
    Some(dissect_with(
        EthernetHeader {
            destination,
            source,
            ether_type,
            vlan_tci: &[],
            payload: frame.get(14..).unwrap_or_default(),
        },
        keep_fields,
    ))
}

/// EtherTypes that carry another tag instead of an upper-layer payload.
const VLAN_ETHERTYPES: &[u16] = &[0x8100, 0x88A8, 0x9100];
/// IEEE 802.1CB redundancy tag.
const RTAG_ETHERTYPE: u16 = 0xF1C1;
/// Marvell Ethertype DSA tags: the programmable value and the `0x0000` form.
const EDSA_ETHERTYPES: &[u16] = &[0xDADA, 0x0000];

/// Peel the link-layer tag stack (VLAN, Marvell EDSA, 802.1CB R-TAG).
///
/// Returns the upper-layer EtherType and the bytes that follow it.  Every tag
/// is recorded on the dissection, so stacked tags keep their order.
fn peel_tags(
    dissection: &mut EthernetDissection,
    mut ether_type: u16,
    payload: &[u8],
) -> (u16, Vec<u8>) {
    let mut rest = payload.to_vec();
    let mut hops = 0;
    loop {
        hops += 1;
        if hops > 8 {
            break;
        }
        if VLAN_ETHERTYPES.contains(&ether_type) {
            if rest.len() < 4 {
                break;
            }
            let tci = u16::from_be_bytes([rest[0], rest[1]]);
            record_vlan(dissection, tci);
            ether_type = u16::from_be_bytes([rest[2], rest[3]]);
            rest.drain(..4);
        } else if ether_type == RTAG_ETHERTYPE {
            // 802.1CB: EtherType, 16-bit sequence number, next EtherType.
            if rest.len() < 4 {
                break;
            }
            let sequence = u16::from_be_bytes([rest[0], rest[1]]);
            record_rtag(dissection, sequence);
            ether_type = u16::from_be_bytes([rest[2], rest[3]]);
            rest.drain(..4);
        } else if EDSA_ETHERTYPES.contains(&ether_type) {
            // Marvell EDSA: two reserved octets, then a 4-octet DSA tag.
            if rest.len() < 8 || rest[0] != 0 || rest[1] != 0 {
                break;
            }
            let tag = u32::from_be_bytes([rest[2], rest[3], rest[4], rest[5]]);
            record_dsa(dissection, tag);
            ether_type = u16::from_be_bytes([rest[6], rest[7]]);
            rest.drain(..8);
        } else {
            break;
        }
    }
    (ether_type, rest)
}

fn record_vlan(dissection: &mut EthernetDissection, tci: u16) {
    if !dissection.layers.contains(&"vlan") {
        dissection.layers.push("vlan");
    }
    let index = dissection.vlan_ids.len();
    dissection.vlan_ids.push(tci & 0x0FFF);
    let name = if index == 0 {
        "vlanId".to_string()
    } else {
        format!("vlanId{}", index + 1)
    };
    dissection.describe("vlan", name, (tci & 0x0FFF).to_string());
    dissection.describe(
        "vlan",
        if index == 0 {
            "priority"
        } else {
            "priority_inner"
        },
        ((tci >> 13) & 0x7).to_string(),
    );
    if index == 0 {
        dissection.link_text = format!(" VLAN {}", tci & 0x0FFF);
    }
}

/// Record an 802.1CB redundancy tag, keeping its sequence number searchable.
fn record_rtag(dissection: &mut EthernetDissection, sequence: u16) {
    if !dissection.layers.contains(&"rtag") {
        dissection.layers.push("rtag");
    }
    dissection.describe("rtag", "sequenceNumber", sequence.to_string());
    dissection
        .link_text
        .push_str(&format!(" R-TAG seq {sequence}"));
}

/// Record a Marvell EDSA switch tag.
fn record_dsa(dissection: &mut EthernetDissection, tag: u32) {
    if !dissection.layers.contains(&"edsa") {
        dissection.layers.push("edsa");
    }
    let mode = tag >> 30;
    let tagged = (tag >> 29) & 1;
    let device = (tag >> 24) & 0x1F;
    let port = (tag >> 19) & 0x1F;
    let b18 = (tag >> 18) & 1;
    let b17 = (tag >> 17) & 1;
    let b16 = (tag >> 16) & 1;
    let priority = (tag >> 13) & 0x7;
    let b12 = (tag >> 12) & 1;
    let vlan = tag & 0x0FFF;
    let mode_name = match mode {
        0 => "To_CPU",
        1 => "From_CPU",
        2 => "To_Sniffer",
        _ => "Forward",
    };
    dissection.describe("edsa", "mode", mode_name);
    dissection.describe("edsa", "tagged", tagged.to_string());
    match mode {
        0 | 2 => {
            dissection.describe("edsa", "sourceDevice", device.to_string());
            dissection.describe("edsa", "sourcePort", port.to_string());
        }
        1 => {
            dissection.describe("edsa", "destinationDevice", device.to_string());
            dissection.describe("edsa", "destinationPort", port.to_string());
        }
        _ => {
            if b18 == 1 {
                dissection.describe("edsa", "sourceTrunk", port.to_string());
            } else {
                dissection.describe("edsa", "sourcePort", port.to_string());
            }
        }
    }
    if mode == 2 {
        // b18 tells whether the frame left the source port on ingress or egress.
        dissection.describe(
            "edsa",
            "direction",
            if b18 == 1 { "ingress" } else { "egress" },
        );
    }
    if mode == 0 {
        // To_CPU code bits are b18, b17 and b12.
        let code = ((b18 << 2) | (b17 << 1) | b12) as u8;
        dissection.describe("edsa", "code", format!("{code} ({})", edsa_code_name(code)));
    }
    dissection.describe("edsa", "cfi", b16.to_string());
    dissection.describe("edsa", "priority", priority.to_string());
    dissection.describe("edsa", "vlanId", vlan.to_string());
    dissection
        .link_text
        .push_str(&format!(" EDSA {mode_name} dev{device} port{port}"));
}

fn edsa_code_name(code: u8) -> &'static str {
    match code {
        0 => "MGMT_TRAP",
        1 => "FRAME2REG",
        2 => "IGMP_MLD_TRAP",
        3 => "POLICY_TRAP",
        4 => "ARP_MIRROR",
        5 => "POLICY_MIRROR",
        _ => "reserved",
    }
}

/// Dissect the pieces BLF keeps separately from the upper-layer payload.
pub fn dissect(header: EthernetHeader<'_>) -> EthernetDissection {
    dissect_with(header, true)
}

/// Dissect for a list row: no per-field vector, only summary and search text.
///
/// Loading a trace needs a summary for every frame but fields for none, so this
/// keeps a large Ethernet capture cheap to hold.
pub fn dissect_summary(header: EthernetHeader<'_>) -> EthernetDissection {
    dissect_with(header, false)
}

fn dissect_with(header: EthernetHeader<'_>, keep_fields: bool) -> EthernetDissection {
    let destination = format_mac(&header.destination);
    let source = format_mac(&header.source);
    let mut dissection = EthernetDissection {
        layers: vec!["ethernet"],
        summary: String::new(),
        link_text: String::new(),
        search: String::new(),
        search_layers: Vec::new(),
        keep_fields,
        detail: String::new(),
        fields: Vec::new(),
        ether_type: header.ether_type,
        // `record_vlan` fills this while the tag stack is peeled.
        vlan_ids: Vec::new(),
        protocol: ether_type_name(header.ether_type),
        destination: destination.clone(),
        source: source.clone(),
    };
    dissection.describe("ethernet", "destination", destination.clone());
    dissection.describe("ethernet", "source", source.clone());
    dissection.describe(
        "ethernet",
        "etherType",
        format!(
            "0x{:04X} ({})",
            header.ether_type,
            ether_type_name(header.ether_type)
        ),
    );
    // BLF keeps the outermost VLAN tag beside the addresses; the rest of the
    // stack (inner VLANs, EDSA, R-TAG) lives in the payload.
    for tci in header.vlan_tci {
        record_vlan(&mut dissection, *tci);
    }
    let (ether_type, payload) = peel_tags(&mut dissection, header.ether_type, header.payload);
    let payload = payload.as_slice();
    if ether_type != header.ether_type {
        dissection.ether_type = ether_type;
        dissection.describe(
            "ethernet",
            "innerEtherType",
            format!("0x{ether_type:04X} ({})", ether_type_name(ether_type)),
        );
    }
    let upper = match ether_type {
        0x0806 => {
            dissect_arp(&mut dissection, payload);
            "ARP"
        }
        0x0800 => dissect_ipv4(&mut dissection, payload),
        0x86DD => dissect_ipv6(&mut dissection, payload),
        0x88CC => {
            dissect_lldp(&mut dissection, payload);
            "LLDP"
        }
        0x88F7 => {
            dissect_ptp(&mut dissection, payload);
            "PTP"
        }
        _ => ether_type_name(ether_type),
    };
    if dissection.layers.len() == 1 {
        dissection.layers.push("payload");
        dissection.describe("payload", "length", payload.len().to_string());
        dissection.describe("payload", "hex", hex(payload));
    }
    dissection.protocol = upper;
    dissection.summary = format!(
        "{source} > {destination}{} {upper}{}",
        dissection.link_text, dissection.detail
    );
    dissection
}

/// Dissect a structured BLF object when it carries an Ethernet frame.
pub fn dissect_object(object: &Object) -> Option<EthernetDissection> {
    dissect_object_with(object, true)
}

/// Dissect an object for a list row: summary and search text only.
pub fn dissect_object_summary(object: &Object) -> Option<EthernetDissection> {
    dissect_object_with(object, false)
}

fn dissect_object_with(object: &Object, keep_fields: bool) -> Option<EthernetDissection> {
    let Object::Structured(value) = object else {
        return None;
    };
    dissect_structured(value, keep_fields)
}

fn dissect_structured(object: &StructuredObject, keep_fields: bool) -> Option<EthernetDissection> {
    match object.object_type {
        ObjectType::EthernetFrame => {
            let destination = field_bytes(&object.fields, "destinationAddress")?;
            let source = field_bytes(&object.fields, "sourceAddress")?;
            let ether_type = u16::try_from(field_u64(&object.fields, "type")?).ok()?;
            let tpid = field_u64(&object.fields, "tpid").unwrap_or(0);
            let tci = u16::try_from(field_u64(&object.fields, "tci").unwrap_or(0)).unwrap_or(0);
            let mut vlan = Vec::new();
            if matches!(tpid, 0x8100 | 0x88A8 | 0x9100) {
                vlan.push(tci);
            }
            let payload = field_bytes(&object.fields, "payLoad").unwrap_or_default();
            let mut header = [0_u8; 6];
            header.copy_from_slice(&destination[..destination.len().min(6)]);
            let mut source_header = [0_u8; 6];
            source_header.copy_from_slice(&source[..source.len().min(6)]);
            Some(dissect_with(
                EthernetHeader {
                    destination: header,
                    source: source_header,
                    ether_type,
                    vlan_tci: &vlan,
                    payload: &payload,
                },
                keep_fields,
            ))
        }
        ObjectType::EthernetFrameEx
        | ObjectType::EthernetFrameForwarded
        | ObjectType::EthernetErrorEx
        | ObjectType::EthernetErrorForwarded => {
            let data = field_bytes(&object.fields, "frameData")?;
            dissect_frame_with(&data, keep_fields)
        }
        _ => None,
    }
}

fn dissect_arp(dissection: &mut EthernetDissection, payload: &[u8]) {
    dissection.layers.push("arp");
    if payload.len() < 8 {
        dissection.describe("arp", "error", "truncated ARP header");
        return;
    }
    let opcode = u16::from_be_bytes([payload[6], payload[7]]);
    dissection.describe("arp", "opcode", arp_opcode_name(opcode));
    if payload.len() < 28 {
        dissection.describe("arp", "error", "truncated ARP body");
        return;
    }
    let sender_mac = format_mac(&payload[8..14]);
    let sender_ip = format_ipv4(&payload[14..18]);
    let target_mac = format_mac(&payload[18..24]);
    let target_ip = format_ipv4(&payload[24..28]);
    dissection.describe("arp", "senderMac", sender_mac.clone());
    dissection.describe("arp", "senderIp", sender_ip.clone());
    dissection.describe("arp", "targetMac", target_mac);
    dissection.describe("arp", "targetIp", target_ip.clone());
    dissection
        .detail
        .push_str(&format!(" {sender_ip} > {target_ip}"));
}

/// Returns the highest protocol it reached.
fn dissect_ipv4(dissection: &mut EthernetDissection, payload: &[u8]) -> &'static str {
    dissection.layers.push("ipv4");
    if payload.len() < 20 {
        dissection.describe("ipv4", "error", "truncated IPv4 header");
        return "IPv4";
    }
    let version = payload[0] >> 4;
    let header_length = usize::from(payload[0] & 0x0F) * 4;
    let total_length = usize::from(u16::from_be_bytes([payload[2], payload[3]]));
    let identification = u16::from_be_bytes([payload[4], payload[5]]);
    let flags = payload[6] >> 5;
    let fragment_offset = u16::from_be_bytes([payload[6] & 0x1F, payload[7]]);
    let ttl = payload[8];
    let protocol = payload[9];
    let source = format_ipv4(&payload[12..16]);
    let destination = format_ipv4(&payload[16..20]);
    dissection.describe("ipv4", "version", version.to_string());
    dissection.describe("ipv4", "headerLength", header_length.to_string());
    dissection.describe("ipv4", "dscp", (payload[1] >> 2).to_string());
    dissection.describe("ipv4", "ecn", (payload[1] & 0x3).to_string());
    dissection.describe("ipv4", "totalLength", total_length.to_string());
    dissection.describe("ipv4", "identification", format!("0x{identification:04X}"));
    dissection.describe("ipv4", "flags", ipv4_flags(flags));
    dissection.describe("ipv4", "fragmentOffset", fragment_offset.to_string());
    dissection.describe("ipv4", "ttl", ttl.to_string());
    dissection.describe(
        "ipv4",
        "protocol",
        format!("{protocol} ({})", ip_protocol_name(protocol)),
    );
    dissection.describe("ipv4", "checksum", ipv4_checksum_state(payload));
    dissection.describe("ipv4", "source", source.clone());
    dissection.describe("ipv4", "destination", destination.clone());
    dissection
        .detail
        .push_str(&format!(" {source} > {destination}"));

    if header_length < 20 || header_length > payload.len() {
        dissection.describe("ipv4", "error", "invalid IPv4 header length");
        return "IPv4";
    }
    if fragment_offset != 0 {
        // A non-first fragment carries no transport header.
        dissection.describe("ipv4", "note", "fragment, transport header not present");
        return "IPv4";
    }
    let end = total_length.min(payload.len()).max(header_length);
    let body = payload.get(header_length..end).unwrap_or_default();
    match protocol {
        1 => {
            dissect_icmp(dissection, body, "icmp");
            "ICMP"
        }
        6 => dissect_tcp(dissection, body),
        17 => dissect_udp(dissection, body),
        other => {
            dissection.layers.push("payload");
            dissection.describe("payload", "length", body.len().to_string());
            dissection.describe("payload", "hex", hex(body));
            ip_protocol_name(other)
        }
    }
}

fn dissect_ipv6(dissection: &mut EthernetDissection, payload: &[u8]) -> &'static str {
    dissection.layers.push("ipv6");
    if payload.len() < 40 {
        dissection.describe("ipv6", "error", "truncated IPv6 header");
        return "IPv6";
    }
    let traffic_class = ((payload[0] & 0x0F) << 4) | (payload[1] >> 4);
    let flow_label = (u32::from(payload[1] & 0x0F) << 16)
        | u32::from(u16::from_be_bytes([payload[2], payload[3]]));
    let payload_length = usize::from(u16::from_be_bytes([payload[4], payload[5]]));
    let next_header = payload[6];
    let source = format_ipv6(&payload[8..24]);
    let destination = format_ipv6(&payload[24..40]);
    dissection.describe("ipv6", "trafficClass", traffic_class.to_string());
    dissection.describe("ipv6", "flowLabel", format!("0x{flow_label:05X}"));
    dissection.describe("ipv6", "payloadLength", payload_length.to_string());
    dissection.describe("ipv6", "hopLimit", payload[7].to_string());
    dissection.describe(
        "ipv6",
        "nextHeader",
        format!("{next_header} ({})", ip_protocol_name(next_header)),
    );
    dissection.describe("ipv6", "source", source.clone());
    dissection.describe("ipv6", "destination", destination.clone());
    dissection
        .detail
        .push_str(&format!(" {source} > {destination}"));

    let mut offset = 40;
    let mut header = next_header;
    // Walk the common extension headers; a fragment stops the transport layer.
    for _ in 0..4 {
        match header {
            0 | 43 | 60 => {
                if payload.len() < offset + 2 {
                    return "IPv6";
                }
                let length = (usize::from(payload[offset + 1]) + 1) * 8;
                header = payload[offset];
                offset += length;
            }
            44 => {
                dissection.describe("ipv6", "note", "fragment, transport header not present");
                return "IPv6";
            }
            51 => {
                if payload.len() < offset + 2 {
                    return "IPv6";
                }
                let length = (usize::from(payload[offset + 1]) + 2) * 4;
                header = payload[offset];
                offset += length;
            }
            _ => break,
        }
    }
    let body = payload.get(offset..).unwrap_or_default();
    match header {
        58 => {
            dissect_icmp(dissection, body, "icmpv6");
            "ICMPv6"
        }
        6 => dissect_tcp(dissection, body),
        17 => dissect_udp(dissection, body),
        other => {
            dissection.layers.push("payload");
            dissection.describe("payload", "length", body.len().to_string());
            dissection.describe("payload", "hex", hex(body));
            ip_protocol_name(other)
        }
    }
}

fn dissect_icmp(dissection: &mut EthernetDissection, payload: &[u8], layer: &'static str) {
    dissection.layers.push(layer);
    if payload.len() < 4 {
        dissection.describe(layer, "error", "truncated ICMP header");
        return;
    }
    let kind = payload[0];
    let code = payload[1];
    dissection.describe(layer, "type", kind.to_string());
    dissection.describe(layer, "code", code.to_string());
    dissection.describe(layer, "name", icmp_name(layer, kind, code));
    if payload.len() >= 8 {
        dissection.describe(layer, "restOfHeader", hex(&payload[4..8]));
    }
    dissection
        .detail
        .push_str(&format!(" {} {}", icmp_name(layer, kind, code), code));
}

fn dissect_udp(dissection: &mut EthernetDissection, payload: &[u8]) -> &'static str {
    dissection.layers.push("udp");
    if payload.len() < 8 {
        dissection.describe("udp", "error", "truncated UDP header");
        return "UDP";
    }
    let source = u16::from_be_bytes([payload[0], payload[1]]);
    let destination = u16::from_be_bytes([payload[2], payload[3]]);
    let length = usize::from(u16::from_be_bytes([payload[4], payload[5]]));
    let checksum = u16::from_be_bytes([payload[6], payload[7]]);
    dissection.describe("udp", "sourcePort", source.to_string());
    dissection.describe("udp", "destinationPort", destination.to_string());
    dissection.describe("udp", "length", length.to_string());
    dissection.describe(
        "udp",
        "checksum",
        if checksum == 0 { "not set" } else { "present" }.to_string(),
    );
    dissection
        .detail
        .push_str(&format!(" {source} > {destination}"));
    let end = length.min(payload.len()).max(8);
    let body = payload.get(8..end).unwrap_or_default();
    dissect_application(dissection, source, destination, body, "UDP")
}

fn dissect_tcp(dissection: &mut EthernetDissection, payload: &[u8]) -> &'static str {
    dissection.layers.push("tcp");
    if payload.len() < 20 {
        dissection.describe("tcp", "error", "truncated TCP header");
        return "TCP";
    }
    let source = u16::from_be_bytes([payload[0], payload[1]]);
    let destination = u16::from_be_bytes([payload[2], payload[3]]);
    let sequence = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let acknowledgement = u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]);
    let header_length = usize::from(payload[12] >> 4) * 4;
    let flags = payload[13];
    let window = u16::from_be_bytes([payload[14], payload[15]]);
    let checksum = u16::from_be_bytes([payload[16], payload[17]]);
    dissection.describe("tcp", "sourcePort", source.to_string());
    dissection.describe("tcp", "destinationPort", destination.to_string());
    dissection.describe("tcp", "sequence", sequence.to_string());
    dissection.describe("tcp", "acknowledgement", acknowledgement.to_string());
    dissection.describe("tcp", "headerLength", header_length.to_string());
    dissection.describe("tcp", "flags", tcp_flags(flags));
    dissection.describe("tcp", "window", window.to_string());
    dissection.describe(
        "tcp",
        "checksum",
        if checksum == 0 { "not set" } else { "present" }.to_string(),
    );
    dissection
        .detail
        .push_str(&format!(" {source} > {destination}"));
    if header_length < 20 || header_length > payload.len() {
        dissection.describe("tcp", "error", "invalid TCP header length");
        return "TCP";
    }
    let body = payload.get(header_length..).unwrap_or_default();
    dissect_application(dissection, source, destination, body, "TCP")
}

fn dissect_application(
    dissection: &mut EthernetDissection,
    source_port: u16,
    destination_port: u16,
    payload: &[u8],
    fallback: &'static str,
) -> &'static str {
    let port = if matches!(destination_port, 30490) || matches!(source_port, 30490) {
        30490
    } else if matches!(destination_port, 13400) || matches!(source_port, 13400) {
        13400
    } else if matches!(destination_port, 67 | 68) || matches!(source_port, 67 | 68) {
        68
    } else if destination_port == 53 || source_port == 53 {
        53
    } else {
        0
    };
    match port {
        30490 => {
            dissect_someip(dissection, payload);
            "SOME/IP"
        }
        13400 => dissect_doip(dissection, payload),
        68 => {
            dissect_dhcp(dissection, payload);
            "DHCP"
        }
        53 => {
            dissect_dns(dissection, payload);
            "DNS"
        }
        _ => {
            dissection.layers.push("payload");
            dissection.describe("payload", "length", payload.len().to_string());
            if !payload.is_empty() {
                dissection.describe("payload", "hex", hex(payload));
            }
            fallback
        }
    }
}

fn dissect_someip(dissection: &mut EthernetDissection, payload: &[u8]) {
    dissection.layers.push("someip");
    if payload.len() < 16 {
        dissection.describe("someip", "error", "truncated SOME/IP header");
        return;
    }
    let message_id = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let length = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let request_id = u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]);
    let protocol_version = payload[12];
    let interface_version = payload[13];
    let message_type = payload[14];
    let return_code = payload[15];
    dissection.describe("someip", "serviceId", format!("0x{:04X}", message_id >> 16));
    dissection.describe(
        "someip",
        "methodId",
        format!("0x{:04X}", message_id & 0xFFFF),
    );
    dissection.describe("someip", "length", length.to_string());
    dissection.describe("someip", "clientId", format!("0x{:04X}", request_id >> 16));
    dissection.describe(
        "someip",
        "sessionId",
        format!("0x{:04X}", request_id & 0xFFFF),
    );
    dissection.describe("someip", "protocolVersion", protocol_version.to_string());
    dissection.describe("someip", "interfaceVersion", interface_version.to_string());
    dissection.describe("someip", "messageType", someip_message_type(message_type));
    dissection.describe("someip", "returnCode", return_code.to_string());
    let body = payload.get(16..).unwrap_or_default();
    dissection.describe("someip", "payloadLength", body.len().to_string());
    dissection
        .detail
        .push_str(&format!(" SOME/IP 0x{:04X}", message_id >> 16));
}

fn dissect_doip(dissection: &mut EthernetDissection, payload: &[u8]) -> &'static str {
    dissection.layers.push("doip");
    if payload.len() < 8 {
        dissection.describe("doip", "error", "truncated DoIP header");
        return "DoIP";
    }
    let version = payload[0];
    let payload_type = u16::from_be_bytes([payload[2], payload[3]]);
    let length = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    dissection.describe("doip", "protocolVersion", format!("0x{version:02X}"));
    dissection.describe(
        "doip",
        "payloadType",
        format!("0x{payload_type:04X} ({})", doip_payload_name(payload_type)),
    );
    dissection.describe("doip", "payloadLength", length.to_string());
    let end = (8 + length as usize).min(payload.len());
    let body = payload.get(8..end).unwrap_or_default();
    if payload_type == 0x8001 && body.len() >= 5 {
        let source = u16::from_be_bytes([body[0], body[1]]);
        let target = u16::from_be_bytes([body[2], body[3]]);
        dissection.describe("doip", "sourceAddress", format!("0x{source:04X}"));
        dissection.describe("doip", "targetAddress", format!("0x{target:04X}"));
        if let Some(name) = dissect_uds(dissection, &body[4..]) {
            dissection
                .detail
                .push_str(&format!(" DoIP {}", doip_payload_name(payload_type)));
            return name;
        }
    } else if payload_type == 0x8002 && body.len() >= 5 {
        let source = u16::from_be_bytes([body[0], body[1]]);
        let target = u16::from_be_bytes([body[2], body[3]]);
        let acknowledge = body[4];
        dissection.describe("doip", "sourceAddress", format!("0x{source:04X}"));
        dissection.describe("doip", "targetAddress", format!("0x{target:04X}"));
        dissection.describe("doip", "acknowledgeCode", format!("0x{acknowledge:02X}"));
    }
    dissection
        .detail
        .push_str(&format!(" DoIP {}", doip_payload_name(payload_type)));
    "DoIP"
}

fn dissect_dhcp(dissection: &mut EthernetDissection, payload: &[u8]) {
    dissection.layers.push("dhcp");
    if payload.len() < 240 {
        dissection.describe("dhcp", "error", "truncated DHCP message");
        return;
    }
    dissection.describe(
        "dhcp",
        "op",
        if payload[0] == 2 { "reply" } else { "request" },
    );
    dissection.describe(
        "dhcp",
        "transactionId",
        format!(
            "0x{:08X}",
            u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]])
        ),
    );
    dissection.describe("dhcp", "clientMac", format_mac(&payload[28..34]));
    dissection.describe("dhcp", "yourIp", format_ipv4(&payload[16..20]));
    let mut offset = 240;
    let mut message_type = None;
    while offset + 2 <= payload.len() {
        let code = payload[offset];
        if code == 255 {
            break;
        }
        if code == 0 {
            offset += 1;
            continue;
        }
        let length = usize::from(payload[offset + 1]);
        let value = payload
            .get(offset + 2..offset + 2 + length)
            .unwrap_or_default();
        match code {
            53 if !value.is_empty() => {
                message_type = Some(value[0]);
                dissection.describe("dhcp", "messageType", dhcp_message_name(value[0]));
            }
            12 => {
                dissection.describe("dhcp", "hostName", utf8_lossy(value));
            }
            54 if value.len() == 4 => {
                dissection.describe("dhcp", "serverIp", format_ipv4(value));
            }
            _ => {}
        }
        offset += 2 + length;
    }
    if let Some(kind) = message_type {
        dissection
            .detail
            .push_str(&format!(" {}", dhcp_message_name(kind)));
    }
}

fn dissect_dns(dissection: &mut EthernetDissection, payload: &[u8]) {
    dissection.layers.push("dns");
    if payload.len() < 12 {
        dissection.describe("dns", "error", "truncated DNS header");
        return;
    }
    let identifier = u16::from_be_bytes([payload[0], payload[1]]);
    let flags = u16::from_be_bytes([payload[2], payload[3]]);
    let questions = u16::from_be_bytes([payload[4], payload[5]]);
    dissection.describe("dns", "transactionId", format!("0x{identifier:04X}"));
    dissection.describe("dns", "response", u8::from(flags & 0x8000 != 0).to_string());
    dissection.describe("dns", "questions", questions.to_string());
    if let Some(name) = dns_name(&payload[12..]) {
        dissection.describe("dns", "question", name.clone());
        dissection.detail.push_str(&format!(" {name}"));
    }
}

fn dissect_lldp(dissection: &mut EthernetDissection, payload: &[u8]) {
    dissection.layers.push("lldp");
    let mut offset = 0;
    while offset + 2 <= payload.len() {
        let header = u16::from_be_bytes([payload[offset], payload[offset + 1]]);
        let kind = header >> 9;
        let length = usize::from(header & 0x1FF);
        let value = payload
            .get(offset + 2..offset + 2 + length)
            .unwrap_or_default();
        match kind {
            0 => break,
            1 => {
                if value.len() > 1 {
                    dissection.describe("lldp", "chassisId", hex(&value[1..]));
                }
            }
            2 => {
                if value.len() > 1 {
                    dissection.describe("lldp", "portId", hex(&value[1..]));
                }
            }
            3 => {
                if value.len() == 2 {
                    dissection.describe(
                        "lldp",
                        "ttl",
                        u16::from_be_bytes([value[0], value[1]]).to_string(),
                    );
                }
            }
            5 => {
                dissection.describe("lldp", "systemName", utf8_lossy(value));
            }
            8 => {
                dissection.describe("lldp", "managementAddress", hex(value));
            }
            _ => {}
        }
        offset += 2 + length;
    }
    dissection.detail.push_str(" LLDP");
}

fn dissect_ptp(dissection: &mut EthernetDissection, payload: &[u8]) {
    dissection.layers.push("ptp");
    if payload.len() < 34 {
        dissection.describe("ptp", "error", "truncated PTP header");
        return;
    }
    let message_type = payload[0] & 0x0F;
    let version = payload[1] & 0x0F;
    let length = u16::from_be_bytes([payload[2], payload[3]]);
    let domain = payload[4];
    let flags = u16::from_be_bytes([payload[6], payload[7]]);
    // The correction field is a 48-bit nanoseconds value in units of 2^-16 ns.
    let correction = i64::from_be_bytes([
        0,
        0,
        payload[8],
        payload[9],
        payload[10],
        payload[11],
        payload[12],
        payload[13],
    ]);
    let clock_identity = payload[20..28]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    let port_number = u16::from_be_bytes([payload[28], payload[29]]);
    let sequence = u16::from_be_bytes([payload[30], payload[31]]);
    let control = payload[32];
    let log_interval = payload[33] as i8;
    let name = ptp_message_name(message_type);
    dissection.describe("ptp", "messageType", format!("{name} ({message_type})"));
    dissection.describe("ptp", "version", version.to_string());
    dissection.describe("ptp", "messageLength", length.to_string());
    dissection.describe("ptp", "domain", domain.to_string());
    dissection.describe("ptp", "flags", ptp_flags(flags));
    dissection.describe(
        "ptp",
        "correctionField",
        format!("{:.3} ns", correction as f64 / 65_536.0),
    );
    dissection.describe(
        "ptp",
        "sourcePortIdentity",
        format!("{clock_identity}/{port_number}"),
    );
    dissection.describe("ptp", "sequenceId", sequence.to_string());
    dissection.describe("ptp", "control", control.to_string());
    dissection.describe("ptp", "logMessageInterval", log_interval.to_string());

    let body_end = usize::from(length).min(payload.len()).max(34);
    let body = payload.get(34..body_end).unwrap_or_default();
    match message_type {
        0x0 | 0x8 => {
            // Sync and Follow_Up carry a 10-octet timestamp.
            dissect_ptp_timestamp(dissection, body, "originTimestamp");
        }
        0x1 | 0x2 => {
            dissect_ptp_timestamp(dissection, body, "originTimestamp");
        }
        0x3 | 0x9 | 0xA => {
            dissect_ptp_timestamp(dissection, body, "requestReceiptTimestamp");
            if body.len() >= 20 {
                let turn = u16::from_be_bytes([body[10], body[11]]);
                dissection.describe("ptp", "requestingPortNumber", turn.to_string());
                dissect_ptp_timestamp(dissection, &body[10..], "responseOriginTimestamp");
            }
        }
        0xB => dissect_ptp_announce(dissection, body),
        _ => {}
    }
    let body_length = match message_type {
        0x0 | 0x1 | 0x2 | 0x8 | 0x9 | 0xC | 0xD => 10,
        0x3 | 0xA => 20,
        0xB => 30,
        _ => 0,
    };
    dissect_ptp_tlvs(dissection, payload, length, 34 + body_length);
    dissection.detail.push_str(&format!(" PTP {name}"));
}

/// gPTP timestamps are 48-bit seconds plus a 32-bit nanoseconds fraction.
fn dissect_ptp_timestamp(dissection: &mut EthernetDissection, body: &[u8], name: &str) {
    if body.len() < 10 {
        dissection.describe("ptp", "error", format!("truncated {name}"));
        return;
    }
    let seconds = u64::from_be_bytes([0, 0, body[0], body[1], body[2], body[3], body[4], body[5]]);
    let nanoseconds = u32::from_be_bytes([body[6], body[7], body[8], body[9]]);
    dissection.describe("ptp", name, format!("{seconds}.{nanoseconds:09} s"));
}

fn dissect_ptp_announce(dissection: &mut EthernetDissection, body: &[u8]) {
    if body.len() < 30 {
        dissection.describe("ptp", "error", "truncated announce body");
        return;
    }
    dissect_ptp_timestamp(dissection, body, "originTimestamp");
    let current_utc_offset = i16::from_be_bytes([body[10], body[11]]);
    let priority1 = body[13];
    let clock_class = body[14];
    let clock_accuracy = body[15];
    let clock_variance = u16::from_be_bytes([body[16], body[17]]);
    let priority2 = body[18];
    let steps_removed = u16::from_be_bytes([body[20], body[21]]);
    let time_source = body[22];
    dissection.describe("ptp", "currentUtcOffset", current_utc_offset.to_string());
    dissection.describe("ptp", "priority1", priority1.to_string());
    dissection.describe("ptp", "clockClass", clock_class.to_string());
    dissection.describe("ptp", "clockAccuracy", format!("0x{clock_accuracy:02X}"));
    dissection.describe("ptp", "offsetScaledLogVariance", clock_variance.to_string());
    dissection.describe("ptp", "priority2", priority2.to_string());
    dissection.describe("ptp", "stepsRemoved", steps_removed.to_string());
    dissection.describe("ptp", "timeSource", time_source.to_string());
}

/// Walk the TLV suffix; 802.1AS uses organizationally-specific TLVs (OUI 00-80-C2).
fn dissect_ptp_tlvs(
    dissection: &mut EthernetDissection,
    payload: &[u8],
    length: u16,
    start: usize,
) {
    let end = usize::from(length).min(payload.len()).max(start);
    let mut offset = start;
    let mut index = 0;
    while offset + 4 <= end {
        // The body length of a PTP message decides where the TLVs start, but a
        // BLF frame rarely tells us that, so only structs we can read are used.
        let tlv_type = u16::from_be_bytes([payload[offset], payload[offset + 1]]);
        let tlv_length = usize::from(u16::from_be_bytes([
            payload[offset + 2],
            payload[offset + 3],
        ]));
        if offset + 4 + tlv_length > end {
            break;
        }
        let value = &payload[offset + 4..offset + 4 + tlv_length];
        index += 1;
        if tlv_type == 0x0003 && value.len() >= 3 {
            let oui = format!("{:02X}-{:02X}-{:02X}", value[0], value[1], value[2]);
            let subtype = if value.len() >= 5 {
                u16::from_be_bytes([value[3], value[4]])
            } else {
                0
            };
            dissection.describe(
                "ptp",
                format!("tlv{index}"),
                format!(
                    "organizationally specific oui {oui} subtype {subtype} ({} octets)",
                    tlv_length
                ),
            );
        } else {
            dissection.describe(
                "ptp",
                format!("tlv{index}"),
                format!("type 0x{tlv_type:04X} ({tlv_length} octets)"),
            );
        }
        offset += 4 + tlv_length;
        if tlv_length == 0 && index > 8 {
            break;
        }
    }
}

fn ptp_flags(flags: u16) -> String {
    let mut names = Vec::new();
    if flags & 0x0200 != 0 {
        names.push("twoStep");
    }
    if flags & 0x0100 != 0 {
        names.push("unicast");
    }
    if flags & 0x0080 != 0 {
        names.push("alternateMaster");
    }
    if flags & 0x0400 != 0 {
        names.push("leap61");
    }
    if flags & 0x0800 != 0 {
        names.push("leap59");
    }
    if flags & 0x1000 != 0 {
        names.push("currentUtcOffsetValid");
    }
    if flags & 0x2000 != 0 {
        names.push("ptpTimescale");
    }
    if flags & 0x4000 != 0 {
        names.push("timeTraceable");
    }
    if flags & 0x8000 != 0 {
        names.push("frequencyTraceable");
    }
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join("|")
    }
}

/// Append a UDS diagnostic message to the dissection.
fn dissect_uds(dissection: &mut EthernetDissection, payload: &[u8]) -> Option<&'static str> {
    let message = crate::uds::decode_uds(payload)?;
    if !dissection.layers.contains(&"uds") {
        dissection.layers.push("uds");
    }
    let summary = message.summary();
    let negative = message.negative.is_some();
    let service_name = message.service_name;
    for (name, value) in message.fields() {
        dissection.describe("uds", name, value);
    }
    dissection.detail.push_str(&format!(" {summary}"));
    if negative {
        Some("UDS/NRC")
    } else {
        Some(uds_protocol_name(service_name))
    }
}

fn uds_protocol_name(service: &'static str) -> &'static str {
    match service {
        "ReadDataByIdentifier" => "UDS/RDBI",
        "WriteDataByIdentifier" => "UDS/WDBI",
        "DiagnosticSessionControl" => "UDS/DSC",
        "ECUReset" => "UDS/ER",
        "SecurityAccess" => "UDS/SA",
        "RoutineControl" => "UDS/RC",
        "TesterPresent" => "UDS/TP",
        "ReadDTCInformation" => "UDS/RDTC",
        "ClearDiagnosticInformation" => "UDS/CDTC",
        "NegativeResponse" => "UDS/NRC",
        _ => "UDS",
    }
}

fn field_value<'a>(fields: &'a [(String, FieldValue)], name: &str) -> Option<&'a FieldValue> {
    fields
        .iter()
        .find(|(field, _)| field == name)
        .map(|(_, value)| value)
}

fn field_bytes(fields: &[(String, FieldValue)], name: &str) -> Option<Vec<u8>> {
    match field_value(fields, name)? {
        FieldValue::Bytes(value) => Some(value.clone()),
        _ => None,
    }
}

fn field_u64(fields: &[(String, FieldValue)], name: &str) -> Option<u64> {
    match field_value(fields, name)? {
        FieldValue::U8(value) => Some(u64::from(*value)),
        FieldValue::U16(value) => Some(u64::from(*value)),
        FieldValue::U32(value) => Some(u64::from(*value)),
        FieldValue::U64(value) => Some(*value),
        FieldValue::I16(value) => u64::try_from(*value).ok(),
        FieldValue::I32(value) => u64::try_from(*value).ok(),
        FieldValue::I64(value) => u64::try_from(*value).ok(),
        _ => None,
    }
}

pub fn ether_type_name(ether_type: u16) -> &'static str {
    match ether_type {
        0x0800 => "IPv4",
        0x0806 => "ARP",
        0x8100 => "VLAN",
        0x86DD => "IPv6",
        0x88A8 => "QinQ",
        0x88CC => "LLDP",
        0x88F7 => "PTP",
        0x9100 => "QinQ",
        0x8847 => "MPLS",
        0x88F8 => "NSH",
        0x0801 => "X.75",
        _ => "Ethernet",
    }
}

pub fn ip_protocol_name(protocol: u8) -> &'static str {
    match protocol {
        1 => "ICMP",
        2 => "IGMP",
        6 => "TCP",
        17 => "UDP",
        41 => "IPv6",
        47 => "GRE",
        50 => "ESP",
        51 => "AH",
        58 => "ICMPv6",
        89 => "OSPF",
        103 => "PIM",
        112 => "VRRP",
        132 => "SCTP",
        136 => "UDPLite",
        _ => "IP payload",
    }
}

fn arp_opcode_name(opcode: u16) -> &'static str {
    match opcode {
        1 => "request",
        2 => "reply",
        3 => "RARP request",
        4 => "RARP reply",
        8 => "InARP request",
        9 => "InARP reply",
        _ => "unknown",
    }
}

fn ipv4_flags(flags: u8) -> String {
    let mut names = Vec::new();
    if flags & 0x1 != 0 {
        names.push("MF");
    }
    if flags & 0x2 != 0 {
        names.push("DF");
    }
    if flags & 0x4 != 0 {
        names.push("reserved");
    }
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join("|")
    }
}

/// Validate the IPv4 header checksum; `-` when the header is too short.
fn ipv4_checksum_state(header: &[u8]) -> &'static str {
    let length = usize::from(header[0] & 0x0F) * 4;
    if length < 20 || header.len() < length {
        return "-";
    }
    let mut sum = 0_u32;
    for chunk in header[..length].chunks(2) {
        if chunk.len() == 2 {
            sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
        }
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    if sum == 0xFFFF {
        "valid"
    } else {
        "invalid"
    }
}

fn icmp_name(layer: &str, kind: u8, code: u8) -> String {
    if layer == "icmpv6" {
        return match (kind, code) {
            (1, _) => "destination unreachable",
            (2, _) => "packet too big",
            (3, _) => "time exceeded",
            (128, _) => "echo request",
            (129, _) => "echo reply",
            (133, _) => "router solicitation",
            (134, _) => "router advertisement",
            (135, _) => "neighbor solicitation",
            (136, _) => "neighbor advertisement",
            _ => "other",
        }
        .to_string();
    }
    match (kind, code) {
        (0, _) => "echo reply",
        (3, 0) => "network unreachable",
        (3, 1) => "host unreachable",
        (3, 3) => "port unreachable",
        (8, _) => "echo request",
        (11, _) => "time exceeded",
        _ => "other",
    }
    .to_string()
}

fn tcp_flags(flags: u8) -> String {
    let mut names = Vec::new();
    for (bit, name) in [
        (0x01, "FIN"),
        (0x02, "SYN"),
        (0x04, "RST"),
        (0x08, "PSH"),
        (0x10, "ACK"),
        (0x20, "URG"),
        (0x40, "ECE"),
        (0x80, "CWR"),
    ] {
        if flags & bit != 0 {
            names.push(name);
        }
    }
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join("|")
    }
}

fn someip_message_type(kind: u8) -> String {
    let name = match kind {
        0x00 => "request",
        0x01 => "request no return",
        0x02 => "notification",
        0x80 => "response",
        0x81 => "error",
        _ => "unknown",
    };
    format!("0x{kind:02X} ({name})")
}

fn doip_payload_name(kind: u16) -> &'static str {
    match kind {
        0x0000 => "generic negative ack",
        0x0001 => "vehicle identification request",
        0x0003 => "vehicle announcement",
        0x0004 => "routing activation request",
        0x0005 => "routing activation response",
        0x0006 => "alive check request",
        0x0007 => "alive check response",
        0x8001 => "diagnostic message",
        0x8002 => "diagnostic ack",
        0x8003 => "diagnostic negative ack",
        _ => "other",
    }
}

fn dhcp_message_name(kind: u8) -> &'static str {
    match kind {
        1 => "discover",
        2 => "offer",
        3 => "request",
        4 => "decline",
        5 => "ack",
        6 => "nak",
        7 => "release",
        8 => "inform",
        _ => "other",
    }
}

fn ptp_message_name(kind: u8) -> &'static str {
    match kind {
        0 => "sync",
        1 => "delay request",
        2 => "peer delay request",
        3 => "peer delay response",
        8 => "follow up",
        9 => "delay response",
        10 => "peer delay response follow up",
        11 => "announce",
        12 => "signaling",
        13 => "management",
        _ => "other",
    }
}

fn dns_name(payload: &[u8]) -> Option<String> {
    let mut labels = Vec::new();
    let mut offset = 0;
    while offset < payload.len() {
        let length = usize::from(payload[offset]);
        if length == 0 {
            break;
        }
        if length & 0xC0 != 0 {
            break;
        }
        offset += 1;
        let label = payload.get(offset..offset + length)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        offset += length;
    }
    (!labels.is_empty()).then(|| labels.join("."))
}

fn format_mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn format_ipv4(bytes: &[u8]) -> String {
    if bytes.len() < 4 {
        return "-".to_string();
    }
    format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3])
}

fn format_ipv6(bytes: &[u8]) -> String {
    if bytes.len() < 16 {
        return "-".to_string();
    }
    let groups = (0..8)
        .map(|index| u16::from_be_bytes([bytes[index * 2], bytes[index * 2 + 1]]))
        .collect::<Vec<_>>();
    // Compress the longest run of zero groups, as RFC 5952 asks.
    let mut best_start = None;
    let mut best_length = 0;
    let mut index = 0;
    while index < groups.len() {
        if groups[index] == 0 {
            let start = index;
            while index < groups.len() && groups[index] == 0 {
                index += 1;
            }
            let length = index - start;
            if length > best_length && length > 1 {
                best_start = Some(start);
                best_length = length;
            }
        } else {
            index += 1;
        }
    }
    match best_start {
        None => groups
            .iter()
            .map(|group| format!("{group:x}"))
            .collect::<Vec<_>>()
            .join(":"),
        Some(start) => {
            let head = groups[..start]
                .iter()
                .map(|group| format!("{group:x}"))
                .collect::<Vec<_>>()
                .join(":");
            let tail = groups[start + best_length..]
                .iter()
                .map(|group| format!("{group:x}"))
                .collect::<Vec<_>>()
                .join(":");
            format!("{head}::{tail}")
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn utf8_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches('\0')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ethernet + IPv4 around an arbitrary transport payload.
    fn ipv4_frame(protocol: u8, body: &[u8]) -> Vec<u8> {
        let mut ip = vec![
            0x45, 0x00, 0x00, 0x00, 0x00, 0x2A, 0x40, 0x00, 64, protocol, 0x00, 0x00, 192, 0, 2, 1,
            192, 0, 2, 2,
        ];
        let total = (ip.len() + body.len()) as u16;
        ip[2..4].copy_from_slice(&total.to_be_bytes());
        let mut sum = 0_u32;
        for chunk in ip.chunks(2) {
            sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        let checksum = !(sum as u16);
        ip[10..12].copy_from_slice(&checksum.to_be_bytes());
        let mut frame = vec![
            0x02, 0x00, 0x00, 0x00, 0x00, 0x02, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00,
        ];
        frame.extend_from_slice(&ip);
        frame.extend_from_slice(body);
        frame
    }

    /// Ethernet + IPv4 + UDP around an application payload.
    fn ipv4_udp_frame(message: &[u8]) -> Vec<u8> {
        let mut udp = vec![0x12, 0x34, 0x77, 0x1A, 0x00, 0x00, 0x00, 0x00];
        udp.extend_from_slice(message);
        let length = (udp.len() as u16).to_be_bytes();
        udp[4..6].copy_from_slice(&length);
        ipv4_frame(17, &udp)
    }

    #[test]
    fn dissects_arp_inside_a_vlan_tag() {
        let mut frame = vec![
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01, 0x81, 0x00,
            0x20, 0x0A, 0x08, 0x06,
        ];
        frame.extend_from_slice(&[
            0x00, 0x01, 0x08, 0x00, 0x06, 0x04, 0x00, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01,
            0x0A, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0x00, 0x00, 0x02,
        ]);
        let dissection = dissect_frame(&frame).expect("dissection");
        assert_eq!(dissection.protocol, "ARP");
        assert_eq!(dissection.vlan_ids, vec![10]);
        assert_eq!(dissection.layers, vec!["ethernet", "vlan", "arp"]);
        let text = dissection.field_text();
        assert!(text.contains("arp.senderIp=10.0.0.1"), "{text}");
        assert!(text.contains("vlan.priority=1"), "{text}");
        assert_eq!(
            dissection.summary,
            "02:00:00:00:00:01 > FF:FF:FF:FF:FF:FF VLAN 10 ARP 10.0.0.1 > 10.0.0.2"
        );
    }

    #[test]
    fn dissects_ipv4_udp_someip() {
        let mut someip = vec![
            0x12, 0x34, 0x56, 0x78, 0x00, 0x00, 0x00, 0x10, 0xAB, 0xCD, 0x00, 0x01, 0x01, 0x01,
            0x00, 0x00,
        ];
        someip.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let frame = ipv4_udp_frame(&someip);
        let dissection = dissect_frame(&frame).expect("dissection");
        assert_eq!(dissection.protocol, "SOME/IP");
        assert_eq!(dissection.layers, vec!["ethernet", "ipv4", "udp", "someip"]);
        let text = dissection.field_text();
        assert!(text.contains("ipv4.source=192.0.2.1"), "{text}");
        assert!(text.contains("ipv4.checksum=valid"), "{text}");
        assert!(text.contains("someip.serviceId=0x1234"), "{text}");
        assert!(text.contains("someip.methodId=0x5678"), "{text}");
        assert!(text.contains("udp.destinationPort=30490"), "{text}");
        assert_eq!(dissection.vlan_ids.len(), 0);
    }

    #[test]
    fn dissects_ipv4_tcp_and_stops_on_a_later_fragment() {
        let mut tcp = vec![
            0x30, 0x39, // source port 12345
            0xD4, 0x30, // destination port 54320
            0x00, 0x00, 0x00, 0x01, // sequence
            0x00, 0x00, 0x00, 0x00, // acknowledgement
            0x50, 0x02, // header length 20, SYN
            0x20, 0x00, // window
            0x00, 0x00, // checksum
            0x00, 0x00, // urgent pointer
        ];
        tcp.extend_from_slice(&[1, 2, 3]);
        let dissection = dissect_frame(&ipv4_frame(6, &tcp)).expect("dissection");
        assert!(
            dissection.layers.contains(&"tcp"),
            "{:?}",
            dissection.layers
        );
        let text = dissection.field_text();
        assert!(text.contains("tcp.destinationPort=54320"), "{text}");
        assert!(text.contains("tcp.flags=SYN"), "{text}");

        // A non-first fragment must stop at IPv4: no transport header is there.
        let mut fragment = ipv4_frame(6, &tcp);
        fragment[14 + 6] = 0x20;
        fragment[14 + 7] = 0x10;
        let dissection = dissect_frame(&fragment).expect("dissection");
        assert_eq!(dissection.protocol, "IPv4");
        assert!(!dissection.layers.contains(&"tcp"));
        assert!(dissection
            .field_text()
            .contains("ipv4.note=fragment, transport header not present"));
    }

    #[test]
    fn dissects_ipv6_udp_dns() {
        let mut udp = vec![0xD4, 0x31, 0x00, 0x35, 0x00, 0x00, 0x00, 0x00];
        udp.extend_from_slice(&[
            0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, b'a',
            b'p', b'i', 0x07, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm',
            0x00, 0x00, 0x01, 0x00, 0x01,
        ]);
        let mut ip = vec![0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 58_u8, 64];
        ip.extend_from_slice(&[0x20, 0x01, 0x0D, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        ip.extend_from_slice(&[0x20, 0x01, 0x0D, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        // IPv6 next header is UDP (17); send ICMPv6 above so the chain still works.
        ip[6] = 17;
        let length = (udp.len() as u16).to_be_bytes();
        udp[4..6].copy_from_slice(&length);
        let mut frame = vec![
            0x02, 0x00, 0x00, 0x00, 0x00, 0x02, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01, 0x86, 0xDD,
        ];
        frame.extend_from_slice(&ip);
        frame.extend_from_slice(&udp);
        let dissection = dissect_frame(&frame).expect("dissection");
        assert_eq!(dissection.protocol, "DNS");
        let text = dissection.field_text();
        assert!(text.contains("ipv6.source=2001:db8::1"), "{text}");
        assert!(text.contains("dns.question=api.example.com"), "{text}");
    }

    #[test]
    fn dissects_ptp_and_lldp_ether_types() {
        let mut ptp = vec![0x00, 0x02, 0x00, 0x2C, 0x00];
        ptp.resize(34, 0);
        ptp[20..28].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // clock identity
        ptp[30..32].copy_from_slice(&[0x12, 0x34]); // sequence id
        let mut frame = vec![
            0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01, 0x88, 0xF7,
        ];
        frame.extend_from_slice(&ptp);
        let dissection = dissect_frame(&frame).expect("dissection");
        assert_eq!(dissection.protocol, "PTP");
        let text = dissection.field_text();
        assert!(text.contains("ptp.messageType=sync"), "{text}");
        assert!(text.contains("ptp.sequenceId=4660"), "{text}");
    }

    #[test]
    fn short_frames_stop_instead_of_reading_past_the_end() {
        assert!(dissect_frame(&[]).is_none());
        assert!(dissect_frame(&[0; 13]).is_none());
        let short_ipv4 = [
            0x02, 0x00, 0x00, 0x00, 0x00, 0x02, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00,
            0x45,
        ];
        let dissection = dissect_frame(&short_ipv4).expect("link layer still readable");
        assert_eq!(dissection.protocol, "IPv4");
        assert!(dissection
            .field_text()
            .contains("ipv4.error=truncated IPv4 header"));
    }

    #[test]
    fn dissects_a_blf_ethernet_frame_object() {
        use crate::types::{
            ObjectHeader, ObjectHeaderBase, ObjectType, RawObjectHeader, StructuredObject,
        };

        let mut payload = vec![0x00, 0x01, 0x08, 0x00, 0x06, 0x04, 0x00, 0x01];
        payload.extend_from_slice(&[0x02, 0, 0, 0, 0, 1, 10, 0, 0, 1]);
        payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 10, 0, 0, 2]);
        let object = Object::Structured(StructuredObject {
            object_type: ObjectType::EthernetFrame,
            header: RawObjectHeader::Object(ObjectHeader {
                base: ObjectHeaderBase {
                    signature: crate::types::OBJECT_SIGNATURE,
                    header_size: 32,
                    header_version: 1,
                    object_size: 0,
                    object_type: ObjectType::EthernetFrame,
                    raw_object_type: ObjectType::EthernetFrame as u32,
                },
                object_flags: 2,
                client_index: 0,
                object_version: 0,
                object_time_stamp: 1_000,
            }),
            class_name: "EthernetFrame".to_string(),
            fields: vec![
                (
                    "sourceAddress".to_string(),
                    FieldValue::Bytes(vec![2, 0, 0, 0, 0, 1]),
                ),
                (
                    "destinationAddress".to_string(),
                    FieldValue::Bytes(vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
                ),
                ("channel".to_string(), FieldValue::U16(2)),
                ("dir".to_string(), FieldValue::U16(0)),
                ("type".to_string(), FieldValue::U16(0x0806)),
                ("tpid".to_string(), FieldValue::U16(0x8100)),
                ("tci".to_string(), FieldValue::U16(0x200A)),
                ("payLoadLength".to_string(), FieldValue::U16(28)),
                ("payLoad".to_string(), FieldValue::Bytes(payload)),
            ],
            extensions: Vec::new(),
            data: Vec::new(),
        });

        let dissection = dissect_object(&object).expect("ethernet object");
        assert_eq!(dissection.protocol, "ARP");
        assert_eq!(dissection.ether_type, 0x0806);
        assert_eq!(dissection.vlan_ids, vec![10]);
        assert_eq!(
            dissection.summary,
            "02:00:00:00:00:01 > FF:FF:FF:FF:FF:FF VLAN 10 ARP 10.0.0.1 > 10.0.0.2"
        );
        // Objects without a frame are not dissected.
        assert!(dissect_object(&Object::Structured(StructuredObject {
            object_type: ObjectType::SysVariable,
            header: RawObjectHeader::Object(ObjectHeader {
                base: ObjectHeaderBase {
                    signature: crate::types::OBJECT_SIGNATURE,
                    header_size: 32,
                    header_version: 1,
                    object_size: 0,
                    object_type: ObjectType::SysVariable,
                    raw_object_type: ObjectType::SysVariable as u32,
                },
                object_flags: 2,
                client_index: 0,
                object_version: 0,
                object_time_stamp: 0,
            }),
            class_name: "SysVariable".to_string(),
            fields: Vec::new(),
            extensions: Vec::new(),
            data: Vec::new(),
        }))
        .is_none());
    }

    fn l2(ether_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![
            0x02, 0x00, 0x00, 0x00, 0x00, 0x02, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01,
        ];
        frame.extend_from_slice(&ether_type.to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn keeps_every_vlan_tag_of_a_stacked_frame() {
        // QinQ: 0x88A8 outer (VID 100), 0x8100 inner (VID 200), then IPv4.
        let ipv4 = ipv4_udp_frame(&[]);
        let mut frame = l2(0x88A8, &[]);
        frame.extend_from_slice(&(0x2000_u16 | 100).to_be_bytes());
        frame.extend_from_slice(&0x8100_u16.to_be_bytes());
        frame.extend_from_slice(&(0x2000_u16 | 200).to_be_bytes());
        frame.extend_from_slice(&ipv4[12..]);
        let dissection = dissect_frame(&frame).expect("dissection");
        assert_eq!(dissection.vlan_ids, vec![100, 200]);
        assert_eq!(dissection.protocol, "SOME/IP");
        let text = dissection.field_text();
        assert!(text.contains("vlan.vlanId=100"), "{text}");
        assert!(text.contains("vlan.vlanId2=200"), "{text}");
    }

    #[test]
    fn dissects_a_marvell_edsa_mirror_frame() {
        // dst, src, EDSA EtherType 0xDADA, two reserved octets, DSA tag, IPv4.
        // Tag: To_Sniffer, device 3, port 7, ingress direction, VID 20.
        let tag: u32 = (0x2 << 30) | (3 << 24) | (7 << 19) | (1 << 18) | 20;
        let mut payload = vec![0x00, 0x00];
        payload.extend_from_slice(&tag.to_be_bytes());
        let ipv4 = ipv4_udp_frame(&[]);
        payload.extend_from_slice(&ipv4[12..]);
        let frame = l2(0xDADA, &payload);
        let dissection = dissect_frame(&frame).expect("dissection");
        assert_eq!(dissection.protocol, "SOME/IP");
        assert_eq!(dissection.ether_type, 0x0800);
        assert!(
            dissection.layers.contains(&"edsa"),
            "{:?}",
            dissection.layers
        );
        let text = dissection.field_text();
        assert!(text.contains("edsa.mode=To_Sniffer"), "{text}");
        assert!(text.contains("edsa.sourceDevice=3"), "{text}");
        assert!(text.contains("edsa.sourcePort=7"), "{text}");
        assert!(text.contains("edsa.direction=ingress"), "{text}");
        assert!(text.contains("edsa.vlanId=20"), "{text}");
        assert!(
            dissection.summary.contains("EDSA To_Sniffer dev3 port7"),
            "{}",
            dissection.summary
        );
    }

    #[test]
    fn dissects_an_802_1cb_redundancy_tag() {
        // Ethernet, R-TAG (seq 4660), then IPv4/UDP.
        let mut payload = Vec::new();
        payload.extend_from_slice(&0x1234_u16.to_be_bytes());
        let ipv4 = ipv4_udp_frame(&[]);
        payload.extend_from_slice(&ipv4[12..]);
        let frame = l2(0xF1C1, &payload);
        let dissection = dissect_frame(&frame).expect("dissection");
        assert_eq!(dissection.protocol, "SOME/IP");
        assert!(
            dissection.layers.contains(&"rtag"),
            "{:?}",
            dissection.layers
        );
        let text = dissection.field_text();
        assert!(text.contains("rtag.sequenceNumber=4660"), "{text}");
        assert!(
            dissection.summary.contains("R-TAG seq 4660"),
            "{}",
            dissection.summary
        );
    }

    #[test]
    fn dissects_a_gptp_follow_up_with_a_timestamp() {
        // 802.1AS Follow_Up: type 0x8, version 2, 44-octet message, body holds
        // the precise origin timestamp and an organizationally specific TLV.
        let mut payload = vec![0x08, 0x02, 0x00, 0x36, 0x00, 0x00, 0x02, 0x08];
        payload.extend_from_slice(&[0; 8]); // correction field
        payload.extend_from_slice(&[0; 4]); // message type specific
        payload.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // clock identity
        payload.extend_from_slice(&[0x00, 0x01]); // port number
        payload.extend_from_slice(&[0x00, 0x2A]); // sequence id 42
        payload.extend_from_slice(&[0x05, 0x7F]); // control, log interval
                                                  // preciseOriginTimestamp: 1728 s + 500000000 ns
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x06, 0xC0]);
        payload.extend_from_slice(&[0x1D, 0xCD, 0x65, 0x00]);
        // TLV: organizationally specific, OUI 00-80-C2, subtype 1.
        payload.extend_from_slice(&[0x00, 0x03, 0x00, 0x06]);
        payload.extend_from_slice(&[0x00, 0x80, 0xC2, 0x00, 0x01, 0x00]);
        let frame = l2(0x88F7, &payload);
        let dissection = dissect_frame(&frame).expect("dissection");
        assert_eq!(dissection.protocol, "PTP");
        let text = dissection.field_text();
        assert!(text.contains("ptp.messageType=follow up"), "{text}");
        assert!(text.contains("ptp.sequenceId=42"), "{text}");
        assert!(
            text.contains("ptp.sourcePortIdentity=01:02:03:04:05:06:07:08/1"),
            "{text}"
        );
        assert!(
            text.contains("ptp.originTimestamp=1728.500000000 s"),
            "{text}"
        );
        assert!(text.contains("oui 00-80-C2 subtype 1"), "{text}");
    }

    #[test]
    fn dissects_uds_inside_a_doip_diagnostic_message() {
        // DoIP diagnostic message (0x8001): SA 0x0E00, TA 0x1001, UDS 0x22 0xF1 0x90.
        let mut payload = vec![0x03, 0xFC, 0x80, 0x01, 0x00, 0x00, 0x00, 0x07];
        payload.extend_from_slice(&[0x0E, 0x00, 0x10, 0x01]);
        payload.extend_from_slice(&[0x22, 0xF1, 0x90]);
        let udp = {
            let mut udp = vec![0x10, 0x00, 0x34, 0x94, 0x00, 0x00, 0x00, 0x00];
            udp.extend_from_slice(&payload);
            let length = (udp.len() as u16).to_be_bytes();
            udp[4..6].copy_from_slice(&length);
            udp
        };
        let mut frame = ipv4_frame(17, &udp);
        // Destination port 13400 is what selects DoIP.
        frame[36] = 0x34;
        frame[37] = 0x58;
        let dissection = dissect_frame(&frame).expect("dissection");
        assert_eq!(dissection.protocol, "UDS/RDBI");
        assert!(
            dissection.layers.contains(&"uds"),
            "{:?}",
            dissection.layers
        );
        let text = dissection.field_text();
        assert!(text.contains("doip.sourceAddress=0x0E00"), "{text}");
        assert!(text.contains("doip.targetAddress=0x1001"), "{text}");
        assert!(
            text.contains("uds.serviceName=ReadDataByIdentifier"),
            "{text}"
        );
        assert!(text.contains("uds.dataIdentifier=0xF190"), "{text}");
    }

    #[test]
    fn formats_addresses_for_display() {
        assert_eq!(format_mac(&[2, 0, 0, 0, 0, 1]), "02:00:00:00:00:01");
        assert_eq!(format_ipv4(&[10, 0, 0, 1]), "10.0.0.1");
        assert_eq!(
            format_ipv6(&[0x20, 0x01, 0x0D, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            "2001:db8::1"
        );
    }
}
