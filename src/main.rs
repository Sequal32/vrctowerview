use anyhow::Result;
use attohttpc;
use fsdparser::{PacketTypes, Parser};
use log::{info, LevelFilter};
use serde::{Deserialize, Deserializer};
use simplelog::{ColorChoice, Config, TermLogger, TerminalMode};
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::net::{IpAddr, TcpListener, TcpStream};
use std::thread::sleep;
use std::time::{Duration, Instant};
use std::{io::Write, net::SocketAddr};
use winping::{Buffer, Pinger};

const VATSIM_DATA_FEED: &str = "https://data.vatsim.net/v3/vatsim-data.json";

#[derive(Debug)]
enum RunningError {
    ATCClientDisconnected,
    VatsimDisconnected,
}

impl std::fmt::Display for RunningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunningError::ATCClientDisconnected => {
                write!(f, "Lost connection with the ATC client.")
            }
            RunningError::VatsimDisconnected => write!(f, "Lost connection to the VATSIM server."),
        }
    }
}

impl std::error::Error for RunningError {}

fn to_ip<'de, D>(deserializer: D) -> Result<Option<IpAddr>, D::Error>
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

#[derive(Deserialize, Debug)]
pub struct VatsimData {
    servers: Vec<VatsimServer>,
}

#[derive(Deserialize, Debug)]
pub struct VatsimServer {
    name: String,
    #[serde(rename = "hostname_or_ip")]
    #[serde(deserialize_with = "to_ip")]
    pub ip: Option<IpAddr>,
}

#[derive(Debug)]
pub struct VatsimServerData<'a> {
    pub server: &'a VatsimServer,
    pub ping: u32,
}

pub struct VatsimServers {
    server_data: Vec<VatsimServer>,
    pinger: Pinger,
}

impl VatsimServers {
    pub fn new() -> Self {
        Self {
            server_data: Vec::new(),
            pinger: Pinger::new_v4().unwrap(),
        }
    }

    pub fn fetch_data(&mut self) -> Result<()> {
        let data = attohttpc::get(VATSIM_DATA_FEED)
            .send()?
            .error_for_status()?
            .json::<VatsimData>()?;

        self.server_data = data.servers;

        Ok(())
    }

    pub fn get_servers(&self) -> &Vec<VatsimServer> {
        &self.server_data
    }

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

///
pub struct VatsimConnector {
    stream: TcpStream,
    connected_to: String,

    my_callsign: Option<String>,
    seen_aircraft: HashMap<String, Instant>,
    aircraft_request_queue: VecDeque<String>,
    tick: Instant,
}

impl VatsimConnector {
    pub fn connect_to_best_server() -> Result<Self> {
        let mut servers = VatsimServers::new();
        servers.fetch_data()?;

        let best_server = servers.get_servers_ping()?.first().unwrap().server;

        let stream = TcpStream::connect(SocketAddr::new(best_server.ip.unwrap(), 6809))?;
        stream.set_nonblocking(true).ok();

        Ok(Self {
            stream,
            connected_to: best_server.name.clone(),
            my_callsign: None,
            seen_aircraft: HashMap::new(),
            aircraft_request_queue: VecDeque::new(),
            tick: Instant::now(),
        })
    }

    pub fn request_full_data_for(&mut self, callsign: &str) {
        let payload = format!(
            "$CQ{}:{}:ACC:{{\"request\":\"full\"}}\r\n",
            if let Some(callsign) = self.my_callsign.as_ref() {
                callsign
            } else {
                ""
            },
            callsign
        );

        self.stream.write_all(payload.as_bytes()).ok();
    }

    fn remove_old_aircraft(&mut self) {
        self.seen_aircraft
            .retain(|_, time| time.elapsed().as_secs() <= 15);
    }

    fn process_message(&mut self, msg: &str) {
        // Sometimes the server sends multiple messages in a single packet
        for msg in msg.split("\n") {
            match Parser::parse(msg) {
                Some(PacketTypes::PilotPosition(p)) => {
                    // Request full aircraft configuration
                    let seen = self
                        .seen_aircraft
                        .insert(p.callsign.clone(), Instant::now());

                    if seen.is_none() {
                        info!("Request full aircraft config for {}", p.callsign);
                        self.aircraft_request_queue.push_back(p.callsign);
                    }
                }
                _ => {}
            }
        }

        self.remove_old_aircraft();
    }

    fn get_next_message(&mut self) -> Result<String> {
        let mut msg = String::new();

        match self.stream.read_to_string(&mut msg) {
            Ok(0) => return Err(RunningError::VatsimDisconnected.into()),
            _ => {}
        }

        self.process_message(&msg);

        Ok(msg)
    }

    fn poll_aircraft_queue(&mut self) {
        if self.my_callsign.is_none() {
            return;
        };

        if let Some(callsign) = self.aircraft_request_queue.pop_front() {
            self.request_full_data_for(&callsign);
        }
    }

    pub fn poll(&mut self) -> Result<String> {
        // Check for old aircraft every 5 seconds
        if self.tick.elapsed().as_secs() >= 5 {
            self.remove_old_aircraft();
            self.tick = Instant::now();
        }
        self.poll_aircraft_queue();
        // Receive data from the VATSIM server
        self.get_next_message()
    }

    pub fn write_message(&mut self, string: &str) {
        self.stream.write_all(string.as_bytes()).ok();
    }

    pub fn set_my_callsign(&mut self, callsign: String) {
        self.my_callsign = Some(callsign);
    }

    pub fn is_callsign_set(&self) -> bool {
        self.my_callsign.is_some()
    }
}

struct StreamInfo {
    stream: TcpStream,
    is_radar_client: bool,
}

pub struct VatsimTowerViewProxy {
    listener: TcpListener,
    // Connected streams
    streams: Vec<StreamInfo>,
    // The connection to VATSIM
    connector: Option<VatsimConnector>,
    callsign_found: Option<String>,
}

impl VatsimTowerViewProxy {
    pub fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:6809").expect("Failed to bind server");
        listener.set_nonblocking(true).ok();

        Self {
            listener,
            streams: Vec::new(),
            connector: None,
            callsign_found: None,
        }
    }

    fn accept_connections(&mut self) {
        match self.listener.accept() {
            Ok((stream, addr)) => {
                info!("Connection from {} connected to the proxy server.", addr);
                self.streams.push(StreamInfo {
                    stream,
                    is_radar_client: false,
                });
            }
            Err(_) => {}
        }
    }

    fn try_get_callsign(msg: &str) -> Option<String> {
        match Parser::parse(&msg) {
            Some(PacketTypes::ATCPosition(atc)) => return Some(atc.callsign),
            _ => {}
        }
        return None;
    }

    fn is_plane_info_request(msg: &str) -> bool {
        match Parser::parse(&msg) {
            Some(PacketTypes::PlaneInfoRequest(_)) => true,
            _ => false,
        }
    }

    pub fn poll(&mut self) -> Result<()> {
        self.accept_connections();

        if self.streams.len() == 0 {
            return Ok(());
        }
        // Connect to VATSIM if not connected
        if self.connector.is_none() && self.callsign_found.is_some() {
            let mut connector = VatsimConnector::connect_to_best_server()?;
            connector.set_my_callsign(self.callsign_found.take().unwrap());

            info!("Connected to the {} VATSIM Server", connector.connected_to);

            self.connector = Some(connector);
        }

        // RECEIVE DATA
        if let Some(connector) = self.connector.as_mut() {
            match connector.poll() {
                Ok(msg) => {
                    // Relay VATSIM server message to connections
                    for stream_info in self.streams.iter_mut() {
                        stream_info.stream.write_all(msg.as_bytes()).ok();
                    }
                }
                Err(_) => {}
            }
        }
        // WRITE DATA
        for stream_info in self.streams.iter_mut() {
            let mut msg = String::new();

            match stream_info.stream.read_to_string(&mut msg) {
                Ok(0) => return Err(RunningError::VatsimDisconnected.into()),
                _ => {}
            };

            if msg.len() == 0 {
                continue;
            }

            if let Some(callsign) = Self::try_get_callsign(&msg) {
                self.callsign_found = Some(callsign);
                stream_info.is_radar_client = true;
            }

            if let Some(connector) = self.connector.as_mut() {
                if stream_info.is_radar_client {
                    connector.write_message(&msg);
                } else if Self::is_plane_info_request(&msg) {
                    let new_msg = msg.replace("TOWER", connector.my_callsign.as_ref().unwrap()); // FIXME:
                    connector.write_message(&new_msg)
                }
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

    let mut proxy = VatsimTowerViewProxy::new();

    loop {
        proxy.poll();
        sleep(Duration::from_millis(50));
    }

    Ok(())
}
