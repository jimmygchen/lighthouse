extern crate lighthouse_network;

use crate::lighthouse_network::{SSZSnappyInboundCodec, SSZSnappyOutboundCodec};
use asynchronous_codec::Decoder as AsyncCodecDecoder;
use bytes::BytesMut;
use gossipsub::{GossipHandlerEvent, GossipsubCodec, ValidationMode};
use lighthouse_network::rpc::SupportedProtocol;
use lighthouse_network::GossipTopic;
use snap::raw::decompress_len;
use std::env;
use std::fs::File;
use std::io::{self, BufRead, Error, ErrorKind, Write};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use tokio_util::codec::Decoder;
use types::{ChainSpec, Config, EthSpec, ForkContext, Hash256, MainnetEthSpec};

type E = MainnetEthSpec;

fn main() -> io::Result<()> {
    // Collect command-line arguments
    let args: Vec<String> = env::args().collect();
    println!("{:?}", args);
    // Check if enough arguments are provided
    if args.len() != 5 {
        eprintln!("Usage: cl-network-packet-parser <source_file> <output_file_path> <config_file> <genesis_validator_root>");
        std::process::exit(1);
    }

    // Get the source file and output file paths
    let source_file = &args[1];
    let output_file_path = &args[2];
    let config_file = &args[3];
    let genesis_validators_root = &args[4];

    // Open the source file for reading
    let network_packets = parse_tcpdump_file(source_file)?;

    // Load chain config
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

    // Open the output file for writing
    let mut output_file = File::create(Path::new(output_file_path))?;
    let num_packets = network_packets.len();

    let mut payload_count: usize = 0;
    for packet in network_packets {
        if let Some((source_ip, target_ip, payload)) = parse_packet(&packet) {
            payload_count += 1;
            let result = decode_gossip_payload(payload)
                .map(|p| ("Gossip", p))
                .or_else(|_| {
                    decode_rpc_request(payload, fork_context.clone())
                        .map(|r| ("RPC Request", vec![r]))
                })
                .or_else(|_| {
                    decode_rpc_response(payload, fork_context.clone())
                        .map(|r| ("RPC Response", vec![r]))
                });

            match result {
                Ok((payload_type, parsed_packets)) => {
                    parsed_packets.iter().for_each(|(protocol, data)| {
                        let output_line = format!(
                            "Source IP: {:>15}, Target IP: {:>15}, Type {:>10}, Protocol: {}, Data: {}",
                            source_ip, target_ip, payload_type, protocol, data
                        );
                        // println!("Writing line {}", output_line);
                        writeln!(output_file, "{}", output_line).unwrap();
                    })
                }
                Err(_) => {}
            }
        } else {
            // println!("No payload found in the packet.");
        }
    }

    println!(
        "Successfully parsed file:\n   Number of Payloads: {}\n    Number of Packets: {}\n   Output file: {}",
        payload_count, num_packets, output_file_path
    );
    Ok(())
}

fn parse_tcpdump_file(file_path: &str) -> io::Result<Vec<Vec<u8>>> {
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut current_packet: Vec<u8> = Vec::new();

    // Open the file
    let file = File::open(Path::new(file_path))?;
    let reader = io::BufReader::new(file);

    for line in reader.lines() {
        let line = line?;

        // Detect the start of a new packet based on the timestamp pattern (e.g., "12:33:16.043916")
        // FIXME: This misses that last packet
        if line.contains(':') && line.contains('.') {
            if !current_packet.is_empty() {
                packets.push(current_packet.clone());
                current_packet.clear();
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
        packets.push(current_packet);
    }

    Ok(packets)
}

/// returns (Source IP, Destination IP, Payload).
fn parse_packet(packet: &[u8]) -> Option<(String, String, &[u8])> {
    // Ensure the packet is large enough to contain the IP addresses (minimum IP header size is 20 bytes)
    if packet.len() < 20 {
        return None;
    }

    // Source IP address: bytes 12-15
    let source_ip = format!(
        "{}.{}.{}.{}",
        packet[12], packet[13], packet[14], packet[15]
    );

    // Destination IP address: bytes 16-19
    let dest_ip = format!(
        "{}.{}.{}.{}",
        packet[16], packet[17], packet[18], packet[19]
    );

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
        Some((source_ip, dest_ip, &packet[payload_start..]))
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
        let mut codec = SSZSnappyOutboundCodec::<E>::new(p.clone(), 20000, fork_context.clone());
        let mut bytes = BytesMut::from(payload);
        if let Ok(r) = codec.decode(&mut bytes) {
            // println!("Found RPC request {:?} {:?}", p.versioned_protocol, r);
            return Ok((
                p.versioned_protocol.protocol().to_string(),
                format!("{:?}", r),
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
        let mut codec = SSZSnappyInboundCodec::<E>::new(p.clone(), 20000, fork_context.clone());
        let mut bytes = BytesMut::from(payload);
        if let Ok(r) = codec.decode(&mut bytes) {
            // println!("Found RPC response {:?} {:?}", p.versioned_protocol, r);
            return Ok((
                p.versioned_protocol.protocol().to_string(),
                format!("{:?}", r),
            ));
        }
    }

    Err("RPC response not found".to_string())
}

/// returns [(protocol, data)]
fn decode_gossip_payload(payload: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut codec = GossipsubCodec::new(20000, ValidationMode::Strict);
    let mut bytes = BytesMut::from(payload);
    let mut msgs = vec![];

    if let Some(GossipHandlerEvent::Message { rpc, .. }) =
        codec.decode(&mut bytes).map_err(|e| e.to_string())?
    {
        println!(
            "{} messages, {} control_msgs, {} subscriptions",
            rpc.messages.len(),
            rpc.control_msgs.len(),
            rpc.subscriptions.len()
        );
        for msg in rpc.messages {
            if let Ok(msg) = gossip_inbound_transform(msg) {
                let topic = GossipTopic::decode(msg.topic.as_str()).unwrap();
                msgs.push((topic.to_string(), "".to_string()));
            }
        }
    }

    if msgs.is_empty() {
        Err("Gossip msg not found".to_string())
    } else {
        Ok(msgs)
    }
}

fn gossip_inbound_transform(
    raw_message: gossipsub::RawMessage,
) -> Result<gossipsub::Message, Error> {
    // check the length of the raw bytes
    let len = decompress_len(&raw_message.data)?;
    if len > 10000000 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "ssz_snappy decoded data > GOSSIP_MAX_SIZE",
        ));
    }

    let mut decoder = snap::raw::Decoder::new();
    let decompressed_data = decoder.decompress_vec(&raw_message.data)?;

    // Build the GossipsubMessage struct
    Ok(gossipsub::Message {
        source: raw_message.source,
        data: decompressed_data,
        sequence_number: raw_message.sequence_number,
        topic: raw_message.topic,
    })
}
