#pragma once

// ---------------------------------------------------------------------------
// VideoEncoderJpegXs
//
// Real VideoEncoder implementation using Intel's open-source SVT-JPEG-XS
// encoder. Selected by CEncoder::Initialize() when Settings::m_codec ==
// ALVR_CODEC_JPEGXS -- see the dispatch logic there. Originally built (and
// still structurally identical apart from where its output goes) as an
// additive, disk-dump-only side channel for comparing JPEG XS against the
// real HEVC/H.264/AV1 stream; promoted to a real, selectable, network-
// streamed codec by routing its encoded output through SetVideoConfigNals()/
// VideoSend() (see EncodeWorkerLoop()) instead of only StreamDumper.
//
// RGB->YUV420 conversion happens on the GPU, using our own small pixel
// shaders (compiled at runtime via D3DCompile, see EnsureGpuPipeline()) and
// our own minimal two-pass render sequence rather than ALVR's
// RenderPipelineYUV class -- see the comment on RenderYuvGpu() for why. We
// render Y and UV into two *separate*, ordinary (non-planar) textures of our
// own -- never a shared NV12 resource, and never touching FrameRender's
// m_pStagingTexture -- so this is purely additive. We initially reused
// ALVR's own already-compiled rgbtoyuv420.hlsl for this, but its chroma
// addressing depends on a viewport-larger-than-render-target clipping trick
// that turned out not to hold up on real hardware (see m_yPixelShader's
// comment in the header) -- our own shaders sidestep that entirely.
//
// Note: we deliberately do NOT render into a single NV12-format texture
// (one resource, two planar RTVs) the way ALVR's own RenderPipelineYUV does.
// That mechanism is only ever exercised by ALVR's real path when HDR is
// enabled (FrameRender.cpp, gated on Settings::m_enableHdr) -- i.e. it's
// essentially unvalidated on most systems, including whichever GPU/driver
// this was developed against, where it silently produced an all-zero
// (solid green when decoded) result despite every D3D11 call succeeding.
// Two ordinary R8_UNORM/R8G8_UNORM render targets sidestep that planar-RTV
// codepath entirely and are about as vanilla as GPU render-to-texture gets.
//
// Uses Intel's open source SVT-JPEG-XS encoder/decoder library
// (https://github.com/OpenVisualCloud/SVT-JPEG-XS, BSD-2-Clause-Patent).
// This is a build-time optional dependency: the whole class compiles out
// unless ALVR_JPEGXS is defined, which build.rs only does when it finds
// deps/windows/svt-jpegxs/{include,lib} populated. See
// deps/windows/svt-jpegxs/README.md for how to build/place that dependency.
//
// Output (network): SetVideoConfigNals()/VideoSend() (bindings.h), the same
// FFI calls H264/HEVC/AV1 use -- see EncodeWorkerLoop(). No NAL-unit framing
// (JPEG XS has none) -- these are called directly, bypassing NalParsing.cpp
// entirely, matching how AV1 already bypasses NAL scanning.
// Output (optional disk dump, Dashboard "Dump stream to disk"):
//  - <output dir>\alvr_stream_jpegxs_svtjpegxs.jxs   raw JPEG XS codestream
//    (concatenated per-frame codestreams; FFmpeg's raw JPEG-XS demuxer,
//    merged upstream in Dec 2025, understands this layout, as should any
//    JPEG-XS-enabled VLC build using that FFmpeg).
//  - <output dir>\alvr_stream_jpegxs_svtjpegxs_stats.csv  same FPS/bitrate
//    CSV format StreamDumper uses for the HEVC/AMF/AV1 dumps.
// ---------------------------------------------------------------------------

#ifdef ALVR_JPEGXS

#include "VideoEncoder.h"
#include "alvr_server/StreamDumper.h"
#include "d3d-render-utils/RenderUtils.h"
#include "shared/d3drender.h"

#include <atomic>
#include <condition_variable>
#include <cstdint>
#include <d3d11.h>
#include <memory>
#include <mutex>
#include <thread>
#include <vector>
#include <wrl.h>

extern "C" {
#include <SvtJpegxsEnc.h>
}

class VideoEncoderJpegXs : public VideoEncoder {
public:
    VideoEncoderJpegXs(std::shared_ptr<CD3DRender> d3dRender, uint32_t width, uint32_t height);
    ~VideoEncoderJpegXs();

    // Throws (see MakeException) if the encoder could not be set up
    // (SVT-JPEG-XS init failure, etc) -- CEncoder::Initialize() catches this
    // and falls back to hardware encoder probing, exactly like AMF/NVENC's
    // own Initialize() failures already do. The GPU conversion pipeline
    // itself is set up lazily on the first Transmit() call (see there for
    // why).
    void Initialize() override;
    void Shutdown() override;

    // Genuinely non-blocking: only issues async GPU work (a render pass + a
    // CopyResource enqueue) and hands a staging-slot index off to the
    // readback thread -- the actual blocking GPU Map() / memcpy() readback
    // happens on ReadbackWorkerLoop, never on this (real-time critical)
    // thread. If the mailbox still holds an unconsumed frame, this frame is
    // silently dropped rather than stalling the caller. presentationTime/
    // insertIDR are accepted but unused -- JPEG XS is intra-only, every
    // frame is effectively a keyframe.
    void Transmit(
        ID3D11Texture2D* pTexture,
        uint64_t presentationTime,
        uint64_t targetTimestampNs,
        bool insertIDR
    ) override;

private:
    // One-time setup of the GPU RGB->YUV420 render pipeline, run on the
    // first Transmit() call (not in Initialize()) because it needs a
    // source texture to bind as input -- FrameRender's staging texture is a
    // stable, persistent GPU object reused every frame (created once in
    // FrameRender::Startup(), only its contents change), so binding it once
    // here and re-rendering every frame is safe. Returns false (and sets
    // m_gpuSetupFailed) if anything throws.
    bool EnsureGpuPipeline(ID3D11Texture2D* sourceTexture);

    // Renders Y and UV in two separate single-render-target passes, both
    // using the same rgbtoyuv420.hlsl pixel shader (it always computes all
    // three Y/U/V values; whichever output isn't bound to a target that
    // draw call is simply discarded). We deliberately do NOT use ALVR's own
    // d3d_render_utils::RenderPipelineYUV here, even though it wraps the
    // same shader for the real HDR path: its Render() binds Y and UV as two
    // simultaneous render targets in one draw call, which -- per a comment
    // left in RenderPipelineYUV.cpp itself ("HACK: Whyyyyyy does the Y
    // channel only render the top-left corner with both render targets
    // enabled????") -- only writes a small top-left region correctly in
    // that configuration. Its existing workaround re-draws Y alone
    // afterwards to fix Y specifically, but never does the equivalent for
    // UV, leaving most of the chroma plane unwritten/zero. That's fine for
    // casual HDR video viewing (average person may not choose to
    // pixel-diff their headset feed) but fatal for us: zero chroma decodes
    // as a hard green cast, which is exactly what showed up in the dumped
    // JPEG XS file. Doing two clean single-target passes here -- without
    // touching the shared RenderPipelineYUV class the other real encoders
    // (AMF/NVENC) also use -- fixes it for this class only.
    void RenderYuvGpu(ID3D11DeviceContext* context);

    // Two-stage pipeline: ReadbackWorkerLoop (GPU Map()/memcpy + UV
    // deinterleave) and EncodeWorkerLoop (SVT-JPEG-XS) run on separate
    // threads so encoding frame N can overlap with reading back frame N+1,
    // instead of running fully serially. They hand off through the m_yuv*
    // mailbox below. The RGB->YUV420 color conversion itself no longer runs
    // on the CPU at all -- the GPU does it before either of these stages.
    void ReadbackWorkerLoop();
    void EncodeWorkerLoop();

    std::shared_ptr<CD3DRender> m_d3dRender;

    // GPU-side RGB->YUV420 conversion, reusing ALVR's own rgbtoyuv420.hlsl
    // shader (see EnsureGpuPipeline()/RenderYuvGpu()) but with our own
    // minimal two-pass render sequence, not ALVR's RenderPipelineYUV class
    // -- see the long comment at the top of this file for why. m_gpuYTex /
    // m_gpuUVTex are our own plain render targets -- never FrameRender's
    // m_pStagingTexture -- so this never affects the real stream.
    Microsoft::WRL::ComPtr<ID3D11VertexShader> m_quadVertexShader;
    // ALVR's "quad" vertex shader (QuadVertexShader.cso) is *not*
    // SV_VertexID-procedural -- it needs a real bound vertex buffer,
    // matching FrameRender's SimpleVertex layout (POSITION float4 @0,
    // TEXCOORD float2 @16, VIEW uint @24; see FrameRender.h). FrameRender's
    // own color-correction/FFR/HDR-YUV passes only work because they run
    // immediately after FrameRender's own eye-compositing draw in the same
    // function call, silently inheriting *that* draw's still-bound vertex
    // buffer + input layout. We have no such guarantee (different thread,
    // different call context entirely), so we create and bind our own
    // self-contained quad -- 4 vertices, plain full-screen NDC quad,
    // TEXCOORD 0..1 -- instead of relying on an accident of leftover state.
    Microsoft::WRL::ComPtr<ID3D11InputLayout> m_quadInputLayout;
    Microsoft::WRL::ComPtr<ID3D11Buffer> m_quadVertexBuffer;
    // Two separate, minimal pixel shaders of our own (compiled at runtime via
    // D3DCompile from an embedded HLSL string in VideoEncoderJpegXs.cpp) --
    // NOT ALVR's shared rgbtoyuv420.hlsl / RGBTOYUV420_CSO anymore. That
    // shared shader's chroma pass relies on a "full-size viewport bound to a
    // half-size render target, rely on rasterizer clipping to the physical
    // target extent, then *2-mod-width the raw clipped pixel index back into
    // a full-res source address" trick, which turned out to NOT reliably
    // clip the way the D3D11 spec's "viewport may exceed the bound render
    // target" wording suggests it should on real hardware -- in practice we
    // observed part of the source image rendered enlarged/misplaced into the
    // chroma plane instead of being cleanly clipped. Our own shaders instead
    // use a genuinely correctly-sized viewport per pass (see m_gpuViewport
    // for Y, m_uvViewport for UV, both set in EnsureGpuPipeline) and sample
    // the source directly at the interpolated `uv` texcoord with no manual
    // texel-index math at all -- standard, unambiguous GPU behavior.
    Microsoft::WRL::ComPtr<ID3D11PixelShader> m_yPixelShader;
    Microsoft::WRL::ComPtr<ID3D11PixelShader> m_uvPixelShader;
    Microsoft::WRL::ComPtr<ID3D11Buffer> m_yuvParamBuffer;
    Microsoft::WRL::ComPtr<ID3D11ShaderResourceView> m_sourceSRV;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_gpuYTex;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_gpuUVTex;
    Microsoft::WRL::ComPtr<ID3D11RenderTargetView> m_rtvY;
    Microsoft::WRL::ComPtr<ID3D11RenderTargetView> m_rtvUV;
    // Full-resolution viewport, used for the Y pass (matches m_gpuYTex's
    // real dimensions exactly -- no clipping trickery involved).
    D3D11_VIEWPORT m_gpuViewport {};
    // Half-resolution viewport (m_chromaWidth x m_chromaHeight), used for
    // the UV pass -- matches m_gpuUVTex's real dimensions exactly. Having a
    // genuinely correctly-sized viewport per render target, rather than
    // sharing one oversized viewport between both passes, is what actually
    // fixes the chroma-plane corruption; see the comment on m_yPixelShader.
    D3D11_VIEWPORT m_uvViewport {};
    // Explicit, known-good pipeline state for RenderYuvGpu(). We share the
    // immediate context with FrameRender's own real-path rendering (and
    // whatever CEncoder/VideoEncoder submission work runs in between), so we
    // cannot assume the rasterizer/blend/depth state is at D3D11 defaults
    // when our draws run -- an inherited scissor rect, cull mode, or write
    // mask left over from earlier in the frame can silently discard every
    // pixel of our two-pass draw, leaving m_gpuYTex/m_gpuUVTex at their
    // all-zero creation-time content (which decodes to solid green). Binding
    // our own no-cull/no-scissor/opaque/depth-disabled state every call rules that
    // class of bug out entirely, independent of whatever the real path did
    // just before us.
    Microsoft::WRL::ComPtr<ID3D11RasterizerState> m_rasterizerState;
    Microsoft::WRL::ComPtr<ID3D11BlendState> m_blendState;
    Microsoft::WRL::ComPtr<ID3D11DepthStencilState> m_depthStencilState;
    bool m_gpuPipelineReady = false;
    bool m_gpuSetupFailed = false;

    // Double-buffered CPU-readback staging copies of m_gpuYTex/m_gpuUVTex.
    // Transmit() CopyResource()s into one slot per frame (cheap async GPU
    // enqueue, two calls -- one per plane) and hands the *other* slot
    // (written one frame earlier, so the GPU has had a full frame period to
    // get to it) off to ReadbackWorkerLoop, which does the actual
    // Map()/memcpy() -- kept off the real encode thread entirely.
    // m_stagingReserved marks a slot as "handed off, not yet Map()'d", so
    // Transmit knows not to recycle it into a new CopyResource() while
    // readback is still reading it.
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_stagingYTex[2];
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_stagingUVTex[2];
    uint64_t m_stagingTimestampNs[2] = { 0, 0 };
    bool m_stagingHasData[2] = { false, false };
    bool m_stagingReserved[2] = { false, false };
    int m_stagingWriteIndex = 0;

    uint32_t m_width;
    uint32_t m_height;
    // 4:2:0 chroma planes are half-width, half-height (m_gpuUVTex's actual
    // dimensions).
    uint32_t m_chromaWidth;
    uint32_t m_chromaHeight;

    svt_jpeg_xs_encoder_api_t m_enc {};
    bool m_encInitialized = false;

    // Set after the one-time, empty SetVideoConfigNals() call on the first
    // successfully encoded frame (EncodeWorkerLoop) -- a member rather than
    // a function-local static so it resets correctly if this encoder is
    // ever destroyed and a new one constructed for a fresh stream.
    bool m_sentConfig = false;

    // Encode stage's local working copy of one frame's YUV420 planes (swapped
    // in from the m_yuv* mailbox below), plus its bitstream output buffer.
    // Touched only by EncodeWorkerLoop.
    std::vector<uint8_t> m_planeY, m_planeCb, m_planeCr;
    std::vector<uint8_t> m_bitstreamBuf;

    std::unique_ptr<StreamDumper> m_streamDumper;

    // Stage 1 mailbox: Transmit (real encode thread) -> ReadbackWorkerLoop.
    // Single-slot, guarded by m_mutex. Carries only a staging-texture slot
    // index + timestamp -- NOT pixel data. ReadbackWorkerLoop does the
    // actual Map()/memcpy() readback and UV deinterleave, which is the whole
    // point: Transmit never blocks on the GPU.
    int m_pendingStagingIndex = -1;
    uint64_t m_pendingTimestampNs = 0;
    bool m_hasPendingFrame = false;
    std::mutex m_mutex;
    std::condition_variable m_cv;

    // ReadbackWorkerLoop's private scratch buffers: Map()/memcpy()/
    // deinterleave lands here first, so the GPU staging slot (and its
    // m_stagingReserved flag) can be released immediately, before waiting
    // on the yuv mailbox below to free up. Only ever touched by
    // ReadbackWorkerLoop -- no locking needed.
    std::vector<uint8_t> m_readbackScratchY, m_readbackScratchCb, m_readbackScratchCr;

    // Stage 2 mailbox: ReadbackWorkerLoop -> EncodeWorkerLoop. Single-slot,
    // guarded by m_yuvMutex. Readback blocks here (backpressure) if the slot
    // still holds a frame the encode stage hasn't picked up yet, so readback
    // never gets more than one frame ahead of encode -- this is what lets
    // readback(N+1) genuinely overlap with encode(N) instead of the two
    // running serially.
    std::vector<uint8_t> m_yuvMailboxY, m_yuvMailboxCb, m_yuvMailboxCr;
    uint64_t m_yuvTimestampNs = 0;
    bool m_yuvReady = false;
    std::mutex m_yuvMutex;
    std::condition_variable m_yuvCv;

    std::atomic_bool m_encodeBusy { false };
    std::atomic_bool m_exiting { false };
    std::thread m_readbackThread;
    std::thread m_encodeThread;
};

#endif // ALVR_JPEGXS
