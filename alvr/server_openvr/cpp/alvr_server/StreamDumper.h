#pragma once

// ---------------------------------------------------------------------------
// StreamDumper
//
// Test-only helper used to compare the HEVC (or H.264/AV1) stream ALVR
// currently sends to the headset against a future JPEG XS encoder.
//
// It taps the encoder output at the same point where ALVR already had a
// dormant "fpOut" debug hook (VideoEncoderNVENC.cpp / VideoEncoderAMF.cpp,
// right where the raw encoded bytes are handed off to ParseFrameNals() /
// VideoSend()), and:
//
//  1. Writes the raw encoded elementary stream (Annex-B for H.264/HEVC,
//     raw OBU stream for AV1) to a file in C:\Temp\ that can be opened
//     directly in VLC (Media > Open File...).
//  2. Appends one CSV row per second to a stats log with FPS, bitrate,
//     and average frame size, so the numbers can be diffed against a
//     later JPEG XS run.
//
// To disable the dump without removing the instrumentation, flip
// kStreamDumpEnabled to false below and rebuild.
// ---------------------------------------------------------------------------

#include <chrono>
#include <cstdint>
#include <ctime>
#include <filesystem>
#include <fstream>
#include <string>

#include "ALVR-common/packet_types.h"

// Master on/off switch for this test instrumentation.
constexpr bool kStreamDumpEnabled = true;

// Directory the dump file and stats log are written to. Must exist or be
// creatable by the driver process.
inline const char* StreamDumpDir() { return "C:\\Temp"; }

inline std::string AlvrCodecName(int codec) {
    switch (codec) {
    case ALVR_CODEC_H264:
        return "h264";
    case ALVR_CODEC_HEVC:
        return "hevc";
    case ALVR_CODEC_AV1:
        return "av1";
    case ALVR_CODEC_JPEGXS:
        return "jpegxs";
    default:
        return "unknown";
    }
}

inline std::string ExtensionForCodec(int codec) {
    switch (codec) {
    case ALVR_CODEC_H264:
        return ".h264";
    case ALVR_CODEC_HEVC:
        return ".h265";
    case ALVR_CODEC_AV1:
        return ".obu";
    case ALVR_CODEC_JPEGXS:
        return ".jxs";
    default:
        return ".bin";
    }
}

class StreamDumper {
public:
    // encoderName identifies which backend produced the stream (e.g. "nvenc",
    // "amf", "svtjpegxs"), so files from different runs don't silently
    // overwrite one another if the GPU/encoder changes between tests.
    // dumpDir defaults to StreamDumpDir() (C:\Temp) to preserve existing
    // behavior for the real-path callers (VideoEncoderNVENC.cpp/AMF.cpp);
    // pass an explicit directory to redirect a given dump elsewhere (e.g.
    // the JPEG XS side channel's configurable output directory).
    StreamDumper(int codec, const std::string& encoderName, const std::string& dumpDir = StreamDumpDir())
        : StreamDumper(AlvrCodecName(codec), ExtensionForCodec(codec), encoderName, dumpDir) { }

    // Overload for streams that aren't one of ALVR's own ALVR_CODEC values
    // (e.g. the JPEG XS side-by-side test encode, codecName="jpegxs").
    StreamDumper(
        const std::string& codecName,
        const std::string& ext,
        const std::string& encoderName,
        const std::string& dumpDir = StreamDumpDir()
    )
        : m_codecName(codecName)
        , m_encoderName(encoderName)
        , m_dumpDir(dumpDir.empty() ? StreamDumpDir() : dumpDir)
        , m_windowStart(std::chrono::steady_clock::now())
        , m_runStart(std::chrono::steady_clock::now()) {
        std::error_code ec;
        std::filesystem::create_directories(m_dumpDir, ec);

        std::string base = m_dumpDir + "\\alvr_stream_" + m_codecName + "_" + encoderName;

        m_streamFile.open(base + ext, std::ios::binary | std::ios::trunc);
        m_statsFile.open(base + "_stats.csv", std::ios::trunc);

        if (m_statsFile) {
            m_statsFile << "wall_clock,elapsed_s,fps,bitrate_kbps,avg_frame_bytes,frames,"
                           "idr_frames,codec,encoder\n";
            m_statsFile.flush();
        }
    }

    bool StreamFileOpen() const { return (bool)m_streamFile; }

    // Writes raw encoded bytes to the dump file. Call once per NAL/packet
    // emitted by the encoder, in order, so the file stays a valid elementary
    // stream.
    void WritePacket(const uint8_t* data, size_t len) {
        if (m_streamFile && data != nullptr && len > 0) {
            m_streamFile.write(reinterpret_cast<const char*>(data), (std::streamsize)len);
        }
    }

    // Call once per encoded video frame (not per NAL) with the total bytes
    // produced for that frame, to keep FPS/bitrate accounting correct even
    // when a frame is split across multiple packets.
    void OnFrameEncoded(size_t totalBytes, bool isIdr) {
        m_bytesInWindow += totalBytes;
        m_framesInWindow += 1;
        if (isIdr) {
            m_idrInWindow += 1;
        }

        auto now = std::chrono::steady_clock::now();
        double windowSecs = std::chrono::duration<double>(now - m_windowStart).count();
        if (windowSecs >= 1.0) {
            FlushWindow(now, windowSecs);
        }
    }

    void Close() {
        if (m_framesInWindow > 0) {
            auto now = std::chrono::steady_clock::now();
            double windowSecs = std::chrono::duration<double>(now - m_windowStart).count();
            if (windowSecs > 0.0) {
                FlushWindow(now, windowSecs);
            }
        }
        if (m_streamFile) {
            m_streamFile.close();
        }
        if (m_statsFile) {
            m_statsFile.close();
        }
    }

private:
    void FlushWindow(std::chrono::steady_clock::time_point now, double windowSecs) {
        double fps = m_framesInWindow / windowSecs;
        double bitrateKbps = (m_bytesInWindow * 8.0 / 1000.0) / windowSecs;
        double avgFrameBytes
            = m_framesInWindow > 0 ? (double)m_bytesInWindow / m_framesInWindow : 0.0;
        double elapsed = std::chrono::duration<double>(now - m_runStart).count();

        if (m_statsFile) {
            m_statsFile << WallClockString() << ',' << elapsed << ',' << fps << ',' << bitrateKbps
                        << ',' << avgFrameBytes << ',' << m_framesInWindow << ','
                        << m_idrInWindow << ',' << m_codecName << ',' << m_encoderName << '\n';
            m_statsFile.flush();
        }

        m_bytesInWindow = 0;
        m_framesInWindow = 0;
        m_idrInWindow = 0;
        m_windowStart = now;
    }

    static std::string WallClockString() {
        auto now = std::chrono::system_clock::now();
        std::time_t t = std::chrono::system_clock::to_time_t(now);
        std::tm tmBuf {};
#ifdef _WIN32
        localtime_s(&tmBuf, &t);
#else
        localtime_r(&t, &tmBuf);
#endif
        char buf[32];
        std::strftime(buf, sizeof(buf), "%Y-%m-%d %H:%M:%S", &tmBuf);
        return std::string(buf);
    }

    std::string m_codecName;
    std::string m_encoderName;
    std::string m_dumpDir;
    std::ofstream m_streamFile;
    std::ofstream m_statsFile;

    uint64_t m_bytesInWindow = 0;
    uint64_t m_framesInWindow = 0;
    uint64_t m_idrInWindow = 0;
    std::chrono::steady_clock::time_point m_windowStart;
    std::chrono::steady_clock::time_point m_runStart;
};
