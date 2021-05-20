use std::{io::{Read, Write}, net::{IpAddr, TcpStream}};
use anyhow::Result;
use serde::{Deserialize, Deserializer};

/// An Error with all the possible runtime errors that could occur specific to the program.
#[derive(Debug)]
pub enum RunningError {
    VatsimDisconnected,
    StreamDisconnected
}

impl std::fmt::Display for RunningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunningError::VatsimDisconnected => write!(f, "Lost connection to the VATSIM server."),
            RunningError::StreamDisconnected => write!(f, "Lost connection to the server.")
        }
    }
}

impl std::error::Error for RunningError {}

/// Converts a String into an IpAddr by either parsing it, or resolving the hostname
pub fn to_ip<'de, D>(deserializer: D) -> Result<Option<IpAddr>, D::Error>
where
    D: Deserializer<'de>,
{
    let server_str = String::deserialize(deserializer)?;
    // Parse string into an IpAddr or lookup the IPv4 hostname
    Ok(server_str.parse::<IpAddr>().ok().or_else(|| {
        dns_lookup::lookup_host(&server_str)
            .ok()
            .and_then(|ips| ips.into_iter().find(|&x| x.is_ipv4()))
    }))
}

/// Displays a message, then waits for a newline to be entered into the terminal
pub fn display_msg_prompt(msg: &str) {
    println!("{}. Press any key to restart.", msg);
    std::io::stdin().read_line(&mut String::new()).ok();
}

/// A wrapper for a TcpStream mainly for read/write operations with Strings 
//// with an extra field for determining whether the stream is a radar client connection
pub struct StreamInfo {
    stream: TcpStream,
    pub is_radar_client: bool,
}

impl StreamInfo {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            is_radar_client: false
        }
    }

    /// Read a string from the stream
    pub fn read_string(&mut self) -> Result<String> {
        let mut buf = String::new();

        match self.stream.read_to_string(&mut buf) {
            Ok(0) => return Err(RunningError::StreamDisconnected.into()),
            _ => {}
        }

        Ok(buf)
    }

    /// Write a string to the stream
    pub fn write_str(&mut self, msg: &str) {
        self.stream.write_all(msg.as_bytes()).ok();
    }
}