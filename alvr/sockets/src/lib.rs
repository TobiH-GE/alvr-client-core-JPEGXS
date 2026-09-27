mod backend;
mod control_socket;
mod stream_socket;

use alvr_common::{anyhow::Result, info};
use alvr_session::{DscpTos, SocketBufferSize};
use socket2::Socket;
use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

pub use control_socket::*;
pub use stream_socket::*;

pub const LOCAL_IP: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
pub const CONTROL_PORT: u16 = 9943;
pub const HANDSHAKE_PACKET_SIZE_BYTES: usize = 56; // this may change in future protocols
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(500);
pub const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(2);

pub const MDNS_SERVICE_TYPE: &str = "_alvr._tcp.local.";
pub const MDNS_PROTOCOL_KEY: &str = "protocol";
pub const MDNS_DEVICE_ID_KEY: &str = "device_id";

pub const WIRED_CLIENT_HOSTNAME: &str = "client.wired";

// "Maximum" asks for u32::MAX, and so can an over-large Custom value. Linux silently clamps such a
// request to its own limit; Apple's BSD stack REJECTS it (ENOBUFS) instead, and the old code then
// only logged the error and carried on with the tiny default buffer. On the Vision Pro that meant
// "Maximum" was in fact the default -- a few hundred KB against frames of 1.8 MB (2 bpp) to 5.4 MB
// (6 bpp) at 4288x1664. Halving until the OS accepts the size gives the largest buffer the platform
// allows, on every platform, which is what "Maximum" was always meant to mean.
//
// But "the largest the platform allows" turned out to be the wrong meaning, and measurably so: on
// the Vision Pro, "Maximum" through this function produced NO picture at all at 4288x1664, while an
// explicit 7,000,000 worked (2026-09-22). visionOS evidently accepts a far larger UDP buffer than
// the ~7.4 MB a stock BSD kernel would, and a buffer that size does not help a real-time stream --
// it turns drops into an ever-growing backlog of stale packets, and under load it can exhaust the
// kernel's shared network-buffer pool, which then costs packets everywhere. The same setting also
// applies to the TCP control connection. So "Maximum" is capped: large enough to hold a burst of
// several frames at the full resolution (1.8 MB per frame at 2 bpp), not "as large as allowed".
// Custom values are taken as given and only stepped down if the OS rejects them.
const MAXIMUM_BUFFER_CAP: usize = 8 * 1024 * 1024;

fn set_largest_accepted(
    requested: u32,
    mut set: impl FnMut(usize) -> std::io::Result<()>,
) -> Option<usize> {
    const FLOOR: usize = 64 * 1024;
    let mut size = if requested == u32::MAX {
        MAXIMUM_BUFFER_CAP
    } else {
        requested as usize
    };
    loop {
        if set(size).is_ok() {
            return Some(size);
        }
        if size <= FLOOR {
            return None;
        }
        // Steps of a quarter rather than halving, so a limit just below the request lands close
        // to it: from 8 MB a ~7.4 MB limit yields 6 MB, where halving would have given 4 MB.
        size = usize::max(FLOOR, size - size / 4);
    }
}

fn set_socket_buffers(
    socket: &socket2::Socket,
    send_buffer_bytes: SocketBufferSize,
    recv_buffer_bytes: SocketBufferSize,
) -> Result<()> {
    info!(
        "Initial socket buffer size: send: {}B, recv: {}B",
        socket.send_buffer_size()?,
        socket.recv_buffer_size()?
    );

    {
        let maybe_size = match send_buffer_bytes {
            SocketBufferSize::Default => None,
            SocketBufferSize::Maximum => Some(u32::MAX),
            SocketBufferSize::Custom(size) => Some(size),
        };

        if let Some(size) = maybe_size {
            match set_largest_accepted(size, |s| socket.set_send_buffer_size(s)) {
                Some(accepted) => info!(
                    "Set socket send buffer succeeded: requested {size}B, accepted {accepted}B, now {}B",
                    socket.send_buffer_size()?
                ),
                None => info!("Error setting socket send buffer: no size down to 64KB accepted"),
            }
        }
    }

    {
        let maybe_size = match recv_buffer_bytes {
            SocketBufferSize::Default => None,
            SocketBufferSize::Maximum => Some(u32::MAX),
            SocketBufferSize::Custom(size) => Some(size),
        };

        if let Some(size) = maybe_size {
            match set_largest_accepted(size, |s| socket.set_recv_buffer_size(s)) {
                Some(accepted) => info!(
                    "Set socket recv buffer succeeded: requested {size}B, accepted {accepted}B, now {}B",
                    socket.recv_buffer_size()?
                ),
                None => info!("Error setting socket recv buffer: no size down to 64KB accepted"),
            }
        }
    }

    Ok(())
}

fn set_dscp(socket: &Socket, dscp: Option<DscpTos>) {
    // https://en.wikipedia.org/wiki/Differentiated_services
    if let Some(dscp) = dscp {
        let tos = match dscp {
            DscpTos::BestEffort => 0,
            DscpTos::ClassSelector(precedence) => precedence << 3,
            DscpTos::AssuredForwarding {
                class,
                drop_probability,
            } => (class << 3) | drop_probability as u8,
            DscpTos::ExpeditedForwarding => 0b101110,
        };

        socket.set_tos((tos << 2) as u32).ok();
    }
}
