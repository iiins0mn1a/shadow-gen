//! Conversion between live simulation event types and serializable snapshots.

use bytes::Bytes;
use shadow_shim_helper_rs::emulated_time::EmulatedTime;
use shadow_shim_helper_rs::simulation_time::SimulationTime;

use super::reconstruct::reconstruct_task;
use super::snapshot_types::*;
use crate::core::work::event::{Event, EventData};
use crate::core::work::event_queue::EventQueue;
use crate::host::host::Host;
use crate::network::packet::PacketRc;

pub fn event_to_snapshot(event: &Event) -> EventSnapshot {
    let time_ns = event
        .time()
        .duration_since(&EmulatedTime::SIMULATION_START)
        .as_nanos() as u64;

    let snapshot_data = match event.data_ref() {
        EventData::Packet(pkt_data) => EventDataSnapshot::Packet(PacketEventSnapshot {
            src_host_id: u32::from(pkt_data.src_host_id()),
            src_host_event_id: pkt_data.src_host_event_id(),
            packet: packet_to_snapshot(pkt_data.packet()),
        }),
        EventData::Local(local_data) => {
            let descriptor = local_data.task().descriptor().cloned().unwrap_or_else(|| {
                log::warn!(
                    "Event (id={}) has no TaskDescriptor; using Opaque fallback",
                    local_data.event_id()
                );
                TaskDescriptor::Opaque {
                    description: format!("{:?}", local_data.task()),
                }
            });
            EventDataSnapshot::Local(LocalEventSnapshot {
                event_id: local_data.event_id(),
                task: descriptor,
            })
        }
    };

    EventSnapshot {
        time_ns,
        data: snapshot_data,
    }
}

/// Convert a live `EventQueue` into a vector of `EventSnapshot`s by draining
/// all events. The queue will be empty after this call.
pub fn drain_event_queue_to_snapshots(queue: &mut EventQueue) -> Vec<EventSnapshot> {
    let mut snapshots = Vec::new();
    while let Some(event) = queue.pop() {
        snapshots.push(event_to_snapshot(&event));
    }
    snapshots
}

/// Rebuild an `EventQueue` from a vector of `EventSnapshot`s.
///
/// Events whose tasks cannot be reconstructed are dropped with a warning.
pub fn rebuild_event_queue(
    snapshots: &[EventSnapshot],
    _host: &Host,
) -> EventQueue {
    let mut queue = EventQueue::new();

    for snap in snapshots {
        let time = EmulatedTime::SIMULATION_START
            + SimulationTime::from_nanos(snap.time_ns);

        match &snap.data {
            EventDataSnapshot::Packet(pkt_snap) => {
                if let Some(packet) = packet_from_snapshot(&pkt_snap.packet) {
                    let packet_rc = PacketRc::from(packet);
                    let event = Event::new_packet_with_ids(
                        packet_rc,
                        time,
                        pkt_snap.src_host_id.into(),
                        pkt_snap.src_host_event_id,
                    );
                    queue.push(event);
                } else {
                    log::warn!("Failed to reconstruct packet event at time_ns={}", snap.time_ns);
                }
            }
            EventDataSnapshot::Local(local_snap) => {
                if let Some(task) = reconstruct_task(&local_snap.task) {
                    let event = Event::new_local_with_id(task, time, local_snap.event_id);
                    queue.push(event);
                } else {
                    log::warn!(
                        "Dropping non-reconstructable event (id={}) at time_ns={}",
                        local_snap.event_id,
                        snap.time_ns
                    );
                }
            }
        }
    }

    queue
}

/// Convert a `Packet` (via `PacketRc`) to a `PacketSnapshot`.
pub fn packet_to_snapshot(packet: &PacketRc) -> PacketSnapshot {
    let src = packet.src_ipv4_address();
    let dst = packet.dst_ipv4_address();
    let priority = packet.priority();

    let payload_bytes: Vec<u8> = packet
        .payload()
        .into_iter()
        .flat_map(|b| b.to_vec())
        .collect();

    let protocol = match packet.iana_protocol() {
        crate::network::packet::IanaProtocol::Tcp => {
            if let Some(tcp_hdr) = packet.ipv4_tcp_header() {
                PacketProtocolSnapshot::Tcp(TcpHeaderSnapshot {
                    src_ip: u32::from(tcp_hdr.ip.src),
                    src_port: tcp_hdr.src_port,
                    dst_ip: u32::from(tcp_hdr.ip.dst),
                    dst_port: tcp_hdr.dst_port,
                    seq: tcp_hdr.seq,
                    ack: tcp_hdr.ack,
                    flags: tcp_hdr.flags.bits(),
                    window: tcp_hdr.window_size,
                    selective_acks: tcp_hdr
                        .selective_acks
                        .map(|sacks| {
                            sacks
                                .iter()
                                .map(|&(a, b)| (a, b))
                                .collect()
                        })
                        .unwrap_or_default(),
                    window_scale: tcp_hdr.window_scale,
                    timestamp: tcp_hdr.timestamp,
                    timestamp_echo: tcp_hdr.timestamp_echo,
                })
            } else {
                log::warn!("TCP packet without accessible header (legacy?); using minimal snapshot");
                PacketProtocolSnapshot::Tcp(TcpHeaderSnapshot {
                    src_ip: u32::from(*src.ip()),
                    src_port: src.port(),
                    dst_ip: u32::from(*dst.ip()),
                    dst_port: dst.port(),
                    seq: 0,
                    ack: 0,
                    flags: 0,
                    window: 0,
                    selective_acks: vec![],
                    window_scale: None,
                    timestamp: None,
                    timestamp_echo: None,
                })
            }
        }
        crate::network::packet::IanaProtocol::Udp => {
            PacketProtocolSnapshot::Udp(UdpHeaderSnapshot {
                src_ip: u32::from(*src.ip()),
                src_port: src.port(),
                dst_ip: u32::from(*dst.ip()),
                dst_port: dst.port(),
            })
        }
    };

    PacketSnapshot {
        protocol,
        payload: payload_bytes,
        priority,
    }
}

/// Reconstruct a `Packet` from a `PacketSnapshot`.
///
/// Uses the public packet constructors. For TCP, selective_acks are
/// currently not restored (set to None) because `SmallArrayBackedSlice`
/// is private in the tcp crate.
pub fn packet_from_snapshot(
    snap: &PacketSnapshot,
) -> Option<crate::network::packet::Packet> {
    use std::net::{Ipv4Addr, SocketAddrV4};

    match &snap.protocol {
        PacketProtocolSnapshot::Tcp(tcp) => {
            let src_ip = Ipv4Addr::from(tcp.src_ip);
            let dst_ip = Ipv4Addr::from(tcp.dst_ip);
            let flags = tcp::TcpFlags::from_bits_truncate(tcp.flags);

            let header = tcp::TcpHeader {
                ip: tcp::Ipv4Header {
                    src: src_ip,
                    dst: dst_ip,
                },
                flags,
                src_port: tcp.src_port,
                dst_port: tcp.dst_port,
                seq: tcp.seq,
                ack: tcp.ack,
                window_size: tcp.window,
                selective_acks: None,
                window_scale: tcp.window_scale,
                timestamp: tcp.timestamp,
                timestamp_echo: tcp.timestamp_echo,
            };

            let payload = tcp::Payload(
                snap.payload
                    .chunks(65536)
                    .map(|chunk| Bytes::copy_from_slice(chunk))
                    .collect(),
            );

            Some(crate::network::packet::Packet::new_ipv4_tcp(
                header, payload, snap.priority,
            ))
        }
        PacketProtocolSnapshot::Udp(udp) => {
            let src = SocketAddrV4::new(Ipv4Addr::from(udp.src_ip), udp.src_port);
            let dst = SocketAddrV4::new(Ipv4Addr::from(udp.dst_ip), udp.dst_port);
            let payload = Bytes::copy_from_slice(&snap.payload);
            Some(crate::network::packet::Packet::new_ipv4_udp(
                src, dst, payload, snap.priority,
            ))
        }
    }
}
