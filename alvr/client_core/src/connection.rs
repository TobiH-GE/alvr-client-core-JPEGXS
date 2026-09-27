#![allow(clippy::if_same_then_else)]

use crate::{
    logging_backend::{LogMirrorData, LOG_CHANNEL_SENDER},
    sockets::AnnouncerSocket,
    statistics::StatisticsManager,
    storage::Config,
    ClientCapabilities, ClientCoreEvent,
};
use alvr_audio::AudioDevice;
use alvr_common::{
    dbg_connection, debug, error, info,
    parking_lot::{Condvar, Mutex, RwLock},
    wait_rwlock, warn, AnyhowToCon, ConResult, ConnectionError, ConnectionState, LifecycleState,
    Pose, RelaxedAtomic, ViewParams, ALVR_VERSION,
};
use alvr_packets::{
    ClientConnectionResult, ClientControlPacket, ClientStatistics, Haptics, RealTimeConfig,
    ServerControlPacket, StreamConfigPacket, Tracking, VideoPacketHeader,
    VideoStreamingCapabilities, AUDIO, HAPTICS, STATISTICS, TRACKING, VIDEO,
};
use alvr_session::{settings_schema::Switch, SocketProtocol};
use alvr_sockets::{
    ControlSocketSender, PeerType, ProtoControlSocket, StreamSender, StreamSocketBuilder,
    KEEPALIVE_INTERVAL, KEEPALIVE_TIMEOUT,
};
use std::{
    collections::VecDeque,
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "android")]
use crate::audio;
#[cfg(not(target_os = "android"))]
use alvr_audio as audio;

const INITIAL_MESSAGE: &str = concat!(
    "Searching for streamer...\n",
    "Open ALVR on your PC then click \"Trust\"\n",
    "next to the device entry",
);
const NETWORK_UNREACHABLE_MESSAGE: &str = "Cannot connect to the streamer.\nNetwork error.";
const SUCCESS_CONNECT_MESSAGE: &str = "Successful connection!\nPlease wait...";
const LOCAL_TRY_MESSAGE: &str = "Trying to connect to localhost...";
// const INCOMPATIBLE_VERSIONS_MESSAGE: &str = concat!(
//     "Streamer and client have\n",
//     "incompatible types.\n",
//     "Please update either the app\n",
//     "on the PC or on the headset",
// );
const STREAM_STARTING_MESSAGE: &str = "The stream will begin soon\nPlease wait...";
const SERVER_RESTART_MESSAGE: &str = "The streamer is restarting\nPlease wait...";
const SERVER_DISCONNECTED_MESSAGE: &str = "The streamer has disconnected.";
const CONNECTION_TIMEOUT_MESSAGE: &str = "Connection timeout.";

const DISCOVERY_RETRY_PAUSE: Duration = Duration::from_millis(500);
const RETRY_CONNECT_MIN_INTERVAL: Duration = Duration::from_secs(1);
const CONNECTION_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const HANDSHAKE_ACTION_TIMEOUT: Duration = Duration::from_secs(2);
const STREAMING_RECV_TIMEOUT: Duration = Duration::from_millis(500);

const MAX_UNREAD_PACKETS: usize = 10; // Applies per stream
// The video stream needs far more than the others, and subscribe_to_stream's own doc comment says
// why: these are the concurrent receive buffers, and "if packets are not read fast enough, the
// shards received for this particular stream will be discarded". Ten was sized for one packet per
// frame. JPEG XS slice packetization makes a frame 66 packets, so ten buffers is a SIXTH of a
// frame -- about 1.7 ms at 90 fps -- and any hiccup on the receive thread silently drops slices.
// The client sees them as slices that never arrive and fills them from the previous frame, i.e.
// stale 16-line strips on motion: measured 2026-09-20 at 2.1 % of slices and 28 % of frames, with
// zero of them merely late (waiting 4 ms for stragglers rescued exactly none). The buffers are
// reused, so this costs memory once, not per packet.
// Raised again from 256 (2026-09-22): at the full 4288x1664 a frame is 210 slice packets, so 256
// buffers covered barely 1.2 frames, and a receive-thread hiccup of one frame period (18,900
// packets/s) was enough to start discarding. 1024 is about 4.9 frames. The buffers grow to the
// largest packet they have held -- 8.5 KB per slice at 2 bpp, 25 KB at 6 bpp -- so the worst case
// is ~26 MB, allocated once and reused.
const MAX_UNREAD_VIDEO_PACKETS: usize = 1024;

// Video receive diagnostics, read and reset by alvr_video_net_stats() (c_api.rs). The client's
// own log goes nowhere readable on the headset, so these reach the app's log through the C API.
pub static VIDEO_NET_QUEUE_MAX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static VIDEO_NET_RECV_BUFFER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static VIDEO_NET_LOST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static VIDEO_NET_RECEIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
// Datagrams the stream receive thread read (every stream, not only video), and the CPU time that
// thread has used in total. Together they say what one datagram costs to receive -- the number
// that decides whether larger, IP-fragmented datagrams are worth it (2026-09-25).
pub static STREAM_NET_DATAGRAMS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static STREAM_NET_RECV_CPU_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// CPU time used by the calling thread so far, in nanoseconds; 0 where unavailable.
fn current_thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0 {
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    } else {
        0
    }
}

// (timestamp, payload, packet_index, packets_per_frame) -- see VideoPacketHeader. (0, 1) is a whole
// frame; JPEG XS column pipelining delivers one column per call, all sharing a timestamp.
pub type DecoderCallback = dyn FnMut(Duration, &[u8], u16, u16, u8) -> bool + Send;

#[derive(Default)]
pub struct ConnectionContext {
    pub state: RwLock<ConnectionState>,
    pub disconnected_notif: Condvar,
    pub control_sender: Mutex<Option<ControlSocketSender<ClientControlPacket>>>,
    pub tracking_sender: Mutex<Option<StreamSender<Tracking>>>,
    pub statistics_sender: Mutex<Option<StreamSender<ClientStatistics>>>,
    pub statistics_manager: Mutex<Option<StatisticsManager>>,
    pub decoder_callback: Mutex<Option<Box<DecoderCallback>>>,
    pub head_pose_queue: RwLock<VecDeque<(Duration, Pose)>>,
    pub last_good_head_pose: RwLock<Pose>,
    pub view_params: RwLock<[ViewParams; 2]>,
    pub uses_multimodal_protocol: RelaxedAtomic,
    pub velocities_multiplier: RwLock<f32>,
    pub max_prediction: RwLock<Duration>,
}

fn set_hud_message(event_queue: &Mutex<VecDeque<ClientCoreEvent>>, message: &str) {
    let message = format!(
        "ALVR v{}\nhostname: {}\nIP: {}\n\n{message}",
        *ALVR_VERSION,
        Config::load().hostname,
        alvr_system_info::local_ip(),
    );

    event_queue
        .lock()
        .push_back(ClientCoreEvent::UpdateHudMessage(message));
}

fn is_streaming(ctx: &ConnectionContext) -> bool {
    *ctx.state.read() == ConnectionState::Streaming
}

pub fn connection_lifecycle_loop(
    capabilities: ClientCapabilities,
    ctx: Arc<ConnectionContext>,
    lifecycle_state: Arc<RwLock<LifecycleState>>,
    event_queue: Arc<Mutex<VecDeque<ClientCoreEvent>>>,
) {
    dbg_connection!("connection_lifecycle_loop: Begin");

    set_hud_message(&event_queue, INITIAL_MESSAGE);

    while *lifecycle_state.read() != LifecycleState::ShuttingDown {
        if *lifecycle_state.read() == LifecycleState::Resumed {
            if let Err(e) = connection_pipeline(
                capabilities.clone(),
                Arc::clone(&ctx),
                Arc::clone(&lifecycle_state),
                Arc::clone(&event_queue),
            ) {
                let message = format!("Connection error:\n{e}\nCheck the PC for more details");
                set_hud_message(&event_queue, &message);
                error!("Connection error: {e}");
            }
        } else {
            debug!("Skip try connection because the device is sleeping");
        }

        *ctx.state.write() = ConnectionState::Disconnected;
        ctx.disconnected_notif.notify_all();

        thread::sleep(CONNECTION_RETRY_INTERVAL);
    }

    dbg_connection!("connection_lifecycle_loop: End");
}

fn connection_pipeline(
    capabilities: ClientCapabilities,
    ctx: Arc<ConnectionContext>,
    lifecycle_state: Arc<RwLock<LifecycleState>>,
    event_queue: Arc<Mutex<VecDeque<ClientCoreEvent>>>,
) -> ConResult {
    dbg_connection!("connection_pipeline: Begin");

    let (mut proto_control_socket, server_ip) = {
        let config = Config::load();
        let mut announcer_socket = AnnouncerSocket::new(&config.hostname).to_con()?;
        let listener_socket =
            alvr_sockets::get_server_listener(HANDSHAKE_ACTION_TIMEOUT).to_con()?;

        loop {
            if *lifecycle_state.write() != LifecycleState::Resumed {
                return Ok(());
            }

            let mut is_broadcast_ok = false;
            if let Err(e) = announcer_socket.announce_broadcast() {
                debug!("Couldn't announce to localhost, retrying on local... {e:}");

                set_hud_message(&event_queue, LOCAL_TRY_MESSAGE);
            } else {
                is_broadcast_ok = true;
            }

            if let Ok(pair) = ProtoControlSocket::connect_to(
                DISCOVERY_RETRY_PAUSE,
                PeerType::Server(&listener_socket),
            ) {
                set_hud_message(&event_queue, SUCCESS_CONNECT_MESSAGE);
                break pair;
            }

            if !is_broadcast_ok {
                warn!("Couldn't announce to network or connect to localhost.");
                set_hud_message(&event_queue, NETWORK_UNREACHABLE_MESSAGE);

                thread::sleep(RETRY_CONNECT_MIN_INTERVAL);

                set_hud_message(&event_queue, INITIAL_MESSAGE);
                return Ok(());
            }
        }
    };

    let mut connection_state_lock = ctx.state.write();
    let disconnect_notif = Arc::new(Condvar::new());

    *connection_state_lock = ConnectionState::Connecting;

    let microphone_sample_rate = AudioDevice::new_input(None)
        .to_con()?
        .input_sample_rate()
        .to_con()?;

    dbg_connection!("connection_pipeline: Send stream capabilities");
    proto_control_socket
        .send(&ClientConnectionResult::ConnectionAccepted {
            client_protocol_id: alvr_common::protocol_id_u64(),
            display_name: alvr_system_info::platform().to_string(),
            server_ip,
            streaming_capabilities: Some(
                alvr_packets::encode_video_streaming_capabilities(&VideoStreamingCapabilities {
                    default_view_resolution: capabilities.default_view_resolution,
                    supported_refresh_rates: capabilities.refresh_rates,
                    microphone_sample_rate,
                    supports_foveated_encoding: capabilities.foveated_encoding,
                    encoder_high_profile: capabilities.encoder_high_profile,
                    encoder_10_bits: capabilities.encoder_10_bits,
                    encoder_av1: capabilities.encoder_av1,
                    // Milestone B: alvr-visionos now has a real JPEG XS decode path
                    // (JXSTestPlayer's live-mode wiring, driven from EventHandler
                    // instead of a local test file) -- forwarded from the app's own
                    // capability declaration (EventHandler.swift's alvr_initialize
                    // call) like every other codec flag here, instead of a hardcoded
                    // constant.
                    encoder_jpegxs: capabilities.encoder_jpegxs,
                    multimodal_protocol: true,
                    prefer_10bit: capabilities.prefer_10bit,
                    prefer_full_range: capabilities.prefer_full_range,
                    preferred_encoding_gamma: capabilities.preferred_encoding_gamma,
                    prefer_hdr: capabilities.prefer_hdr,
                })
                .to_con()?,
            ),
        })
        .to_con()?;
    let config_packet =
        proto_control_socket.recv::<StreamConfigPacket>(HANDSHAKE_ACTION_TIMEOUT)?;
    dbg_connection!("connection_pipeline: stream config received");

    let stream_config = alvr_packets::decode_stream_config(&config_packet).to_con()?;

    ctx.uses_multimodal_protocol
        .set(stream_config.negotiated_config.use_multimodal_protocol);

    let streaming_start_event = ClientCoreEvent::StreamingStarted(Box::new(stream_config.clone()));

    let settings = stream_config.settings;
    let negotiated_config = stream_config.negotiated_config;

    *ctx.velocities_multiplier.write() = settings.extra.velocities_multiplier;
    *ctx.max_prediction.write() = Duration::from_millis(settings.headset.max_prediction_ms);

    *ctx.statistics_manager.lock() = Some(StatisticsManager::new(
        settings.connection.statistics_history_size,
        Duration::from_secs_f32(1.0 / negotiated_config.refresh_rate_hint),
        if let Switch::Enabled(config) = settings.headset.controllers {
            config.steamvr_pipeline_frames
        } else {
            0.0
        },
    ));

    let (mut control_sender, mut control_receiver) = proto_control_socket
        .split(STREAMING_RECV_TIMEOUT)
        .to_con()?;

    match control_receiver.recv(HANDSHAKE_ACTION_TIMEOUT) {
        Ok(ServerControlPacket::StartStream) => {
            info!("Stream starting");
            set_hud_message(&event_queue, STREAM_STARTING_MESSAGE);
        }
        Ok(ServerControlPacket::Restarting) => {
            info!("Server restarting");
            set_hud_message(&event_queue, SERVER_RESTART_MESSAGE);
            return Ok(());
        }
        Err(e) => {
            info!("Server disconnected. Cause: {e}");
            set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);
            return Ok(());
        }
        _ => {
            info!("Unexpected packet");
            set_hud_message(&event_queue, "Unexpected packet");
            return Ok(());
        }
    }

    let stream_protocol = if negotiated_config.wired {
        SocketProtocol::Tcp
    } else {
        settings.connection.stream_protocol
    };

    dbg_connection!("connection_pipeline: create StreamSocket");
    let stream_socket_builder = StreamSocketBuilder::listen_for_server(
        Duration::from_secs(1),
        settings.connection.stream_port,
        stream_protocol,
        settings.connection.dscp,
        settings.connection.client_send_buffer_bytes,
        settings.connection.client_recv_buffer_bytes,
    )
    .to_con()?;

    dbg_connection!("connection_pipeline: Send StreamReady");
    if let Err(e) = control_sender.send(&ClientControlPacket::StreamReady) {
        info!("Server disconnected. Cause: {e:?}");
        set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);
        return Ok(());
    }

    dbg_connection!("connection_pipeline: accept connection");
    let mut stream_socket = stream_socket_builder.accept_from_server(
        server_ip,
        settings.connection.stream_port,
        settings.connection.packet_size as _,
        HANDSHAKE_ACTION_TIMEOUT,
    )?;

    info!("Connected to server");

    let mut video_receiver =
        stream_socket.subscribe_to_stream::<VideoPacketHeader>(VIDEO, MAX_UNREAD_VIDEO_PACKETS);
    let mut game_audio_receiver = stream_socket.subscribe_to_stream(AUDIO, MAX_UNREAD_PACKETS);
    let tracking_sender = stream_socket.request_stream(TRACKING);
    let mut haptics_receiver =
        stream_socket.subscribe_to_stream::<Haptics>(HAPTICS, MAX_UNREAD_PACKETS);
    let statistics_sender = stream_socket.request_stream(STATISTICS);

    let video_receive_thread = spawn_named("alvr.video.consume", {
        let ctx = Arc::clone(&ctx);
        move || {
            let mut stream_corrupted = true;
            while is_streaming(&ctx) {
                let data = match video_receiver.recv(STREAMING_RECV_TIMEOUT) {
                    Ok(data) => data,
                    Err(ConnectionError::TryAgain(_)) => continue,
                    Err(ConnectionError::Other(_)) => return,
                };
                let Ok((header, nal)) = data.get() else {
                    return;
                };
                VIDEO_NET_RECEIVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Counted for every packet, independent of is_idr: every JPEG XS slice packet is
                // flagged IDR, so the loss check further down never runs for them.
                VIDEO_NET_LOST.fetch_add(
                    data.lost_before() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );

                // On the LAST column: "received" keeps meaning "the whole frame is here", so
                // network latency stays comparable with the whole-frame path and the derived
                // throughput stays finite -- it is packet bits divided by that latency, and
                // timestamping the first column drove the divisor towards zero and the reported
                // bitrate to absurd values (48 Gbps peaks, 2026-09-19). The pipelining then
                // shows up where it belongs: as a shorter decode stage, because most of the
                // work already happened while the later columns were still arriving.
                if header.packet_index + 1 >= header.packets_per_frame.max(1) {
                    if let Some(stats) = &mut *ctx.statistics_manager.lock() {
                        stats.report_video_packet_received(header.timestamp);
                    }
                }

                if header.is_idr {
                    stream_corrupted = false;
                } else if data.had_packet_loss() {
                    stream_corrupted = true;
                    if let Some(sender) = &mut *ctx.control_sender.lock() {
                        sender.send(&ClientControlPacket::RequestIdr).ok();
                    }
                    warn!("Network dropped video packet");
                }

                if !stream_corrupted || !settings.connection.avoid_video_glitching {
                    let submitted = ctx
                        .decoder_callback
                        .lock()
                        .as_mut()
                        .is_some_and(|callback| {
                            callback(
                                header.timestamp,
                                nal,
                                header.packet_index,
                                header.packets_per_frame.max(1),
                                header.column_index,
                            )
                        });

                    if !submitted {
                        stream_corrupted = true;
                        if let Some(sender) = &mut *ctx.control_sender.lock() {
                            sender.send(&ClientControlPacket::RequestIdr).ok();
                        }
                        warn!("Dropped video packet. Reason: Decoder saturation")
                    }
                } else {
                    if let Some(sender) = &mut *ctx.control_sender.lock() {
                        sender.send(&ClientControlPacket::RequestIdr).ok();
                    }
                    warn!("Dropped video packet. Reason: Waiting for IDR frame")
                }
            }
        }
    });

    let game_audio_thread = if let Switch::Enabled(config) = settings.audio.game_audio {
        let device = AudioDevice::new_output(None).to_con()?;
        spawn_named("alvr.audio.play", {
            let ctx = Arc::clone(&ctx);
            move || {
                while is_streaming(&ctx) {
                    alvr_common::show_err(audio::play_audio_loop(
                        || is_streaming(&ctx),
                        &device,
                        2,
                        negotiated_config.game_audio_sample_rate,
                        config.buffering.clone(),
                        &mut game_audio_receiver,
                    ));
                }
            }
        })
    } else {
        thread::spawn(|| ())
    };

    let microphone_thread = if matches!(settings.audio.microphone, Switch::Enabled(_)) {
        let device = AudioDevice::new_input(None).to_con()?;

        let microphone_sender = stream_socket.request_stream(AUDIO);

        spawn_named("alvr.audio.mic", {
            let ctx = Arc::clone(&ctx);
            move || {
                while is_streaming(&ctx) {
                    let ctx = Arc::clone(&ctx);
                    match audio::record_audio_blocking(
                        Arc::new(move || is_streaming(&ctx)),
                        microphone_sender.clone(),
                        &device,
                        1,
                        false,
                    ) {
                        Ok(()) => break,
                        Err(e) => {
                            error!("Audio record error: {e}");

                            continue;
                        }
                    }
                }
            }
        })
    } else {
        thread::spawn(|| ())
    };

    let haptics_receive_thread = spawn_named("alvr.haptics.recv", {
        let ctx = Arc::clone(&ctx);
        let event_queue = Arc::clone(&event_queue);
        move || {
            while is_streaming(&ctx) {
                let data = match haptics_receiver.recv(STREAMING_RECV_TIMEOUT) {
                    Ok(packet) => packet,
                    Err(ConnectionError::TryAgain(_)) => continue,
                    Err(ConnectionError::Other(_)) => return,
                };
                let Ok(haptics) = data.get_header() else {
                    return;
                };

                event_queue.lock().push_back(ClientCoreEvent::Haptics {
                    device_id: haptics.device_id,
                    duration: haptics.duration,
                    frequency: haptics.frequency,
                    amplitude: haptics.amplitude,
                });
            }
        }
    });

    let (log_channel_sender, log_channel_receiver) = mpsc::channel();

    let control_send_thread = spawn_named("alvr.control.send", {
        let ctx = Arc::clone(&ctx);
        let event_queue = Arc::clone(&event_queue);
        let disconnect_notif = Arc::clone(&disconnect_notif);
        move || {
            let mut keepalive_deadline = Instant::now();

            #[cfg(target_os = "android")]
            let mut battery_deadline = Instant::now();

            while is_streaming(&ctx) && *lifecycle_state.read() == LifecycleState::Resumed {
                if let (Ok(packet), Some(sender)) = (
                    log_channel_receiver.recv_timeout(STREAMING_RECV_TIMEOUT),
                    &mut *ctx.control_sender.lock(),
                ) {
                    if let Err(e) = sender.send(&packet) {
                        info!("Server disconnected. Cause: {e:?}");
                        set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);

                        //break;
                    }
                }

                if Instant::now() > keepalive_deadline {
                    if let Some(sender) = &mut *ctx.control_sender.lock() {
                        sender.send(&ClientControlPacket::KeepAlive).ok();

                        keepalive_deadline = Instant::now() + KEEPALIVE_INTERVAL;
                    }
                }

                #[cfg(target_os = "android")]
                if Instant::now() > battery_deadline {
                    let (gauge_value, is_plugged) = alvr_system_info::get_battery_status();
                    if let Some(sender) = &mut *ctx.control_sender.lock() {
                        sender
                            .send(&ClientControlPacket::Battery(crate::BatteryInfo {
                                device_id: *alvr_common::HEAD_ID,
                                gauge_value,
                                is_plugged,
                            }))
                            .ok();
                    }

                    battery_deadline = Instant::now() + Duration::from_secs(5);
                }
            }

            disconnect_notif.notify_one();
        }
    });

    let control_receive_thread = spawn_named("alvr.control.recv", {
        let ctx = Arc::clone(&ctx);
        let event_queue = Arc::clone(&event_queue);
        let disconnect_notif = Arc::clone(&disconnect_notif);
        move || {
            let mut disconnection_deadline = Instant::now() + KEEPALIVE_TIMEOUT;
            while is_streaming(&ctx) {
                let maybe_packet = control_receiver.recv(STREAMING_RECV_TIMEOUT);

                match maybe_packet {
                    Ok(ServerControlPacket::DecoderConfig(config)) => {
                        event_queue
                            .lock()
                            .push_back(ClientCoreEvent::DecoderConfig {
                                codec: config.codec,
                                config_nal: config.config_buffer,
                            });
                    }
                    Ok(ServerControlPacket::Restarting) => {
                        info!("{SERVER_RESTART_MESSAGE}");
                        set_hud_message(&event_queue, SERVER_RESTART_MESSAGE);
                        disconnect_notif.notify_one();
                    }
                    Ok(ServerControlPacket::ReservedBuffer(buffer)) => {
                        // NB: it's normal for deserialization to fail if server has different
                        // version
                        if let Ok(config) = RealTimeConfig::decode(&buffer) {
                            event_queue
                                .lock()
                                .push_back(ClientCoreEvent::RealTimeConfig(config));
                        }
                    }
                    Ok(_) => (),
                    Err(ConnectionError::TryAgain(_)) => {
                        if Instant::now() > disconnection_deadline {
                            info!("{CONNECTION_TIMEOUT_MESSAGE}");
                            set_hud_message(&event_queue, CONNECTION_TIMEOUT_MESSAGE);
                            disconnect_notif.notify_one();
                        } else {
                            continue;
                        }
                    }
                    Err(e) => {
                        info!("{SERVER_DISCONNECTED_MESSAGE} Cause: {e}");
                        set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);
                        disconnect_notif.notify_one();
                    }
                }

                disconnection_deadline = Instant::now() + KEEPALIVE_TIMEOUT;
            }
        }
    });

    let stream_receive_thread = spawn_named("alvr.stream.recv", {
        let ctx = Arc::clone(&ctx);
        let event_queue = Arc::clone(&event_queue);
        let disconnect_notif = Arc::clone(&disconnect_notif);
        move || {
            // This thread must drain the socket faster than ~113,000 shards a second arrive at
            // full resolution. At the default QoS it competes with the decoder's threads and
            // loses: the kernel buffer ran full and packets were dropped (2026-09-22).
            #[cfg(target_vendor = "apple")]
            {
                extern "C" {
                    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
                }
                const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
                let result = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
                if result != 0 {
                    warn!("Stream receive thread: setting QoS user-interactive failed ({result})");
                }
            }

            // Sample the kernel receive queue every 256 shards (~400 times a second at full
            // resolution): one ioctl per shard would double the syscall rate this loop exists
            // to keep up with.
            let mut shard_count: u32 = 0;
            let mut datagrams: u64 = 0;
            while is_streaming(&ctx) {
                shard_count = shard_count.wrapping_add(1);
                if shard_count % 256 == 0 {
                    STREAM_NET_DATAGRAMS.fetch_add(datagrams, std::sync::atomic::Ordering::Relaxed);
                    datagrams = 0;
                    STREAM_NET_RECV_CPU_NS
                        .store(current_thread_cpu_ns(), std::sync::atomic::Ordering::Relaxed);
                    let (queued, buffer) = stream_socket.receive_queue();
                    if let Some(q) = queued {
                        VIDEO_NET_QUEUE_MAX.fetch_max(q, std::sync::atomic::Ordering::Relaxed);
                    }
                    if let Some(b) = buffer {
                        VIDEO_NET_RECV_BUFFER.store(b, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                match stream_socket.recv() {
                    Ok(()) => datagrams += 1,
                    Err(ConnectionError::TryAgain(_)) => continue,
                    Err(e) => {
                        info!("Client disconnected. Cause: {e}");
                        set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);
                        disconnect_notif.notify_one();
                    }
                }
            }
        }
    });

    *ctx.control_sender.lock() = Some(control_sender);
    *ctx.tracking_sender.lock() = Some(tracking_sender);
    *ctx.statistics_sender.lock() = Some(statistics_sender);
    if let Switch::Enabled(filter_level) = settings.extra.logging.client_log_report_level {
        *LOG_CHANNEL_SENDER.lock() = Some(LogMirrorData {
            sender: log_channel_sender,
            filter_level,
            debug_groups_config: settings.extra.logging.debug_groups,
        });
    }
    event_queue.lock().push_back(streaming_start_event);

    *connection_state_lock = ConnectionState::Streaming;

    dbg_connection!("connection_pipeline: Unlock streams");

    // Unlock CONNECTION_STATE and block thread
    wait_rwlock(&disconnect_notif, &mut connection_state_lock);

    *connection_state_lock = ConnectionState::Disconnecting;

    *ctx.control_sender.lock() = None;
    *ctx.tracking_sender.lock() = None;
    *ctx.statistics_sender.lock() = None;
    *LOG_CHANNEL_SENDER.lock() = None;

    event_queue
        .lock()
        .push_back(ClientCoreEvent::StreamingStopped);

    // Remove lock to allow threads to properly exit:
    drop(connection_state_lock);

    dbg_connection!("connection_pipeline: Destroying streams");

    video_receive_thread.join().ok();
    game_audio_thread.join().ok();
    microphone_thread.join().ok();
    haptics_receive_thread.join().ok();
    control_send_thread.join().ok();
    control_receive_thread.join().ok();
    stream_receive_thread.join().ok();

    dbg_connection!("connection_pipeline: End");

    Ok(())
}

// Named, so per-thread CPU accounting on the client can tell these apart (2026-09-24: 30
// unnamed threads took 203 % of the process's 254 %). On Apple platforms the name becomes the
// OS thread name as well.
fn spawn_named<F, T>(name: &str, f: F) -> thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .expect("failed to spawn thread")
}
