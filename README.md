# VRC Tower View

## About

VRC doesn't support towerview on its own so this program bridges the gap.

WARNING: Use at your own risk! This is **unofficial software that connects to VATSIM servers**, however, it should also be perfectly safe to use as it simply relays messages to/from VRC that would be sent/received to the VATSIM server anyway.

## Installing

1. Download the latest [release](https://github.com/Sequal32/vrctowerview/releases/latest).
2. Install [Microsoft Visual C++ Redistributable](https://www.microsoft.com/en-us/download/details.aspx?id=52685).
3. For VRC:
    Open or *create* `myservers.txt` in `Documents/VRC`. Add the following entry: 
    ```
    `127.0.0.1 TOWERVIEWPROXY`
    ```
   For Euroscope:
   Open or *create* `myipaddr.txt` in `Documents/EuroScope`. Add the following entry: 
    ```
    127.0.0.1 TOWERVIEW-PROXY
    ```
4. Start `towerview.exe`
5. Connect using the new server in VRC/Euroscope. (The best VATSIM server will be selected based on ping.)
6. Send `.towerview` in vPilot or the equivalent command in other pilot clients.

## How does it work?

This program serves as a proxy between a VATSIM server, ATC client (VRC), and the towerview client. All messages from the VATSIM server are relayed to the ATC client and vice versa. Only select messages (such as requesting model-matching data for planes) are relayed between the towerview client and the VATSIM server. The connection flow works as the following:

```

Program Start -> Ping VATSIM Servers -> Connect to best server based on Ping -> Messages from server are cached until... 

ATC client connects -> Cached messages sent relayed to ATC Client -> ATC Client responds with a ClientIdentification payload -> 
ATC Callsign retrieved from ID payload -> Messages begin relaying between ATC Client & Vatsim Server -> 
Program keeps track of new pilots connecting/disconnecting and then requests the full aircraft configuration payload from the VATSIM server

Towerview client connects -> Tracked aircraft database reset -> Messages relayed between VATSIM server and towerview client -> 
Towerview free to request model-matching data from the VATSIM server 

```