use alvr_common::{anyhow::Result, info, warn, ALVR_NAME};
use alvr_sockets::CONTROL_PORT;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};

// Rounds (one per DISCOVERY_RETRY_PAUSE, 0.5 s) spent announcing on wired interfaces only before
// Wi-Fi is added. The streamer connects back to whichever address an announcement came from,
// and with Wi-Fi and a USB Ethernet adapter both up it used to find the headset over Wi-Fi:
// the announcement went out only on the default-route interface, which is the Wi-Fi one
// (2026-09-22). About three seconds for a streamer on the cable to answer before Wi-Fi is tried.
const WIRED_ONLY_ROUNDS: u32 = 6;

struct Interface {
    name: String,
    address: Ipv4Addr,
    broadcast: Ipv4Addr,
    socket: UdpSocket,
}

impl Interface {
    // On Apple devices en0 is the Wi-Fi interface; a USB/Thunderbolt Ethernet adapter shows up as
    // another enN. Anything else that can broadcast (bridges, VPN tunnels) is not announced on.
    fn is_wifi(&self) -> bool {
        self.name == "en0"
    }
}

pub struct AnnouncerSocket {
    packet: [u8; 56],
    interfaces: Vec<Interface>,
    rounds: u32,
}

impl AnnouncerSocket {
    pub fn new(hostname: &str) -> Result<Self> {
        let mut packet = [0; 56];
        packet[0..ALVR_NAME.len()].copy_from_slice(ALVR_NAME.as_bytes());
        packet[16..24].copy_from_slice(&alvr_common::protocol_id_u64().to_le_bytes());
        packet[24..24 + hostname.len()].copy_from_slice(hostname.as_bytes());

        Ok(Self {
            packet,
            interfaces: vec![],
            rounds: 0,
        })
    }

    // One announcement round. Each interface gets its own socket bound to its own address and
    // sends to its own subnet's broadcast address, so the OS cannot route every announcement out
    // of the default interface. Wired first; Wi-Fi only when there is no wired interface, or
    // after WIRED_ONLY_ROUNDS rounds without a streamer answering.
    pub fn announce_broadcast(&mut self) -> Result<()> {
        self.refresh_interfaces();
        self.rounds += 1;

        let has_wired = self.interfaces.iter().any(|i| !i.is_wifi());
        let wifi_too = !has_wired || self.rounds > WIRED_ONLY_ROUNDS;
        if has_wired && self.rounds == WIRED_ONLY_ROUNDS + 1 {
            info!("No streamer answered on the wired interface(s), announcing on Wi-Fi too");
        }

        let mut sent = 0;
        let mut last_error = None;
        for interface in &self.interfaces {
            if interface.is_wifi() && !wifi_too {
                continue;
            }
            match interface
                .socket
                .send_to(&self.packet, (interface.broadcast, CONTROL_PORT))
            {
                Ok(_) => sent += 1,
                Err(e) => last_error = Some(e),
            }
        }

        match (sent, last_error) {
            (0, Some(e)) => Err(e.into()),
            (0, None) => alvr_common::anyhow::bail!("no network interface to announce on"),
            _ => Ok(()),
        }
    }

    // Keeps one socket per usable IPv4 interface, rebuilding the set only when it changed (an
    // adapter plugged in, a new link-local address).
    fn refresh_interfaces(&mut self) {
        let current = list_broadcast_interfaces();
        let unchanged = current.len() == self.interfaces.len()
            && current.iter().all(|(name, address, broadcast)| {
                self.interfaces.iter().any(|i| {
                    &i.name == name && i.address == *address && i.broadcast == *broadcast
                })
            });
        if unchanged {
            return;
        }

        self.interfaces.clear();
        for (name, address, broadcast) in current {
            match UdpSocket::bind((IpAddr::V4(address), CONTROL_PORT)).and_then(|socket| {
                socket.set_broadcast(true)?;
                Ok(socket)
            }) {
                Ok(socket) => {
                    info!("Announcing on {name} ({address} -> {broadcast})");
                    self.interfaces.push(Interface {
                        name,
                        address,
                        broadcast,
                        socket,
                    });
                }
                Err(e) => warn!("Cannot announce on {name} ({address}): {e}"),
            }
        }
        // Wired interfaces first, so they are also first in every round.
        self.interfaces.sort_by_key(|i| i.is_wifi());
    }
}

// (name, address, broadcast address) of every interface that is up, not loopback, can broadcast
// and has an IPv4 address.
#[cfg(unix)]
fn list_broadcast_interfaces() -> Vec<(String, Ipv4Addr, Ipv4Addr)> {
    use std::ffi::CStr;

    let mut result = vec![];
    let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut addrs) } != 0 {
        return result;
    }
    let mut cursor = addrs;
    while !cursor.is_null() {
        let ifa = unsafe { &*cursor };
        cursor = ifa.ifa_next;

        let flags = ifa.ifa_flags as libc::c_int;
        let wanted = libc::IFF_UP | libc::IFF_RUNNING | libc::IFF_BROADCAST;
        if flags & wanted != wanted || flags & libc::IFF_LOOPBACK != 0 {
            continue;
        }
        if ifa.ifa_addr.is_null() || ifa.ifa_netmask.is_null() {
            continue;
        }
        if unsafe { (*ifa.ifa_addr).sa_family } as libc::c_int != libc::AF_INET {
            continue;
        }
        let v4 = |sa: *const libc::sockaddr| {
            let sin = unsafe { &*(sa as *const libc::sockaddr_in) };
            Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))
        };
        let address = v4(ifa.ifa_addr);
        let netmask = v4(ifa.ifa_netmask);
        let broadcast = Ipv4Addr::from(u32::from(address) | !u32::from(netmask));
        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }
            .to_string_lossy()
            .into_owned();
        result.push((name, address, broadcast));
    }
    unsafe { libc::freeifaddrs(addrs) };
    result
}

// Other platforms: the previous behaviour, one announcement from the default interface.
#[cfg(not(unix))]
fn list_broadcast_interfaces() -> Vec<(String, Ipv4Addr, Ipv4Addr)> {
    match alvr_system_info::local_ip() {
        IpAddr::V4(address) => vec![("default".into(), address, Ipv4Addr::BROADCAST)],
        IpAddr::V6(_) => vec![],
    }
}
