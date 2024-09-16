extern crate lighthouse_network;

use crate::lighthouse_network::{SSZSnappyInboundCodec, SSZSnappyOutboundCodec};
use asynchronous_codec::Decoder as AsyncCodecDecoder;
use bytes::BytesMut;
use gossipsub::{GossipHandlerEvent, GossipsubCodec, ValidationMode};
use lighthouse_network::rpc::SupportedProtocol;
use lighthouse_network::GossipTopic;
use std::env;
use std::fs::File;
use std::io::{self, BufRead, Error, Write};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use tokio_util::codec::Decoder;
use types::{ChainSpec, Config, EthSpec, ForkContext, Hash256, MainnetEthSpec};

type E = MainnetEthSpec;

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() != 3 && args.len() != 5 {
        eprintln!("Usage: libp2p-packet-parser [source_file] [output_file_path] <config_file> <genesis_validator_root>");
        std::process::exit(1);
    }

    let config_file = &args[args.len() - 2];
    let genesis_validators_root = &args[args.len() - 1];

    let config = Config::from_file(Path::new(config_file)).unwrap();
    let spec = ChainSpec::from_config::<E>(&config).unwrap();
    let genesis_validators_root = Hash256::from_str(&genesis_validators_root).unwrap();
    let fork_context = Arc::new(ForkContext::new::<E>(
        spec.deneb_fork_epoch
            .unwrap()
            .start_slot(E::slots_per_epoch()),
        genesis_validators_root,
        &spec,
    ));

    // File mode: 4 arguments
    if args.len() == 5 {
        let source_file = &args[1];
        let output_file_path = &args[2];
        process_file(source_file, output_file_path, fork_context)?;
    } else {
        // Streaming mode: 2 arguments
        let stdin = io::stdin();
        let handle = stdin.lock();
        let mut buffered = io::BufReader::new(handle);
        process_lines(&mut buffered, |packet| {
            handle_packet(packet, fork_context.clone(), &mut io::stdout()).unwrap();
        });
    }

    Ok(())
}

/// Process packets from file mode
fn process_file(
    source_file: &String,
    output_file_path: &String,
    fork_context: Arc<ForkContext>,
) -> io::Result<()> {
    // Read packets from file and process them
    let file = File::open(Path::new(source_file))?;
    let mut reader = io::BufReader::new(file);
    let mut packets: Vec<NetworkPacket> = Vec::new();
    process_lines(&mut reader, |packet| packets.push(packet));

    let mut output_file = File::create(Path::new(output_file_path))?;
    let mut payload_count = 0;
    let packet_count = packets.len();

    for packet in packets {
        handle_packet(packet, fork_context.clone(), &mut output_file).inspect(|payload_found| {
            if *payload_found {
                payload_count += 1;
            }
        })?;
    }

    println!(
        "Successfully parsed file:\n   Number of Payloads: {}\n    Number of Packets: {}\n   Output file: {}",
        payload_count, packet_count, output_file_path
    );

    Ok(())
}

/// Shared logic for handling packet data
fn handle_packet(
    packet: NetworkPacket,
    fork_context: Arc<ForkContext>,
    output: &mut dyn Write,
) -> io::Result<bool> {
    let NetworkPacket {
        timestamp,
        source_ip,
        dest_ip,
        data,
    } = packet;

    let mut payload_found = false;
    if let Some(payload) = parse_packet_data(&data) {
        payload_found = true;
        let result = decode_gossip_payload(fork_context.spec.gossip_max_size, payload)
            .map(|p| ("Gossip", p))
            .or_else(|_| {
                decode_rpc_response(payload, fork_context.clone())
                    .map(|r| ("RPC Response", vec![r]))
            })
            .or_else(|_| {
                decode_rpc_request(payload, fork_context.clone()).map(|r| ("RPC Request", vec![r]))
            });

        match result {
            Ok((payload_type, parsed_packets)) => {
                parsed_packets.iter().for_each(|(protocol, data)| {
                    let output_line = format!(
                        "{} Source: {:>15}, Dest: {:>15}, Type {:>10}, Protocol: {}, Data: {}",
                        timestamp, source_ip, dest_ip, payload_type, protocol, data
                    );
                    writeln!(output, "{}", output_line).unwrap();
                });
            }
            Err(_) => {}
        }
    }

    Ok(payload_found)
}

/// Represents a network packet with timestamp, source IP, destination IP, and packet data.
struct NetworkPacket {
    timestamp: String,
    source_ip: String,
    dest_ip: String,
    data: Vec<u8>,
}

fn process_lines<R, F>(reader: &mut R, mut handle_packet: F)
where
    R: BufRead,
    F: FnMut(NetworkPacket),
{
    let mut current_packet: Vec<u8> = Vec::new();
    let mut timestamp = String::new();
    let mut source_ip = String::new();
    let mut dest_ip = String::new();

    for line in reader.lines() {
        let line = line.unwrap();

        // Detect the start of a new packet based on the timestamp pattern (e.g., "12:33:16.043916")
        if line.contains(':') && line.contains('.') && line.contains("IP ") {
            if !current_packet.is_empty() {
                handle_packet(NetworkPacket {
                    timestamp: timestamp.clone(),
                    source_ip: source_ip.clone(),
                    dest_ip: dest_ip.clone(),
                    data: current_packet.clone(),
                });
                current_packet.clear();
            }

            // Extract the timestamp, source and destination IP from the tcpdump log line
            if let Some((ts, ip_info)) = line.split_once("IP") {
                timestamp = ts.trim().to_string();
                let ip_parts: Vec<&str> = ip_info.split_whitespace().collect();
                if ip_parts.len() > 3 {
                    source_ip = ip_parts[0].to_string();
                    dest_ip = ip_parts[2].trim_end_matches(':').to_string();
                }
            }
        }

        // Check if the line starts with a hex offset (e.g., "0x0000:")
        if let Some(hex_data) = line.split_once(':') {
            let data = hex_data.1.trim(); // Get the hex string part after the offset
            let bytes: Vec<u8> = data
                .split_whitespace() // Split by whitespace
                .filter_map(|s| u8::from_str_radix(s, 16).ok()) // Convert hex to u8
                .collect();

            // Append bytes to the current packet
            current_packet.extend(bytes);
        }
    }

    // Push the last packet if it's not empty
    if !current_packet.is_empty() {
        handle_packet(NetworkPacket {
            timestamp: timestamp.clone(),
            source_ip: source_ip.clone(),
            dest_ip: dest_ip.clone(),
            data: current_packet,
        });
    }
}

/// returns (Source IP, Destination IP, Payload).
fn parse_packet_data(packet: &[u8]) -> Option<&[u8]> {
    // Ensure the packet is large enough to contain the IP addresses (minimum IP header size is 20 bytes)
    if packet.len() < 20 {
        return None;
    }

    // The IP header length is in the first byte of the IP header (lower nibble).
    let ip_header_len = (packet[0] & 0x0F) * 4; // IP header length in bytes

    // Ensure the IP header is within bounds
    if packet.len() < ip_header_len as usize {
        return None;
    }

    // TCP header starts after the IP header.
    let tcp_header_start = ip_header_len as usize;

    // Ensure we have enough bytes for the TCP header (minimum TCP header size is 20 bytes)
    if packet.len() < tcp_header_start + 20 {
        return None;
    }

    // The TCP header length is in the first byte of the TCP header (upper nibble).
    let tcp_header_len = ((packet[tcp_header_start + 12] >> 4) & 0xF) * 4; // TCP header length in bytes

    // Ensure the full TCP header is within bounds
    if packet.len() < tcp_header_start + tcp_header_len as usize {
        return None;
    }

    // The payload starts after the TCP header.
    let payload_start = tcp_header_start + tcp_header_len as usize;

    // Ensure the payload starts within the bounds of the packet
    if payload_start < packet.len() {
        Some(&packet[payload_start..])
    } else {
        None // No payload found
    }
}

/// returns (protocol, data)
fn decode_rpc_request(
    payload: &[u8],
    fork_context: Arc<ForkContext>,
) -> Result<(String, String), String> {
    let protocol_ids = SupportedProtocol::currently_supported(&fork_context);
    for p in protocol_ids {
        let mut codec = SSZSnappyInboundCodec::<E>::new(p.clone(), 20000, fork_context.clone());
        let mut bytes = BytesMut::from(payload);
        if let Ok(r) = codec.decode(&mut bytes) {
            return Ok((
                p.versioned_protocol.protocol().to_string(),
                r.map(|req| format!("{:?}", req))
                    .unwrap_or_else(|| "None".to_string()),
            ));
        }
    }

    Err("RPC request not found".to_string())
}

/// returns (protocol, data)
fn decode_rpc_response(
    payload: &[u8],
    fork_context: Arc<ForkContext>,
) -> Result<(String, String), String> {
    let protocol_ids = SupportedProtocol::currently_supported(&fork_context);
    for p in protocol_ids {
        let mut codec = SSZSnappyOutboundCodec::<E>::new(p.clone(), 20000, fork_context.clone());
        let mut bytes = BytesMut::from(payload);
        if let Ok(r) = codec.decode(&mut bytes) {
            return Ok((
                p.versioned_protocol.protocol().to_string(),
                r.map(|req| format!("{:?}", req))
                    .unwrap_or_else(|| "None".to_string()),
            ));
        }
    }

    Err("RPC response not found".to_string())
}

/// returns [(protocol, data)]
fn decode_gossip_payload(max_len: u64, payload: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut codec = GossipsubCodec::new(max_len as usize, ValidationMode::Anonymous);
    let mut bytes = BytesMut::from(payload);
    let mut msgs = vec![];

    if let Some(GossipHandlerEvent::Message {
        rpc,
        invalid_messages: _,
    }) = codec.decode(&mut bytes).map_err(|e| e.to_string())?
    {
        // println!(
        //     "{} messages, {} control_msgs, {} subscriptions, {} invalid_messages",
        //     rpc.messages.len(),
        //     rpc.control_msgs.len(),
        //     rpc.subscriptions.len(),
        //     invalid_messages.len(),
        // );
        for msg in rpc.messages {
            if let Ok(msg) = gossip_inbound_transform(msg) {
                let topic = GossipTopic::decode(msg.topic.as_str()).unwrap();
                msgs.push((topic.to_string(), "".to_string()));
            }
        }

        return Ok(msgs);
    } else {
        Err("Gossip msg not found".to_string())
    }
}

fn gossip_inbound_transform(
    raw_message: gossipsub::RawMessage,
) -> Result<gossipsub::Message, Error> {
    let mut decoder = snap::raw::Decoder::new();
    let decompressed_data = decoder.decompress_vec(&raw_message.data)?;
    Ok(gossipsub::Message {
        source: raw_message.source,
        data: decompressed_data,
        sequence_number: raw_message.sequence_number,
        topic: raw_message.topic,
    })
}
