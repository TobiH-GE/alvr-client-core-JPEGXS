// Note: for StreamSocket, the client uses a server socket, the server uses a client socket.
// This is because of certificate management. The server needs to trust a client and its certificate
//
// StreamSender and StreamReceiver endpoints allow for convenient conversion of the header to/from
// bytes while still handling the additional byte buffer with zero copies and extra allocations.

// Performance analysis:
// We want to minimize the transmission time for various sizes of packets.
// The current code locks the write socket *per shard* and not *per packet*. This leds to the best
// performance outcome given that the possible packets can be either very small (one shard) or very
// large (hundreds/thousands of shards, for video). if we don't allow interleaving shards, a very
// small packet will need to wait a long time before getting received if there was an ongoing
// transmission of a big packet before. If we allow interleaving shards, small packets can be
// transmitted quicker, with only minimal latency increase for the ongoing transmission of the big
// packet.
// Note: We can't clone the underlying socket for each StreamSender and the mutex around the socket
// cannot be removed. This is because we need to make sure at least shards are written whole.

use crate::backend::{tcp, udp, SocketReader, SocketWriter};
use alvr_common::{
    anyhow::Result, debug, parking_lot::Mutex, AnyhowToCon, ConResult, HandleTryAgain, ToCon,
};
use alvr_session::{DscpTos, SocketBufferSize, SocketProtocol};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    cmp::Ordering,
    collections::HashMap,
    marker::PhantomData,
    mem,
    net::{IpAddr, TcpListener, UdpSocket},
    sync::{mpsc, Arc},
    time::Duration,
};

const SHARD_PREFIX_SIZE: usize = mem::size_of::<u32>() // packet length - field itself (4 bytes)
    + mem::size_of::<u16>() // stream ID
    + mem::size_of::<u32>() // packet index
    + mem::size_of::<u32>() // shards count
    + mem::size_of::<u32>(); // shards index

/// Memory buffer that contains a hidden prefix
#[derive(Default)]
pub struct Buffer<H = ()> {
    inner: Vec<u8>,
    hidden_offset: usize, // this corresponds to prefix + header
    length: usize,
    _phantom: PhantomData<H>,
}

impl<H> Buffer<H> {
    /// Length of payload (without prefix)
    #[must_use]
    pub fn len(&self) -> usize {
        self.length
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the whole payload of the buffer
    pub fn get(&self) -> &[u8] {
        &self.inner[self.hidden_offset..][..self.length]
    }

    /// If the range is outside the valid range, new space will be allocated
    /// NB: the offset parameter is applied on top of the internal offset of the buffer
    pub fn get_range_mut(&mut self, offset: usize, size: usize) -> &mut [u8] {
        let required_size = self.hidden_offset + offset + size;
        if required_size > self.inner.len() {
            self.inner.resize(required_size, 0);
        }

        self.length = self.length.max(offset + size);

        &mut self.inner[self.hidden_offset + offset..][..size]
    }

    /// If length > current length, allocate more space
    pub fn set_len(&mut self, length: usize) {
        self.inner.resize(self.hidden_offset + length, 0);
        self.length = length;
    }
}

#[derive(Clone)]
pub struct StreamSender<H> {
    inner: Arc<Mutex<Box<dyn SocketWriter>>>,
    stream_id: u16,
    max_packet_size: usize,
    // if the packet index overflows the worst that happens is a false positive packet loss
    next_packet_index: u32,
    used_buffers: Vec<Vec<u8>>,
    _phantom: PhantomData<H>,
}

impl<H> StreamSender<H> {
    /// Shard and send a buffer with zero copies and zero allocations.
    /// The prefix of each shard is written over the previously sent shard to avoid reallocations.
    pub fn send(&mut self, mut buffer: Buffer<H>) -> Result<()> {
        let max_shard_data_size = self.max_packet_size - SHARD_PREFIX_SIZE;
        let actual_buffer_size = buffer.hidden_offset + buffer.length;
        let data_size = actual_buffer_size - SHARD_PREFIX_SIZE;
        let shards_count = (data_size as f32 / max_shard_data_size as f32).ceil() as usize;

        for idx in 0..shards_count {
            // this overlaps with the previous shard, this is intended behavior and allows to
            // reduce allocations
            let packet_start_position = idx * max_shard_data_size;
            let sub_buffer = &mut buffer.inner[packet_start_position..];

            // NB: true shard length (account for last shard that is smaller)
            let packet_length = usize::min(
                self.max_packet_size,
                actual_buffer_size - packet_start_position,
            );

            // todo: switch to little endian
            // todo: do not remove sizeof<u32> for packet length
            sub_buffer[0..4]
                .copy_from_slice(&((packet_length - mem::size_of::<u32>()) as u32).to_be_bytes());
            sub_buffer[4..6].copy_from_slice(&self.stream_id.to_be_bytes());
            sub_buffer[6..10].copy_from_slice(&self.next_packet_index.to_be_bytes());
            sub_buffer[10..14].copy_from_slice(&(shards_count as u32).to_be_bytes());
            sub_buffer[14..18].copy_from_slice(&(idx as u32).to_be_bytes());

            self.inner.lock().send(&sub_buffer[..packet_length])?;
        }

        self.next_packet_index += 1;

        self.used_buffers.push(buffer.inner);

        Ok(())
    }
}

impl<H: Serialize> StreamSender<H> {
    pub fn get_buffer(&mut self, header: &H) -> Result<Buffer<H>> {
        let mut buffer = self.used_buffers.pop().unwrap_or_default();

        let header_size = bincode::serialized_size(header)? as usize;
        let hidden_offset = SHARD_PREFIX_SIZE + header_size;

        if buffer.len() < hidden_offset {
            buffer.resize(hidden_offset, 0);
        }

        bincode::serialize_into(&mut buffer[SHARD_PREFIX_SIZE..hidden_offset], header)?;

        Ok(Buffer {
            inner: buffer,
            hidden_offset,
            length: 0,
            _phantom: PhantomData,
        })
    }

    pub fn send_header(&mut self, header: &H) -> Result<()> {
        let buffer = self.get_buffer(header)?;
        self.send(buffer)
    }
}

pub struct ReceiverData<H> {
    buffer: Option<Vec<u8>>,
    size: usize, // counting the prefix
    used_buffer_queue: mpsc::Sender<Vec<u8>>,
    had_packet_loss: bool,
    // How many packets were skipped right before this one -- the count behind had_packet_loss.
    lost_before: u32,
    _phantom: PhantomData<H>,
}

impl<H> ReceiverData<H> {
    pub fn had_packet_loss(&self) -> bool {
        self.had_packet_loss
    }

    pub fn lost_before(&self) -> u32 {
        self.lost_before
    }
}

impl<H: DeserializeOwned> ReceiverData<H> {
    pub fn get(&self) -> Result<(H, &[u8])> {
        let mut data: &[u8] = &self.buffer.as_ref().unwrap()[SHARD_PREFIX_SIZE..self.size];
        // This will partially consume the slice, leaving only the actual payload
        let header = bincode::deserialize_from(&mut data)?;

        Ok((header, data))
    }
    pub fn get_header(&self) -> Result<H> {
        Ok(self.get()?.0)
    }
}

impl<H> Drop for ReceiverData<H> {
    fn drop(&mut self) {
        self.used_buffer_queue
            .send(self.buffer.take().unwrap())
            .ok();
    }
}

struct ReconstructedPacket {
    index: u32,
    buffer: Vec<u8>,
    size: usize, // contains prefix
}

pub struct StreamReceiver<H> {
    packet_receiver: mpsc::Receiver<ReconstructedPacket>,
    used_buffer_queue: mpsc::Sender<Vec<u8>>,
    last_packet_index: Option<u32>,
    _phantom: PhantomData<H>,
}

fn wrapping_cmp(lhs: u32, rhs: u32) -> Ordering {
    let diff = lhs.wrapping_sub(rhs);
    if diff == 0 {
        Ordering::Equal
    } else if diff < u32::MAX / 2 {
        Ordering::Greater
    } else {
        // if diff > u32::MAX / 2, it means the sub operation wrapped
        Ordering::Less
    }
}

/// Get next packet reconstructing from shards.
/// Returns true if a packet has been recontructed and copied into the buffer.
impl<H: DeserializeOwned + Serialize> StreamReceiver<H> {
    pub fn recv(&mut self, timeout: Duration) -> ConResult<ReceiverData<H>> {
        let packet = self
            .packet_receiver
            .recv_timeout(timeout)
            .handle_try_again()?;

        let mut had_packet_loss = false;
        let mut lost_before: u32 = 0;

        if let Some(last_idx) = self.last_packet_index {
            // Use wrapping arithmetics
            match wrapping_cmp(packet.index, last_idx.wrapping_add(1)) {
                Ordering::Equal => (),
                Ordering::Greater => {
                    // Skipped some indices
                    had_packet_loss = true;
                    lost_before = packet.index.wrapping_sub(last_idx).wrapping_sub(1);
                }
                Ordering::Less => {
                    // Old packet, discard
                    self.used_buffer_queue.send(packet.buffer).to_con()?;
                    return alvr_common::try_again();
                }
            }
        }
        self.last_packet_index = Some(packet.index);

        Ok(ReceiverData {
            buffer: Some(packet.buffer),
            size: packet.size,
            used_buffer_queue: self.used_buffer_queue.clone(),
            had_packet_loss,
            lost_before,
            _phantom: PhantomData,
        })
    }
}

pub enum StreamSocketBuilder {
    Tcp(TcpListener),
    Udp(UdpSocket),
}

impl StreamSocketBuilder {
    pub fn listen_for_server(
        timeout: Duration,
        port: u16,
        stream_socket_config: SocketProtocol,
        stream_tos_config: Option<DscpTos>,
        send_buffer_bytes: SocketBufferSize,
        recv_buffer_bytes: SocketBufferSize,
    ) -> Result<Self> {
        Ok(match stream_socket_config {
            SocketProtocol::Udp => StreamSocketBuilder::Udp(udp::bind(
                port,
                stream_tos_config,
                send_buffer_bytes,
                recv_buffer_bytes,
            )?),
            SocketProtocol::Tcp => StreamSocketBuilder::Tcp(tcp::bind(
                timeout,
                port,
                stream_tos_config,
                send_buffer_bytes,
                recv_buffer_bytes,
            )?),
        })
    }

    pub fn accept_from_server(
        self,
        server_ip: IpAddr,
        port: u16,
        max_packet_size: usize,
        timeout: Duration,
    ) -> ConResult<StreamSocket> {
        let (send_socket, receive_socket): (Box<dyn SocketWriter>, Box<dyn SocketReader>) =
            match self {
                StreamSocketBuilder::Udp(socket) => {
                    let (send_socket, receive_socket) =
                        udp::connect(&socket, server_ip, port, timeout).to_con()?;

                    (Box::new(send_socket), Box::new(receive_socket))
                }
                StreamSocketBuilder::Tcp(listener) => {
                    let (send_socket, receive_socket) =
                        tcp::accept_from_server(&listener, Some(server_ip), timeout)?;

                    (Box::new(send_socket), Box::new(receive_socket))
                }
            };

        Ok(StreamSocket {
            // +4 is a workaround to retain compatibilty with old protocol
            // todo: remove +4
            max_packet_size: max_packet_size + 4,
            send_socket: Arc::new(Mutex::new(send_socket)),
            receive_socket,
            shard_recv_state: None,
            datagram_scratch: Vec::new(),
            stream_recv_components: HashMap::new(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn connect_to_client(
        timeout: Duration,
        client_ip: IpAddr,
        port: u16,
        protocol: SocketProtocol,
        dscp: Option<DscpTos>,
        send_buffer_bytes: SocketBufferSize,
        recv_buffer_bytes: SocketBufferSize,
        max_packet_size: usize,
    ) -> ConResult<StreamSocket> {
        let (send_socket, receive_socket): (Box<dyn SocketWriter>, Box<dyn SocketReader>) =
            match protocol {
                SocketProtocol::Udp => {
                    let socket =
                        udp::bind(port, dscp, send_buffer_bytes, recv_buffer_bytes).to_con()?;
                    let (send_socket, receive_socket) =
                        udp::connect(&socket, client_ip, port, timeout).to_con()?;

                    (Box::new(send_socket), Box::new(receive_socket))
                }
                SocketProtocol::Tcp => {
                    let (send_socket, receive_socket) = tcp::connect_to_client(
                        timeout,
                        &[client_ip],
                        port,
                        send_buffer_bytes,
                        recv_buffer_bytes,
                    )?;

                    (Box::new(send_socket), Box::new(receive_socket))
                }
            };

        Ok(StreamSocket {
            // +4 is a workaround to retain compatibilty with old protocol
            // todo: remove +4
            max_packet_size: max_packet_size + 4,
            send_socket: Arc::new(Mutex::new(send_socket)),
            receive_socket,
            shard_recv_state: None,
            datagram_scratch: Vec::new(),
            stream_recv_components: HashMap::new(),
        })
    }
}

struct RecvState {
    shard_length: usize, // contains prefix length itself
    stream_id: u16,
    packet_index: u32,
    shards_count: usize,
    shard_index: usize,
    packet_cursor: usize, // counts also the prefix bytes
    overwritten_data_backup: Option<[u8; SHARD_PREFIX_SIZE]>,
    should_discard: bool,
}

// Which shards of a packet have arrived: one bit each (2026-09-25). This was a HashSet<usize>,
// which ran SipHash on every shard -- ~200,000 of them a second at 3 bpp, twice that at 6 bpp, all
// on the receive thread -- and allocated a hash table per packet.
#[derive(Default)]
struct ShardMask {
    bits: Vec<u64>,
    count: usize,
}

impl ShardMask {
    fn with_shards(shards_count: usize) -> Self {
        Self {
            bits: vec![0; shards_count.div_ceil(64)],
            count: 0,
        }
    }

    // A repeated shard is counted once.
    fn insert(&mut self, shard_index: usize) {
        let (word, bit) = (shard_index / 64, shard_index % 64);
        if word >= self.bits.len() {
            self.bits.resize(word + 1, 0);
        }
        let mask = 1u64 << bit;
        if self.bits[word] & mask == 0 {
            self.bits[word] |= mask;
            self.count += 1;
        }
    }

    fn len(&self) -> usize {
        self.count
    }
}

struct InProgressPacket {
    buffer: Vec<u8>,
    buffer_length: usize,
    received_shard_indices: ShardMask,
}

struct StreamRecvComponents {
    used_buffer_sender: mpsc::Sender<Vec<u8>>,
    used_buffer_receiver: mpsc::Receiver<Vec<u8>>,
    packet_queue: mpsc::Sender<ReconstructedPacket>,
    in_progress_packets: HashMap<u32, InProgressPacket>,
    discarded_shards_sink: InProgressPacket,
}

// Note: used buffers don't *have* to be split by stream ID, but doing so improves memory usage
// todo: impose cap on number of created buffers to avoid OOM crashes
// Packets discarded inside ALVR for lack of a free receive buffer, since process start.
pub static DISCARDED_PACKETS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub struct StreamSocket {
    max_packet_size: usize,
    send_socket: Arc<Mutex<Box<dyn SocketWriter>>>,
    receive_socket: Box<dyn SocketReader>,
    shard_recv_state: Option<RecvState>,
    stream_recv_components: HashMap<u16, StreamRecvComponents>,
    // UDP only: the whole shard of the current recv(), read in one call. See recv().
    datagram_scratch: Vec<u8>,
}

impl StreamSocket {
    pub fn request_stream<T>(&self, stream_id: u16) -> StreamSender<T> {
        StreamSender {
            inner: Arc::clone(&self.send_socket),
            stream_id,
            max_packet_size: self.max_packet_size,
            next_packet_index: 0,
            used_buffers: vec![],
            _phantom: PhantomData,
        }
    }

    // max_concurrent_buffers: number of buffers allocated by this call which will be reused to
    // receive packets for this stream ID. If packets are not read fast enough, the shards received
    // for this particular stream will be discarded
    pub fn subscribe_to_stream<T>(
        &mut self,
        stream_id: u16,
        max_concurrent_buffers: usize,
    ) -> StreamReceiver<T> {
        let (packet_sender, packet_receiver) = mpsc::channel();
        let (used_buffer_sender, used_buffer_receiver) = mpsc::channel();

        for _ in 0..max_concurrent_buffers {
            used_buffer_sender.send(vec![]).ok();
        }

        self.stream_recv_components.insert(
            stream_id,
            StreamRecvComponents {
                used_buffer_sender: used_buffer_sender.clone(),
                used_buffer_receiver,
                packet_queue: packet_sender,
                in_progress_packets: HashMap::new(),
                discarded_shards_sink: InProgressPacket {
                    buffer: vec![],
                    buffer_length: 0,
                    received_shard_indices: ShardMask::default(),
                },
            },
        );

        StreamReceiver {
            packet_receiver,
            used_buffer_queue: used_buffer_sender,
            _phantom: PhantomData,
            last_packet_index: None,
        }
    }

    // Diagnostics: (bytes queued in the kernel receive buffer, that buffer's size).
    pub fn receive_queue(&self) -> (Option<usize>, Option<usize>) {
        (self.receive_socket.queued_bytes(), self.receive_socket.buffer_bytes())
    }

    pub fn recv(&mut self) -> ConResult {
        // UDP: one syscall per shard. The stream path below peeks at the 18-byte prefix first and
        // then reads the datagram a second time straight into its place in the packet buffer,
        // which saves a 1.4 KB copy at the price of a whole extra syscall per shard. At the full
        // 4288x1664 that is ~113,000 shards a second, and with the decoder busy the headset's
        // receive thread fell behind: the kernel buffer ran 100 % full and 5-11 % of the packets
        // were dropped (2026-09-22). The copy costs nanoseconds, the syscall microseconds. A UDP
        // datagram also always arrives whole, so none of the partial-read state is needed.
        let datagram = self.receive_socket.is_datagram();

        let shard_recv_state_mut = if let Some(state) = &mut self.shard_recv_state {
            state
        } else {
            let mut bytes = [0; SHARD_PREFIX_SIZE];
            if datagram {
                if self.datagram_scratch.len() < self.max_packet_size {
                    self.datagram_scratch.resize(self.max_packet_size, 0);
                }
                let count = self.receive_socket.recv(&mut self.datagram_scratch)?;
                if count < SHARD_PREFIX_SIZE {
                    return alvr_common::try_again();
                }
                bytes.copy_from_slice(&self.datagram_scratch[..SHARD_PREFIX_SIZE]);
                let declared = mem::size_of::<u32>()
                    + u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
                if declared != count {
                    // Truncated or not one of ours: drop it rather than misplace its bytes.
                    return alvr_common::try_again();
                }
            } else {
                let count = self.receive_socket.peek(&mut bytes)?;
                if count < SHARD_PREFIX_SIZE {
                    return alvr_common::try_again();
                }
            }

            // todo: switch to little endian
            // todo: do not remove sizeof<u32> for packet length
            let shard_length = mem::size_of::<u32>()
                + u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
            let stream_id = u16::from_be_bytes(bytes[4..6].try_into().unwrap());
            let packet_index = u32::from_be_bytes(bytes[6..10].try_into().unwrap());
            let shards_count = u32::from_be_bytes(bytes[10..14].try_into().unwrap()) as usize;
            let shard_index = u32::from_be_bytes(bytes[14..18].try_into().unwrap()) as usize;

            self.shard_recv_state.insert(RecvState {
                shard_length,
                stream_id,
                packet_index,
                shards_count,
                shard_index,
                packet_cursor: 0,
                overwritten_data_backup: None,
                should_discard: false,
            })
        };

        let Some(components) = self
            .stream_recv_components
            .get_mut(&shard_recv_state_mut.stream_id)
        else {
            debug!(
                "Received packet from stream {} before subscribing!",
                shard_recv_state_mut.stream_id
            );
            if datagram {
                // Already consumed from the socket, so there is nothing to retry: drop it.
                self.shard_recv_state = None;
            }
            return alvr_common::try_again();
        };

        let in_progress_packet = if shard_recv_state_mut.should_discard {
            &mut components.discarded_shards_sink
        } else if let Some(packet) = components
            .in_progress_packets
            .get_mut(&shard_recv_state_mut.packet_index)
        {
            packet
        } else if let Some(buffer) = components.used_buffer_receiver.try_recv().ok().or_else(|| {
            // By default, try to dequeue a used buffer. In case none were found, recycle one of the
            // in progress packets, chances are these buffers are "dead" because one of their shards
            // has been dropped by the network. The OLDEST one: taking whichever the hash map
            // yielded first could pick a packet that was still arriving.
            let idx = components
                .in_progress_packets
                .keys()
                .copied()
                .min_by(|a, b| wrapping_cmp(*a, *b))?;
            Some(components.in_progress_packets.remove(&idx).unwrap().buffer)
        }) {
            // NB: Can't use entry pattern because we want to allow bailing out on the line above
            components.in_progress_packets.insert(
                shard_recv_state_mut.packet_index,
                InProgressPacket {
                    buffer,
                    buffer_length: 0,
                    received_shard_indices: ShardMask::with_shards(
                        shard_recv_state_mut.shards_count,
                    ),
                },
            );
            components
                .in_progress_packets
                .get_mut(&shard_recv_state_mut.packet_index)
                .unwrap()
        } else {
            // This branch may be hit in case the thread related to the stream hangs for some reason
            shard_recv_state_mut.should_discard = true;
            // A whole packet thrown away inside ALVR because no receive buffer was free -- the
            // one kind of loss that happens after the socket, not before it.
            DISCARDED_PACKETS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            shard_recv_state_mut.packet_cursor = 0; // reset cursor from old shards
                                                    // always write at the start of the packet so the buffer doesn't grow much
            shard_recv_state_mut.shard_index = 0;

            &mut components.discarded_shards_sink
        };

        let max_shard_data_size = self.max_packet_size - SHARD_PREFIX_SIZE;
        // Note: there is no prefix offset, since we want to write the prefix too.
        let packet_start_index = shard_recv_state_mut.shard_index * max_shard_data_size;

        // Prepare buffer to accomodate receiving shard
        {
            // Note: this contains the prefix offset
            in_progress_packet.buffer_length = usize::max(
                in_progress_packet.buffer_length,
                packet_start_index + shard_recv_state_mut.shard_length,
            );

            if in_progress_packet.buffer.len() < in_progress_packet.buffer_length {
                in_progress_packet
                    .buffer
                    .resize(in_progress_packet.buffer_length, 0);
            }
        }

        let sub_buffer = &mut in_progress_packet.buffer[packet_start_index..];

        // Read shard into the single contiguous buffer
        if datagram {
            // Already read. The prefix is not copied: only the data is ever read back out
            // (ReceiverData::get starts at SHARD_PREFIX_SIZE), so unlike the stream path there is
            // nothing to overwrite and nothing to restore.
            let len = shard_recv_state_mut.shard_length;
            sub_buffer[SHARD_PREFIX_SIZE..len]
                .copy_from_slice(&self.datagram_scratch[SHARD_PREFIX_SIZE..len]);
        } else {
            // Backup the small section of bytes that will be overwritten by reading from socket.
            if shard_recv_state_mut.overwritten_data_backup.is_none() {
                shard_recv_state_mut.overwritten_data_backup =
                    Some(sub_buffer[..SHARD_PREFIX_SIZE].try_into().unwrap())
            }

            // This loop may bail out at any time if a timeout is reached. This is correctly handled by
            // the previous code.
            while shard_recv_state_mut.packet_cursor < shard_recv_state_mut.shard_length {
                let size = self.receive_socket.recv(
                    &mut sub_buffer
                        [shard_recv_state_mut.packet_cursor..shard_recv_state_mut.shard_length],
                )?;
                shard_recv_state_mut.packet_cursor += size;
            }

            // Restore backed up bytes
            // Safety: overwritten_data_backup is always set just before receiving the packet
            sub_buffer[..SHARD_PREFIX_SIZE]
                .copy_from_slice(&shard_recv_state_mut.overwritten_data_backup.take().unwrap());
        }

        // An index past the declared count is not ours to count: it would complete the packet
        // with a shard missing.
        if !shard_recv_state_mut.should_discard
            && shard_recv_state_mut.shard_index < shard_recv_state_mut.shards_count
        {
            in_progress_packet
                .received_shard_indices
                .insert(shard_recv_state_mut.shard_index);
        }

        // Check if packet is complete and send
        if in_progress_packet.received_shard_indices.len() == shard_recv_state_mut.shards_count {
            let size = in_progress_packet.buffer_length;
            components
                .packet_queue
                .send(ReconstructedPacket {
                    index: shard_recv_state_mut.packet_index,
                    buffer: components
                        .in_progress_packets
                        .remove(&shard_recv_state_mut.packet_index)
                        .unwrap()
                        .buffer,
                    size,
                })
                .ok();

            // Keep only shards with later packet index (using wrapping logic), recycling the
            // buffers of the older, incomplete ones -- StreamReceiver would discard them anyway once
            // this newer packet has been handed on. One pass instead of a search per packet.
            let completed = shard_recv_state_mut.packet_index;
            let used_buffer_sender = &components.used_buffer_sender;
            components.in_progress_packets.retain(|idx, packet| {
                if wrapping_cmp(*idx, completed) == Ordering::Less {
                    used_buffer_sender.send(mem::take(&mut packet.buffer)).ok();
                    false
                } else {
                    true
                }
            });
        }

        // Mark current shard as read and allow for a new shard to be read
        self.shard_recv_state = None;

        Ok(())
    }
}

#[cfg(test)]
mod shard_mask_tests {
    use super::ShardMask;

    #[test]
    fn counts_each_shard_once_across_word_boundaries() {
        let mut mask = ShardMask::with_shards(130);
        for idx in [0, 63, 64, 127, 128, 129, 64, 0] {
            mask.insert(idx);
        }
        assert_eq!(mask.len(), 6);
        for idx in 0..130 {
            mask.insert(idx);
        }
        assert_eq!(mask.len(), 130);
    }

    #[test]
    fn default_mask_grows_on_demand() {
        let mut mask = ShardMask::default();
        mask.insert(200);
        mask.insert(200);
        assert_eq!(mask.len(), 1);
    }
}
