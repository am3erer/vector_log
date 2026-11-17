//! Minimal pcapng writer for turning a trace into something Wireshark opens.
//!
//! Only the blocks a capture needs are written: a section header, one interface
//! description per link type, and one enhanced packet block per frame. Timestamps
//! keep microsecond resolution, which is the pcapng default.
//!
//! Ethernet and CAN frames are supported. LIN has no pcapng link type that the
//! writer can fill without a pseudo-header, so LIN frames are counted and left
//! out; `ExportReport::skipped` tells the caller about them.

use std::collections::HashMap;

/// Link types written by this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkType {
    /// `LINKTYPE_ETHERNET`
    Ethernet,
    /// `LINKTYPE_CAN_SOCKETCAN`
    SocketCan,
}

impl LinkType {
    fn value(self) -> u32 {
        match self {
            Self::Ethernet => 1,
            Self::SocketCan => 227,
        }
    }
}

/// How many frames of each kind ended up in the capture.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExportReport {
    pub ethernet: usize,
    pub can: usize,
    pub skipped: usize,
}

/// Incremental pcapng writer.
#[derive(Debug)]
pub struct PcapWriter {
    output: Vec<u8>,
    interfaces: HashMap<LinkType, u32>,
    report: ExportReport,
}

impl Default for PcapWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl PcapWriter {
    pub fn new() -> Self {
        let mut writer = Self {
            output: Vec::new(),
            interfaces: HashMap::new(),
            report: ExportReport::default(),
        };
        writer.write_section_header();
        writer
    }

    /// Add one Ethernet frame; `frame` is the complete L2 frame.
    pub fn write_ethernet(&mut self, timestamp_ns: u64, frame: &[u8]) {
        let interface = self.interface(LinkType::Ethernet);
        self.write_packet(interface, timestamp_ns, frame);
        self.report.ethernet += 1;
    }

    /// Add one CAN frame as a SocketCAN packet.
    pub fn write_can(
        &mut self,
        timestamp_ns: u64,
        channel: u16,
        id: u32,
        extended: bool,
        data: &[u8],
    ) {
        let interface = self.interface(LinkType::SocketCan);
        let mut packet = Vec::with_capacity(16);
        // SocketCAN keeps the channel in the upper bits of the CAN id and the
        // flag bits in the top three bits.
        let mut can_id = id & 0x1FFF_FFFF;
        if extended {
            can_id |= 0x8000_0000;
        }
        can_id |= u32::from(channel & 0xFF) << 24;
        packet.extend_from_slice(&can_id.to_ne_bytes());
        packet.push(data.len().min(8) as u8);
        packet.extend_from_slice(&[0; 3]);
        let mut payload = [0_u8; 8];
        payload[..data.len().min(8)].copy_from_slice(&data[..data.len().min(8)]);
        packet.extend_from_slice(&payload);
        self.write_packet(interface, timestamp_ns, &packet);
        self.report.can += 1;
    }

    /// Count a frame that this writer cannot represent.
    pub fn skip(&mut self) {
        self.report.skipped += 1;
    }

    pub fn report(&self) -> ExportReport {
        self.report
    }

    /// Finish the capture and return its bytes.
    pub fn finish(self) -> Vec<u8> {
        self.output
    }

    fn interface(&mut self, link_type: LinkType) -> u32 {
        if let Some(id) = self.interfaces.get(&link_type) {
            return *id;
        }
        let id = u32::try_from(self.interfaces.len()).unwrap_or(0);
        self.interfaces.insert(link_type, id);
        self.write_interface_description(link_type);
        id
    }

    fn write_section_header(&mut self) {
        let mut body = Vec::new();
        body.extend_from_slice(&0x1A2B_3C4D_u32.to_ne_bytes()); // byte-order magic
        body.extend_from_slice(&1_u16.to_ne_bytes()); // major version
        body.extend_from_slice(&0_u16.to_ne_bytes()); // minor version
        body.extend_from_slice(&u64::MAX.to_ne_bytes()); // section length unknown
        self.write_block(0x0A0D_0D0A, &body);
    }

    fn write_interface_description(&mut self, link_type: LinkType) {
        let mut body = Vec::new();
        // LinkType is a 16-bit field in an interface description block.
        body.extend_from_slice(&(link_type.value() as u16).to_ne_bytes());
        body.extend_from_slice(&0_u16.to_ne_bytes()); // reserved
        body.extend_from_slice(&65535_u32.to_ne_bytes()); // snaplen
                                                          // `if_tsresol` = 6 means microseconds, the default resolution.
        let option_header = 9_u16.to_ne_bytes();
        body.extend_from_slice(&option_header);
        body.extend_from_slice(&1_u16.to_ne_bytes()); // option length
        body.push(6);
        body.extend_from_slice(&[0; 3]); // padding
        body.extend_from_slice(&0_u16.to_ne_bytes()); // end of options
        body.extend_from_slice(&0_u16.to_ne_bytes());
        self.write_block(0x0000_0001, &body);
    }

    fn write_packet(&mut self, interface: u32, timestamp_ns: u64, data: &[u8]) {
        let timestamp = timestamp_ns / 1000; // microseconds
        let mut body = Vec::new();
        body.extend_from_slice(&interface.to_ne_bytes());
        body.extend_from_slice(&((timestamp >> 32) as u32).to_ne_bytes());
        body.extend_from_slice(&(timestamp as u32).to_ne_bytes());
        body.extend_from_slice(&(data.len() as u32).to_ne_bytes());
        body.extend_from_slice(&(data.len() as u32).to_ne_bytes());
        body.extend_from_slice(data);
        while body.len() % 4 != 0 {
            body.push(0);
        }
        self.write_block(0x0000_0006, &body);
    }

    fn write_block(&mut self, block_type: u32, body: &[u8]) {
        let total = (12 + body.len()) as u32;
        self.output.extend_from_slice(&block_type.to_ne_bytes());
        self.output.extend_from_slice(&total.to_ne_bytes());
        self.output.extend_from_slice(body);
        self.output.extend_from_slice(&total.to_ne_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_ne_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ])
    }

    #[test]
    fn writes_a_section_header_and_one_interface_per_link_type() {
        let mut writer = PcapWriter::new();
        writer.write_ethernet(1_000_000, &[0xAA; 60]);
        writer.write_ethernet(2_000_000, &[0xBB; 60]);
        writer.write_can(3_000_000, 1, 0x123, false, &[1, 2, 3]);
        let bytes = writer.finish();

        assert_eq!(read_u32(&bytes, 0), 0x0A0D_0D0A);
        assert_eq!(read_u32(&bytes, 4) as usize, 28);
        // Two interfaces were created: Ethernet and SocketCAN.
        let mut interfaces = 0;
        let mut offset = 0;
        while offset + 12 <= bytes.len() {
            let block_type = read_u32(&bytes, offset);
            let total = read_u32(&bytes, offset + 4) as usize;
            if total == 0 || offset + total > bytes.len() {
                break;
            }
            // Every block length must be a multiple of four.
            assert_eq!(total % 4, 0, "block {block_type:#X} is not aligned");
            if block_type == 0x0000_0001 {
                interfaces += 1;
                let link_type = u16::from_ne_bytes([bytes[offset + 8], bytes[offset + 9]]);
                assert!(link_type == 1 || link_type == 227, "unexpected link type");
            }
            offset += total;
        }
        assert_eq!(interfaces, 2);
    }

    #[test]
    fn counts_frames_and_skips() {
        let mut writer = PcapWriter::new();
        writer.write_ethernet(1_000, &[0; 14]);
        writer.write_can(2_000, 1, 0x7E0, false, &[0x02, 0x10, 0x03]);
        writer.skip();
        let report = writer.report();
        assert_eq!(report.ethernet, 1);
        assert_eq!(report.can, 1);
        assert_eq!(report.skipped, 1);
    }

    #[test]
    fn packet_blocks_carry_the_frame_bytes() {
        let mut writer = PcapWriter::new();
        writer.write_ethernet(1_234_000, &[0x11; 20]);
        let bytes = writer.finish();
        // The packet block follows the section header (28) and the interface
        // description (20 + 20-byte option area + 12 header/trailer).
        let mut offset = 0;
        let mut found = false;
        while offset + 12 <= bytes.len() {
            let block_type = read_u32(&bytes, offset);
            let total = read_u32(&bytes, offset + 4) as usize;
            if total == 0 || offset + total > bytes.len() {
                break;
            }
            if block_type == 0x0000_0006 {
                assert_eq!(read_u32(&bytes, offset + 20), 20); // captured length
                assert_eq!(bytes[offset + 28], 0x11);
                found = true;
            }
            offset += total;
        }
        assert!(found, "no packet block written");
    }
}
