mod util;

use anyhow::{Context, Result};
use attohttpc;
use fsdparser::{PacketTypes, Parser};
use log::{info, LevelFilter};
use serde::Deserialize;
use simplelog::{ColorChoice, Config, TermLogger, TerminalMode};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::net::{IpAddr, TcpListener, TcpStream};
use std::thread::sleep;
use std::time::{Duration, Instant};
use util::*;
#[cfg(target_os = "windows")]
use winping::{Buffer, Pinger};

const VATSIM_DATA_FEED: &str = "https://data.vatsim.net/v3/vatsim-data.json";

/// Represents the servers fetched from the `VATSIM_DATA_FEED` endpoint.
#[derive(Deserialize, Debug)]
pub struct VatsimData {
    servers: Vec<VatsimServer>,
}

/// Represents a server from the `VATSIM_DATA_FEED` endpoint with its IP parsed
#[derive(Deserialize, Debug)]
pub struct VatsimServer {
    name: String,
    #[serde(rename = "hostname_or_ip")]
    #[serde(deserialize_with = "to_ip")]
    pub ip: Option<IpAddr>,
}

/// A wrapper struct for a `VatsimServer` and the ping from the client to it
#[derive(Debug)]
pub struct VatsimServerData<'a> {
    pub server: &'a VatsimServer,
    pub ping: u32,
}

/// Fetches data from the `VATSIM_DATA_FEED` endpoint with the capability of pinging them.
pub struct VatsimServers {
    server_data: Vec<VatsimServer>,
    #[cfg(target_os = "windows")]
    pinger: Pinger,
}

impl VatsimServers {
    pub fn new() -> Self {
        let mut pinger = Pinger::new_v4().unwrap();
        pinger.set_timeout(1000);

        Self {
            server_data: Vec::new(),
            #[cfg(target_os = "windows")]
            pinger,
        }
    }

    /// Fetches data from a request to the `VATSIM_DATA_FEED` endpoint
    pub fn fetch_data(&mut self) -> Result<()> {
        let data = attohttpc::get(VATSIM_DATA_FEED)
            .send()?
            .error_for_status()?
            .json::<VatsimData>()?;

        self.server_data = data.servers;

        Ok(())
    }

    /// Retrieves the data from the last fetch
    pub fn get_servers(&self) -> &Vec<VatsimServer> {
        &self.server_data
    }

    /// Pings all the VATSIM servers and returns an array sorted based on server RTT
    #[cfg(target_os = "windows")]
    fn get_servers_ping(&mut self) -> Result<Vec<VatsimServerData>> {
        let pinger = &mut self.pinger;

        let mut pings: Vec<VatsimServerData> = self
            .server_data
            .iter()
            .map(|server| {
                // Ping server, or set max ping if can't be reached
                let ping = server
                    .ip
                    .and_then(|ip| pinger.send(ip, &mut Buffer::new()).ok())
                    .unwrap_or(u32::MAX);

                VatsimServerData { server, ping }
            })
            .collect();

        pings.sort_by(|a, b| a.ping.cmp(&b.ping));

        Ok(pings)
    }
}

/// Handles a connection to a VATSIM server
pub struct VatsimConnector {
    // The connection
    stream: StreamInfo,
    // The name of the connected server
    connected_to: String,
    // The callsign to be used when communicating with the server
    my_callsign: String,
    // A callsign map to track added/removed aircraft
    seen_aircraft: HashMap<String, Instant>,
    // Used for polling
    tick: Instant,
}

impl VatsimConnector {
    /// Connects to the best VATSIM server based on ping.
    pub fn connect_to_best_server() -> Result<Self> {
        let mut servers = VatsimServers::new();
        servers.fetch_data()?;

        #[cfg(target_os = "windows")]
        let best_server = {
            let best_servers = servers.get_servers_ping()?;

            for server in best_servers.iter() {
                if server.ping == u32::MAX {
                    info!("{}: Unreachable", server.server.name);
                } else {
                    info!("Ping to {}: {}ms", server.server.name, server.ping)
                }
            }

            best_servers.first().unwrap().server
        };
        #[cfg(not(target_os = "windows"))] // FIXME: seperate OS implementation?
        let best_server = servers.get_servers().get(0).unwrap();

        let stream = TcpStream::connect(SocketAddr::new(best_server.ip.unwrap(), 6809))?;
        stream.set_nonblocking(true).ok();

        Ok(Self {
            stream: StreamInfo::new(stream),
            connected_to: best_server.name.clone(),
            my_callsign: String::new(),
            seen_aircraft: HashMap::new(),
            tick: Instant::now(),
        })
    }

    /// Manually requests a full aircraft configuration data from the server for the specified
    /// `callsign`.
    pub fn request_full_data_for(&mut self, callsign: &str) {
        let payload = format!(
            "$CQ{}:{}:ACC:{{\"request\":\"full\"}}\r\n",
            self.my_callsign, callsign
        );

        self.stream.write_str(&payload);
    }

    /// Removes a tracked aircraft if no position data was received for at least 15 seconds
    fn remove_old_aircraft(&mut self) {
        self.seen_aircraft
            .retain(|_, time| time.elapsed().as_secs() <= 15);
    }

    /// Maps a callsign to an instant in time to keep track of newly added aircraft
    /// and aircraft no longer tracked
    fn add_aircraft(&mut self, callsign: String) {
        let seen = self.seen_aircraft.insert(callsign.clone(), Instant::now());
        // New aircraft added - requests full aircraft configuration
        if seen.is_none() {
            info!("Request full aircraft config for {}", callsign);
            self.request_full_data_for(&callsign);
        }
    }

    fn process_message(&mut self, msg: &str) {
        // Sometimes the server sends multiple messages in a single packet
        for msg in msg.split("\n") {
            match Parser::parse(msg) {
                Some(PacketTypes::PilotPosition(p)) => {
                    self.add_aircraft(p.callsign);
                }
                _ => {}
            }
        }
    }

    /// Processes and reads a message from the server
    fn get_next_message(&mut self) -> Result<Option<String>> {
        match self.stream.read_string() {
            Ok(Some(msg)) => {
                self.process_message(&msg);
                return Ok(Some(msg));
            }
            Ok(None) => return Ok(None),
            Err(_) => return Err(RunningError::VatsimDisconnected.into()),
        }
    }

    pub fn write_message(&mut self, msg: &str) {
        self.stream.write_str(msg);
    }

    pub fn poll(&mut self) -> Result<Option<String>> {
        // Check for old aircraft every 5 seconds
        if self.tick.elapsed().as_secs() >= 5 {
            self.remove_old_aircraft();
            self.tick = Instant::now();
        }
        // Receive data from the VATSIM server
        self.get_next_message()
    }

    /// Sets the callsign to be used when "manually" requesting data from the server
    pub fn set_my_callsign(&mut self, callsign: String) {
        self.my_callsign = callsign;
    }

    pub fn reset_tracked(&mut self) {
        self.seen_aircraft.clear();
    }
}

/// Handles communication between multiple connections and a VATSIM server
pub struct VatsimTowerViewProxy {
    listener: TcpListener,
    // Connected streams
    streams: Vec<StreamInfo>,
    // The connection to VATSIM
    connector: VatsimConnector,
    // MSG queue to relay
    msg_queue: VecDeque<String>,
}

impl VatsimTowerViewProxy {
    /// Starts the proxy server by binding to port 6809 and connecting to the best VATSIM server
    pub fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:6809")
            .with_context(|| "Failed to start proxy server. Is another instance running?")?;
        listener.set_nonblocking(true).ok();

        info!("Attempting to connect to VATSIM...");
        let connector = VatsimConnector::connect_to_best_server()?;
        info!("Connected to VATSIM Server {}", connector.connected_to);

        Ok(Self {
            listener,
            streams: Vec::new(),
            connector,
            msg_queue: VecDeque::new(),
        })
    }

    fn is_radar_client_connected(&self) -> bool {
        self.streams.iter().find(|x| x.is_radar_client).is_some()
    }

    /// Accept incoming connections
    fn accept_connections(&mut self) {
        match self.listener.accept() {
            Ok((stream, addr)) => {
                info!("Connection from {} connected to the proxy server.", addr);
                self.streams.push(StreamInfo::new(stream));

                // Probably the tower view that is connected - need to clear tracked in order to request full aircraft configs
                if self.is_radar_client_connected() {
                    self.connector.reset_tracked();
                }
            }
            Err(_) => {}
        }
    }

    /// Processes communication between connections to the proxy server and the VATSIM server
    pub fn poll(&mut self) -> Result<()> {
        self.accept_connections();

        // Relays messages from the VATSIM server to connections
        match self.connector.poll() {
            Ok(Some(msg)) => {
                self.msg_queue.push_back(msg);
            }
            Err(_) | Ok(None) => {}
        }

        // Write msgs that weren't relayed before a stream connected
        if self.streams.len() > 0 {
            while let Some(mut msg) = self.msg_queue.pop_front() {
                for stream_info in self.streams.iter_mut() {
                    if !stream_info.is_radar_client {
                        msg = msg.replace("ZBW_TM_OBS", "TOWER");
                    }
                    stream_info.write_str(&msg);
                }
            }
        }

        // Read data from connections and relays to the VATSIM server
        for stream_info in self.streams.iter_mut() {
            let msg = match stream_info.read_string() {
                Ok(Some(m)) => m,
                Ok(None) => continue,
                Err(_) => {
                    // Stream disconnected, check if it was the radar client
                    if stream_info.is_radar_client {
                        return Err(RunningError::ATCClientDisconnected.into());
                    } else {
                        continue;
                    }
                }
            };

            let mut should_relay = stream_info.is_radar_client;

            for msg_slice in msg.split("\n") {
                match Parser::parse(&msg_slice) {
                    Some(PacketTypes::PlaneInfoRequest(_)) => {
                        // From the tower view client requesting the info for a plane
                        let msg = msg_slice
                            .to_string()
                            .replace("TOWER", &self.connector.my_callsign); // FIXME:

                        // Since msg can contain multiple payloads, write this message manually
                        self.connector.write_message(&(msg + "\r\n"))
                    }
                    Some(PacketTypes::ClientIdentification(info)) => {
                        // Get the ATC client's callsign from the identification payload it sends
                        self.connector.set_my_callsign(info.from);
                        stream_info.is_radar_client = true;
                        should_relay = true;
                    }
                    _ => {}
                }
            }

            // Relay message
            if should_relay {
                self.connector.write_message(&msg);
            }
        }

        Ok(())
    }
}

fn main() -> Result<()> {
    TermLogger::init(
        LevelFilter::Info,
        Config::default(),
        TerminalMode::Stdout,
        ColorChoice::Auto,
    )
    .ok();

    loop {
        let mut proxy = match VatsimTowerViewProxy::start() {
            Ok(p) => p,
            Err(e) => {
                display_msg_prompt(&format!("Could not start proxy server! Reason: {}", e));
                continue;
            }
        };

        info!("Waiting for connection from the ATC client and the towerview client.");

        loop {
            match proxy.poll() {
                Ok(_) => {}
                Err(e) => {
                    display_msg_prompt(&e.to_string());
                    break;
                }
            }
            sleep(Duration::from_millis(50));
        }
    }
}
