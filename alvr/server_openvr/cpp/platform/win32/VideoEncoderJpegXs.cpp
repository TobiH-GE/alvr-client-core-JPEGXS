#include "VideoEncoderJpegXs.h"

#ifdef ALVR_JPEGXS

#include "alvr_server/Logger.h"
#include "alvr_server/Settings.h"
#include "alvr_server/bindings.h"

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstring>
#include <stdexcept>
#include <string>

// Also used (beyond the shader-reflection diagnostic below) to compile our
// own RGB->YUV420 pixel shaders at runtime via D3DCompile -- see
// EnsureGpuPipeline().
#include <d3d11shader.h>
#include <d3dcompiler.h>
#pragma comment(lib, "d3dcompiler.lib")

namespace {

// Fallback fraction of hardware_concurrency() the JPEG XS encoder's thread
// pool uses when the Dashboard's "Encoder thread count" is left at 0
// (auto), mirroring SoftwareEncodingConfig's thread_count=0="auto"
// convention. Deliberately well under 1.0: unlike AMF/NVENC (dedicated GPU
// hardware, ~zero sustained CPU cost), this is a software encoder running
// alongside SteamVR, the actual VR application (same machine, OpenVR/
// SteamVR driver model), and ALVR's own network/statistics threads.
// Consuming every logical core for our own thread pool starves those --
// observed in practice (back when this ran as an additive side channel
// next to a real hardware-encoded stream) to collapse that other stream's
// adaptive bitrate control to near-nothing, presumably because its own
// threads got starved and it misread the resulting stall as bad network
// conditions. 0.5 leaves half the machine's logical cores for the rest of
// the system. If the Dashboard thread count is set explicitly (nonzero),
// that value is used as-is instead.
constexpr double kJpegXsAutoCpuFraction = 0.5;

} // namespace

VideoEncoderJpegXs::VideoEncoderJpegXs(
    std::shared_ptr<CD3DRender> d3dRender, uint32_t width, uint32_t height
)
    : m_d3dRender(d3dRender)
    , m_width(width)
    , m_height(height)
    , m_chromaWidth(width / 2)
    , m_chromaHeight(height / 2) { }

VideoEncoderJpegXs::~VideoEncoderJpegXs() { Shutdown(); }

void VideoEncoderJpegXs::Initialize() {
    // CPU-readable staging copies of our GPU-produced Y/UV planes. Two of
    // each, for double-buffered readback -- see Transmit(). The GPU
    // render pipeline itself (m_gpuYTex/m_gpuUVTex and friends) is set up
    // lazily on the first Transmit() call instead of here -- see
    // EnsureGpuPipeline() for why. Two separate ordinary-format staging
    // textures (not one NV12 texture's two subresources) to match
    // m_gpuYTex/m_gpuUVTex -- see the top-of-file comment for why we don't
    // use a planar NV12 resource here.
    D3D11_TEXTURE2D_DESC yDesc = {};
    yDesc.Width = m_width;
    yDesc.Height = m_height;
    yDesc.MipLevels = 1;
    yDesc.ArraySize = 1;
    yDesc.Format = DXGI_FORMAT_R8_UNORM;
    yDesc.SampleDesc.Count = 1;
    yDesc.Usage = D3D11_USAGE_STAGING;
    yDesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    yDesc.BindFlags = 0;

    D3D11_TEXTURE2D_DESC uvDesc = yDesc;
    uvDesc.Width = m_chromaWidth;
    uvDesc.Height = m_chromaHeight;
    uvDesc.Format = DXGI_FORMAT_R8G8_UNORM;

    for (int i = 0; i < 2; i++) {
        HRESULT hr = m_d3dRender->GetDevice()->CreateTexture2D(&yDesc, nullptr, &m_stagingYTex[i]);
        if (FAILED(hr)) {
            throw MakeException(
                "VideoEncoderJpegXs: failed to create Y staging texture %d (hr=0x%08lx).", i, hr
            );
        }
        hr = m_d3dRender->GetDevice()->CreateTexture2D(&uvDesc, nullptr, &m_stagingUVTex[i]);
        if (FAILED(hr)) {
            throw MakeException(
                "VideoEncoderJpegXs: failed to create UV staging texture %d (hr=0x%08lx).", i, hr
            );
        }
    }

    SvtJxsErrorType_t err = svt_jpeg_xs_encoder_load_default_parameters(
        SVT_JPEGXS_API_VER_MAJOR, SVT_JPEGXS_API_VER_MINOR, &m_enc
    );
    if (err != SvtJxsErrorNone) {
        throw MakeException(
            "VideoEncoderJpegXs: svt_jpeg_xs_encoder_load_default_parameters failed (%d).", (int)err
        );
    }

    // Real-time throughput lever: by default svt_jpeg_xs_encoder_load_default_parameters()
    // leaves threads_num at 0 ("lowest possible number of threads"), which is
    // nowhere near enough to keep up with a 90 Hz VR stream. "Encoder thread
    // count" in the Dashboard's JPEG XS settings (0 = auto) controls this;
    // see kJpegXsAutoCpuFraction above for the auto heuristic.
    unsigned int hwThreads = std::thread::hardware_concurrency();
    if (hwThreads == 0) {
        hwThreads = 8;
    }
    uint32_t configuredThreads = Settings::Instance().m_jpegXsThreadCount;
    uint32_t encodeThreads = configuredThreads > 0
        ? configuredThreads
        : (uint32_t)std::max(1.0, std::round(hwThreads * kJpegXsAutoCpuFraction));

    // Bits-per-pixel target, from the Dashboard's JPEG XS settings (default
    // 6.0, a "high quality" starting point for direct comparison against
    // HEVC). SVT-JPEG-XS defines bpp relative to the luma sample count
    // regardless of chroma format, so this carries over unchanged from the
    // earlier 4:2:2 CPU implementation. bpp_denominator=100 gives 0.01 bpp
    // precision on the configured float value.
    float configuredBpp = Settings::Instance().m_jpegXsBitsPerPixel;
    uint32_t bppDenominator = 100;
    uint32_t bppNumerator = (uint32_t)std::max(1.0f, std::round(configuredBpp * (float)bppDenominator));

    m_enc.source_width = m_width;
    m_enc.source_height = m_height;
    m_enc.input_bit_depth = 8;
    // 4:2:0: the RGB->YUV conversion now happens on the GPU via ALVR's own
    // rgbtoyuv420.hlsl shader (see EnsureGpuPipeline()), which produces
    // half-width/half-height 4:2:0 chroma (into m_gpuUVTex) -- one step more
    // aggressive chroma subsampling than the previous CPU implementation's
    // 4:2:2, in exchange for eliminating almost all CPU conversion work and
    // roughly halving the GPU readback size again (~1.5 bytes/pixel vs. ~2
    // for 4:2:2).
    m_enc.colour_format = COLOUR_FORMAT_PLANAR_YUV420;
    m_enc.bpp_numerator = bppNumerator;
    m_enc.bpp_denominator = bppDenominator;
    m_enc.threads_num = encodeThreads;
    m_enc.verbose = VERBOSE_ERRORS;

    err = svt_jpeg_xs_encoder_init(SVT_JPEGXS_API_VER_MAJOR, SVT_JPEGXS_API_VER_MINOR, &m_enc);
    if (err != SvtJxsErrorNone) {
        throw MakeException("VideoEncoderJpegXs: svt_jpeg_xs_encoder_init failed (%d).", (int)err);
    }
    m_encInitialized = true;

    size_t planeSize = (size_t)m_width * m_height;
    size_t chromaPlaneSize = (size_t)m_chromaWidth * m_chromaHeight;

    // Encode stage's local working buffers.
    m_planeY.resize(planeSize);
    m_planeCb.resize(chromaPlaneSize);
    m_planeCr.resize(chromaPlaneSize);

    // Readback's private scratch buffers and the mailbox it hands off to.
    m_readbackScratchY.resize(planeSize);
    m_readbackScratchCb.resize(chromaPlaneSize);
    m_readbackScratchCr.resize(chromaPlaneSize);
    m_yuvMailboxY.resize(planeSize);
    m_yuvMailboxCb.resize(chromaPlaneSize);
    m_yuvMailboxCr.resize(chromaPlaneSize);

    // Generous margin over the nominal target size, in case a frame briefly
    // overshoots the rate-control budget.
    uint32_t targetBytes = (uint32_t
    )(((uint64_t)m_width * m_height * m_enc.bpp_numerator / m_enc.bpp_denominator + 7) / 8);
    m_bitstreamBuf.resize((size_t)targetBytes * 2 + 4096);

    if (Settings::Instance().m_jpegXsDumpToDisk) {
        const std::string& outputDir = Settings::Instance().m_jpegXsOutputDirectory;
        m_streamDumper = std::make_unique<StreamDumper>("jpegxs", ".jxs", "svtjpegxs", outputDir);
        if (!m_streamDumper->StreamFileOpen()) {
            Error(
                "VideoEncoderJpegXs: failed to open JPEG XS dump file in %s.\n", outputDir.c_str()
            );
        } else {
            Info(
                "VideoEncoderJpegXs: dumping the real JPEG XS stream to %s (bpp=%u/%u, GPU "
                "RGB->YUV420, %u encode threads = %u/%u logical cores). Note: this is a "
                "software encoder -- unlike AMF/NVENC it competes with the VR application's "
                "own CPU usage on this machine.\n",
                outputDir.c_str(),
                bppNumerator,
                bppDenominator,
                encodeThreads,
                encodeThreads,
                hwThreads
            );
        }
    } else {
        Info(
            "VideoEncoderJpegXs: running without disk dump (Dashboard 'Dump stream to disk' "
            "disabled). bpp=%u/%u, GPU RGB->YUV420, %u encode threads = %u/%u logical cores. "
            "Note: this is a software encoder -- unlike AMF/NVENC it competes with the VR "
            "application's own CPU usage on this machine.\n",
            bppNumerator,
            bppDenominator,
            encodeThreads,
            encodeThreads,
            hwThreads
        );
    }

    m_exiting = false;
    m_readbackThread = std::thread(&VideoEncoderJpegXs::ReadbackWorkerLoop, this);
    m_encodeThread = std::thread(&VideoEncoderJpegXs::EncodeWorkerLoop, this);
}

bool VideoEncoderJpegXs::EnsureGpuPipeline(ID3D11Texture2D* sourceTexture) {
    if (m_gpuPipelineReady) {
        return true;
    }
    if (m_gpuSetupFailed) {
        return false;
    }

    try {
        auto* device = m_d3dRender->GetDevice();
        auto* context = m_d3dRender->GetContext();

        std::vector<uint8_t> quadShaderCSO(
            QUAD_SHADER_CSO_PTR, QUAD_SHADER_CSO_PTR + QUAD_SHADER_CSO_LEN
        );
        m_quadVertexShader = d3d_render_utils::CreateVertexShader(device, quadShaderCSO);

        // TEMPORARY DIAGNOSTIC: reflect the actual input signature of
        // QuadVertexShader.cso instead of continuing to guess whether it's
        // SV_VertexID-procedural or needs real POSITION/TEXCOORD/VIEW
        // vertex data.
        {
            Microsoft::WRL::ComPtr<ID3D11ShaderReflection> reflector;
            // __uuidof(...) instead of the extern IID_ID3D11ShaderReflection
            // symbol -- the latter lives in dxguid.lib, which isn't linked
            // here; __uuidof is resolved by the compiler itself, no extra
            // lib needed.
            HRESULT hr = D3DReflect(
                quadShaderCSO.data(),
                quadShaderCSO.size(),
                __uuidof(ID3D11ShaderReflection),
                (void**)reflector.GetAddressOf()
            );
            if (SUCCEEDED(hr)) {
                D3D11_SHADER_DESC shaderDesc;
                reflector->GetDesc(&shaderDesc);
                Info(
                    "VideoEncoderJpegXs: QUAD SHADER REFLECTION: %u input parameters.\n",
                    shaderDesc.InputParameters
                );
                for (UINT i = 0; i < shaderDesc.InputParameters; i++) {
                    D3D11_SIGNATURE_PARAMETER_DESC paramDesc;
                    reflector->GetInputParameterDesc(i, &paramDesc);
                    Info(
                        "VideoEncoderJpegXs: QUAD SHADER REFLECTION: input[%u] = %s%u "
                        "(register=%u, mask=0x%x, componentType=%d)\n",
                        i,
                        paramDesc.SemanticName,
                        paramDesc.SemanticIndex,
                        paramDesc.Register,
                        paramDesc.Mask,
                        (int)paramDesc.ComponentType
                    );
                }
            } else {
                Info("VideoEncoderJpegXs: QUAD SHADER REFLECTION: D3DReflect failed hr=0x%08lx\n", (unsigned long)hr);
            }
        }

        // Own input layout + vertex buffer for the quad shader -- see the
        // long comment on the member declarations in the header for why
        // this is necessary (the shader is not SV_VertexID-procedural).
        // Layout matches FrameRender::SimpleVertex exactly (POSITION
        // float4 @0, TEXCOORD float2 @16, VIEW uint @24), validated
        // against our own copy of the same QuadVertexShader.cso bytes, so
        // this is guaranteed compatible with the shader we're binding.
        {
            D3D11_INPUT_ELEMENT_DESC layout[] = {
                { "POSITION", 0, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA,
                  0 },
                { "TEXCOORD", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 16, D3D11_INPUT_PER_VERTEX_DATA, 0 },
                { "VIEW", 0, DXGI_FORMAT_R32_UINT, 0, 24, D3D11_INPUT_PER_VERTEX_DATA, 0 },
            };
            OK_OR_THROW(
                device->CreateInputLayout(
                    layout,
                    3,
                    quadShaderCSO.data(),
                    quadShaderCSO.size(),
                    &m_quadInputLayout
                ),
                "VideoEncoderJpegXs: failed to create quad input layout."
            );

            struct QuadVertex {
                float pos[4];
                float tex[2];
                uint32_t view;
            };
            // Plain full-screen NDC quad (z/w chosen so the shader's clip
            // divide is a no-op), triangle-strip winding matching
            // FrameRender's own convention (TL, BR, TR, BL). TEXCOORD is
            // Y-flipped relative to position (v=1 at the NDC top) to match
            // ALVR's own top-left-origin texture convention -- see
            // FrameRender.cpp's "Left View" vertex block for the same
            // pairing.
            QuadVertex vertices[4] = {
                { { -1.0f, 1.0f, 0.5f, 1.0f }, { 0.0f, 1.0f }, 0 }, // top-left
                { { 1.0f, -1.0f, 0.5f, 1.0f }, { 1.0f, 0.0f }, 0 }, // bottom-right
                { { 1.0f, 1.0f, 0.5f, 1.0f }, { 1.0f, 1.0f }, 0 }, // top-right
                { { -1.0f, -1.0f, 0.5f, 1.0f }, { 0.0f, 0.0f }, 0 }, // bottom-left
            };

            D3D11_BUFFER_DESC vbDesc = {};
            vbDesc.Usage = D3D11_USAGE_IMMUTABLE;
            vbDesc.ByteWidth = sizeof(vertices);
            vbDesc.BindFlags = D3D11_BIND_VERTEX_BUFFER;
            D3D11_SUBRESOURCE_DATA vbData = {};
            vbData.pSysMem = vertices;
            OK_OR_THROW(
                device->CreateBuffer(&vbDesc, &vbData, &m_quadVertexBuffer),
                "VideoEncoderJpegXs: failed to create quad vertex buffer."
            );
        }

        struct YUVParams {
            float offset[4];
            float yCoeff[4];
            float uCoeff[4];
            float vCoeff[4];
        };

        // sourceTexture (FrameRender's non-HDR composition texture) is
        // created as fully-typed DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, not
        // TYPELESS -- so unlike a typeless resource, we cannot create an SRV
        // on it with a different (non-_SRGB) format to suppress the
        // automatic gamma decode; D3D11 rejects that (CreateShaderResourceView
        // fails outright). ALVR's own FrameRender.cpp hits the same
        // constraint for its color-correction pass and works around it in
        // shader code instead (see its "inputColorAdjust" logic) -- we do
        // the same here: detect the _SRGB formats host-side and pass a flag
        // into the shader (packed into offset.w, otherwise unused) that
        // tells it to re-encode (gamma-compress) the already-linearized
        // Sample() result before applying the BT.709 coefficients, which
        // expect gamma-encoded R'G'B' input, not scene-linear values.
        bool sourceNeedsSrgbEncode = false;
        {
            D3D11_TEXTURE2D_DESC sourceDesc;
            sourceTexture->GetDesc(&sourceDesc);
            sourceNeedsSrgbEncode = sourceDesc.Format == DXGI_FORMAT_R8G8B8A8_UNORM_SRGB
                || sourceDesc.Format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
                || sourceDesc.Format == DXGI_FORMAT_B8G8R8X8_UNORM_SRGB;
        }

        // BT.709, 8-bit, full range -- matches the color math the CPU-side
        // RGB->YUV conversion used before this GPU rewrite, for a
        // consistent look between runs. (ALVR's own HDR path uses BT.2020
        // instead, since it's converting a wide-gamut source; not
        // applicable here -- we always treat the source as ordinary SDR.)
        YUVParams paramStruct
            = { { 0.0000000f, 0.5000000f, 0.5000000f, sourceNeedsSrgbEncode ? 1.0f : 0.0f }, // offset
                { 0.2126000f, 0.7152000f, 0.0722000f, 0.0f }, // yCoeff
                { -0.1145721f, -0.3854279f, 0.5000000f, 0.0f }, // uCoeff
                { 0.5000000f, -0.4541529f, -0.0458471f, 0.0f } }; // vCoeff

        m_yuvParamBuffer = d3d_render_utils::CreateBuffer(device, paramStruct);

        // Our own minimal RGB->YUV420 pixel shaders, compiled at runtime via
        // D3DCompile (we already link d3dcompiler.lib for the shader
        // reflection diagnostic above) rather than reusing ALVR's
        // rgbtoyuv420.hlsl / RGBTOYUV420_CSO -- see the long comment on
        // m_yPixelShader in the header for why. No manual texel-index math:
        // each pass just samples the source at the interpolated `uv`
        // texcoord, which is correct as long as the bound viewport actually
        // matches the render target's real dimensions (see m_gpuViewport /
        // m_uvViewport below). The sampler is declared inline in the HLSL
        // itself (a "shader-literal" sampler, compiled directly into the
        // shader bytecode) -- same technique ALVR's own rgbtoyuv420.hlsl
        // uses, no host-side CreateSamplerState()/PSSetSamplers() call
        // needed.
        static const char* kYuvShaderSource = R"HLSL(
            cbuffer YUVParams {
                float4 offset;
                float4 yCoeff;
                float4 uCoeff;
                float4 vCoeff;
            };

            Texture2D<float4> sourceTexture;

            SamplerState bilinearSampler {
                Filter = MIN_MAG_LINEAR_MIP_POINT;
                AddressU = CLAMP;
                AddressV = CLAMP;
            };

            // offset.w != 0 means sourceTexture is bound through an _SRGB
            // SRV, so Sample() already linearized it -- re-encode
            // (gamma-compress) back to gamma space before applying the
            // BT.709 coefficients below, which expect gamma-encoded R'G'B',
            // not scene-linear light. Standard IEC 61966-2-1 sRGB transfer
            // function, branchless (offset.w is uniform for the whole draw
            // call, so this costs nothing on real hardware).
            float3 reencodeSrgbIfNeeded(float3 rgb) {
                float3 lo = rgb * 12.92;
                float3 hi = 1.055 * pow(max(rgb, 0.0), 1.0 / 2.4) - 0.055;
                float3 encoded = lerp(lo, hi, step(0.0031308, rgb));
                return offset.w > 0.5 ? encoded : rgb;
            }

            float4 mainY(float2 uv : TEXCOORD0) : SV_Target0 {
                float3 rgb = reencodeSrgbIfNeeded(sourceTexture.Sample(bilinearSampler, uv).rgb);
                float y = dot(rgb, yCoeff.rgb) + offset.x;
                return float4(y, 0, 0, 1);
            }

            float4 mainUV(float2 uv : TEXCOORD0) : SV_Target0 {
                float3 rgb = reencodeSrgbIfNeeded(sourceTexture.Sample(bilinearSampler, uv).rgb);
                float u = dot(rgb, uCoeff.rgb) + offset.y;
                float v = dot(rgb, vCoeff.rgb) + offset.z;
                return float4(u, v, 0, 1);
            }
        )HLSL";

        auto compileYuvShader = [&](const char* entryPoint, ID3D11PixelShader** outShader) {
            Microsoft::WRL::ComPtr<ID3DBlob> bytecode;
            Microsoft::WRL::ComPtr<ID3DBlob> errors;
            HRESULT hr = D3DCompile(
                kYuvShaderSource,
                strlen(kYuvShaderSource),
                "VideoEncoderJpegXs_yuv.hlsl",
                nullptr,
                nullptr,
                entryPoint,
                "ps_5_0",
                0,
                0,
                bytecode.GetAddressOf(),
                errors.GetAddressOf()
            );
            if (FAILED(hr)) {
                std::string errorText = errors
                    ? std::string(
                          reinterpret_cast<const char*>(errors->GetBufferPointer()),
                          errors->GetBufferSize()
                      )
                    : "(no error blob)";
                throw std::runtime_error(
                    "VideoEncoderJpegXs: D3DCompile failed for entry point '" + std::string(entryPoint)
                    + "': " + errorText
                );
            }
            OK_OR_THROW(
                device->CreatePixelShader(
                    bytecode->GetBufferPointer(), bytecode->GetBufferSize(), nullptr, outShader
                ),
                "VideoEncoderJpegXs: failed to create YUV pixel shader."
            );
        };
        compileYuvShader("mainY", m_yPixelShader.GetAddressOf());
        compileYuvShader("mainUV", m_uvPixelShader.GetAddressOf());

        // Two separate, ordinary render targets -- not a shared NV12
        // resource. See the top-of-file comment for why: relying on a
        // pixel shader writing directly into a planar NV12 texture's two
        // plane-specific views turned out to silently no-op on at least one
        // real-world GPU/driver, despite every D3D11 call reporting success.
        // Plain R8_UNORM/R8G8_UNORM 2D textures have no such ambiguity.
        m_gpuYTex = d3d_render_utils::CreateTexture(device, m_width, m_height, DXGI_FORMAT_R8_UNORM);
        m_gpuUVTex = d3d_render_utils::CreateTexture(
            device, m_chromaWidth, m_chromaHeight, DXGI_FORMAT_R8G8_UNORM
        );

        // Plain nullptr desc -- inherits sourceTexture's native format. We
        // tried overriding the format here to strip _SRGB and avoid the
        // automatic gamma decode, but that fails outright: the composition
        // texture is created as fully-typed DXGI_FORMAT_R8G8B8A8_UNORM_SRGB
        // (FrameRender::Startup()), not TYPELESS, so D3D11 does not permit a
        // differently-formatted view of it at all. Instead, the gamma
        // decode is compensated for in the pixel shader -- see
        // sourceNeedsSrgbEncode / offset.w above and
        // reencodeSrgbIfNeeded() in the shader source.
        OK_OR_THROW(
            device->CreateShaderResourceView(sourceTexture, nullptr, &m_sourceSRV),
            "VideoEncoderJpegXs: failed to create source texture SRV."
        );

        OK_OR_THROW(
            device->CreateRenderTargetView(m_gpuYTex.Get(), nullptr, &m_rtvY),
            "VideoEncoderJpegXs: failed to create Y render target view."
        );

        OK_OR_THROW(
            device->CreateRenderTargetView(m_gpuUVTex.Get(), nullptr, &m_rtvUV),
            "VideoEncoderJpegXs: failed to create UV render target view."
        );

        m_gpuViewport = {};
        m_gpuViewport.Width = (float)m_width;
        m_gpuViewport.Height = (float)m_height;
        m_gpuViewport.MinDepth = 0.0f;
        m_gpuViewport.MaxDepth = 1.0f;
        m_gpuViewport.TopLeftX = 0.0f;
        m_gpuViewport.TopLeftY = 0.0f;

        // Genuinely half-size viewport for the UV pass -- matches
        // m_gpuUVTex's real dimensions exactly. See the comment on
        // m_uvViewport in the header for why this (rather than reusing
        // m_gpuViewport for both passes) is the actual fix for the chroma
        // corruption.
        m_uvViewport = {};
        m_uvViewport.Width = (float)m_chromaWidth;
        m_uvViewport.Height = (float)m_chromaHeight;
        m_uvViewport.MinDepth = 0.0f;
        m_uvViewport.MaxDepth = 1.0f;
        m_uvViewport.TopLeftX = 0.0f;
        m_uvViewport.TopLeftY = 0.0f;

        // Explicit, known-good state -- see the member declarations' comment
        // for why this is needed (we don't control what state the shared
        // context was left in before our draws run).
        D3D11_RASTERIZER_DESC rasterDesc = {};
        rasterDesc.FillMode = D3D11_FILL_SOLID;
        rasterDesc.CullMode = D3D11_CULL_NONE;
        rasterDesc.FrontCounterClockwise = FALSE;
        rasterDesc.DepthClipEnable = TRUE;
        rasterDesc.ScissorEnable = FALSE;
        OK_OR_THROW(
            device->CreateRasterizerState(&rasterDesc, &m_rasterizerState),
            "VideoEncoderJpegXs: failed to create rasterizer state."
        );

        D3D11_BLEND_DESC blendDesc = {};
        blendDesc.RenderTarget[0].BlendEnable = FALSE;
        blendDesc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL;
        OK_OR_THROW(
            device->CreateBlendState(&blendDesc, &m_blendState),
            "VideoEncoderJpegXs: failed to create blend state."
        );

        D3D11_DEPTH_STENCIL_DESC depthDesc = {};
        depthDesc.DepthEnable = FALSE;
        depthDesc.StencilEnable = FALSE;
        OK_OR_THROW(
            device->CreateDepthStencilState(&depthDesc, &m_depthStencilState),
            "VideoEncoderJpegXs: failed to create depth-stencil state."
        );

        // TEMPORARY DIAGNOSTIC: fully synchronous smoke test -- Clear,
        // CopyResource, Flush, Map, all on this one thread, right here,
        // completely decoupled from the async submit/readback pipeline
        // (which spans three threads: whatever calls Transmit,
        // ReadbackWorkerLoop, and FrameRender's own rendering thread). Every
        // previous test (real render, then an explicit clear-before-draw)
        // came back as all zeros even though Map() itself reports success.
        // If this synchronous test ALSO reads back zero, the bug is in
        // basic resource/view identity or CopyResource, nothing to do with
        // threading. If this succeeds, the bug is specific to the async
        // multi-threaded path. Reuses m_stagingYTex[0], which Initialize()
        // (called before any Transmit/EnsureGpuPipeline call) has
        // already created. Remove once the green issue is confirmed fixed.
        {
            float testColor[4] = { 222.0f / 255.0f, 0.0f, 0.0f, 1.0f };
            context->ClearRenderTargetView(m_rtvY.Get(), testColor);
            context->CopyResource(m_stagingYTex[0].Get(), m_gpuYTex.Get());
            context->Flush();
            D3D11_MAPPED_SUBRESOURCE mapped;
            HRESULT hr = context->Map(m_stagingYTex[0].Get(), 0, D3D11_MAP_READ, 0, &mapped);
            if (SUCCEEDED(hr)) {
                uint8_t firstByte = *reinterpret_cast<const uint8_t*>(mapped.pData);
                Info(
                    "VideoEncoderJpegXs: SYNC SMOKE TEST: cleared Y to 222, synchronous "
                    "readback first byte = %u (RowPitch=%u)\n",
                    (unsigned)firstByte,
                    (unsigned)mapped.RowPitch
                );
                context->Unmap(m_stagingYTex[0].Get(), 0);
            } else {
                Info(
                    "VideoEncoderJpegXs: SYNC SMOKE TEST: Map() failed hr=0x%08lx\n",
                    (unsigned long)hr
                );
            }
        }

        m_gpuPipelineReady = true;
        Info(
            "VideoEncoderJpegXs: GPU RGB->YUV420 pipeline ready (%ux%u Y + %ux%u UV, separate "
            "plain render targets, own runtime-compiled shaders, own two-pass Y/UV renderer, "
            "correctly-sized viewport per pass).\n",
            m_width,
            m_height,
            m_chromaWidth,
            m_chromaHeight
        );
        return true;
    } catch (const std::exception& e) {
        Error(
            "VideoEncoderJpegXs: GPU pipeline setup failed, disabling JPEG XS test dump: %s\n",
            e.what()
        );
        m_gpuSetupFailed = true;
        return false;
    }
}

void VideoEncoderJpegXs::RenderYuvGpu(ID3D11DeviceContext* context) {
    // Pin down rasterizer/blend/depth state explicitly -- see the member
    // declarations' comment in the header. Without this, whatever state the
    // real path (or CEncoder/VideoEncoder submission in between) left the
    // shared context in applies to our draws too; a leftover scissor rect,
    // non-default cull mode, or restricted color write mask can silently
    // discard every pixel we try to write, leaving the output texture at its
    // all-zero creation-time content (which decodes to solid green).
    context->RSSetState(m_rasterizerState.Get());
    context->OMSetBlendState(m_blendState.Get(), nullptr, 0xffffffff);
    context->OMSetDepthStencilState(m_depthStencilState.Get(), 0);

    // Bind our own input layout + vertex buffer -- see the long comment on
    // the member declarations in the header. UPDATE: shader reflection
    // (D3DReflect) now proves QuadVertexShader.cso has exactly one input
    // parameter, SV_VertexID -- it *is* fully procedural after all, no
    // vertex buffer needed. Keeping this bound anyway is harmless (it's
    // simply unused by the shader), so leaving it in rather than ripping it
    // back out; just noting the earlier theory (FrameRender-inherited
    // vertex buffer) is now known to be wrong.
    context->IASetInputLayout(m_quadInputLayout.Get());
    ID3D11Buffer* vertexBuffers[] = { m_quadVertexBuffer.Get() };
    UINT stride = sizeof(float) * 4 + sizeof(float) * 2 + sizeof(uint32_t); // 28 bytes
    UINT offset = 0;
    context->IASetVertexBuffers(0, 1, vertexBuffers, &stride, &offset);
    context->IASetIndexBuffer(nullptr, DXGI_FORMAT_UNKNOWN, 0);

    context->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
    context->VSSetShader(m_quadVertexShader.Get(), nullptr, 0);
    context->PSSetConstantBuffers(0, 1, m_yuvParamBuffer.GetAddressOf());

    // Unbind ALL render targets before binding sourceTexture as an SRV. The
    // real cause of the green output: sourceTexture is ALVR's own compositor
    // output texture, which the main render path (FrameRender::RenderFrame)
    // leaves bound as an active render target on this same immediate context
    // -- it's never had a reason to unbind it before now, since nothing else
    // previously tried to read it as a shader input. D3D11's resource-hazard
    // tracker silently forces a shader-resource slot back to nullptr if the
    // same resource is simultaneously bound as a render target anywhere on
    // the context (no error, no exception -- this is why every prior
    // diagnostic came back clean). That's exactly what the pre-Draw
    // diagnostic below has been showing: srv=MISMATCH on every single frame,
    // everything else MATCH. Clearing render targets first breaks the
    // hazard.
    ID3D11RenderTargetView* clearRtv[] = { nullptr };
    context->OMSetRenderTargets(1, clearRtv, nullptr);

    ID3D11ShaderResourceView* srvs[] = { m_sourceSRV.Get() };
    context->PSSetShaderResources(0, 1, srvs);

    // Two single-target passes instead of one dual-target pass -- see the
    // long comment on the declaration of this method for why. Each pass now
    // uses its own pixel shader (mainY / mainUV) and its own, genuinely
    // correctly-sized viewport (m_gpuViewport for the full-res Y target,
    // m_uvViewport for the half-res UV target) -- see the comment on
    // m_yPixelShader in the header for why sharing one oversized viewport
    // between both passes was the actual bug.
    context->PSSetShader(m_yPixelShader.Get(), nullptr, 0);
    context->RSSetViewports(1, &m_gpuViewport);
    ID3D11RenderTargetView* rtvY[] = { m_rtvY.Get() };
    context->OMSetRenderTargets(1, rtvY, nullptr);
    context->Draw(4, 0);

    context->PSSetShader(m_uvPixelShader.Get(), nullptr, 0);
    context->RSSetViewports(1, &m_uvViewport);
    ID3D11RenderTargetView* rtvUV[] = { m_rtvUV.Get() };
    context->OMSetRenderTargets(1, rtvUV, nullptr);
    context->Draw(4, 0);

    // Leave render targets unbound afterwards rather than pointing at our
    // own textures -- the real path sets its own targets before it next
    // needs them, but there's no reason to leave stale state bound in the
    // meantime. Same reasoning for restoring rasterizer/blend/depth state to
    // D3D11 defaults (nullptr = default state) rather than leaving our own
    // (e.g. CullMode=NONE) bound -- non-invasive in both directions.
    ID3D11RenderTargetView* none[] = { nullptr };
    context->OMSetRenderTargets(1, none, nullptr);
    context->RSSetState(nullptr);
    context->OMSetBlendState(nullptr, nullptr, 0xffffffff);
    context->OMSetDepthStencilState(nullptr, 0);
}

void VideoEncoderJpegXs::Transmit(
    ID3D11Texture2D* sourceTexture,
    uint64_t /*presentationTime*/,
    uint64_t targetTimestampNs,
    bool /*insertIDR*/
) {
    if (!m_encInitialized || sourceTexture == nullptr) {
        return;
    }
    if (!EnsureGpuPipeline(sourceTexture)) {
        return; // GPU pipeline unavailable; skip this (and all future) frames.
    }

    auto t0 = std::chrono::steady_clock::now();
    auto* context = m_d3dRender->GetContext();

    int writeIndex = m_stagingWriteIndex;
    int readIndex = 1 - m_stagingWriteIndex;

    // If the slot we're about to overwrite is still reserved (readback
    // hasn't Map()'d it yet), skip this frame entirely rather than
    // overwriting GPU data mid-readback. With the mailbox limiting us to one
    // outstanding pending frame, this should be rare in steady state -- it
    // means readback has fallen behind.
    bool writeSlotReserved;
    {
        std::lock_guard<std::mutex> lock(m_mutex);
        writeSlotReserved = m_stagingReserved[writeIndex];
    }
    if (writeSlotReserved) {
        auto t1 = std::chrono::steady_clock::now();
        LogPeriod(
            "VideoEncoderJpegXs.Submit",
            "submit skipped, staging slot %d still reserved by readback = %.2fms",
            writeIndex,
            std::chrono::duration<double, std::milli>(t1 - t0).count()
        );
        return;
    }

    // GPU RGB->YUV420 conversion: a couple of cheap fullscreen-quad draw
    // calls (reusing ALVR's own already-compiled rgbtoyuv420.hlsl pipeline)
    // into our own m_gpuYTex/m_gpuUVTex, followed by two CopyResource calls
    // into this frame's staging slot. The actual GPU *wait* (Map()) still
    // happens on ReadbackWorkerLoop, never here -- but the synchronous
    // smoke test in EnsureGpuPipeline() (Clear+Copy+Flush+Map, all on this
    // same thread) proved the underlying mechanism works, while the exact
    // same sequence *without* Flush(), read back later from a *different*
    // thread (ReadbackWorkerLoop), consistently came back all-zero. Without
    // an explicit Flush(), the driver is free to leave these commands
    // sitting recorded-but-unsubmitted indefinitely; Map()'s implicit
    // wait-for-GPU is apparently not enough to guarantee that on its own
    // when the Map() happens on a different thread than the one that
    // recorded the copy. Flush() here forces submission right away, so by
    // the time ReadbackWorkerLoop's Map() call runs (up to a frame later),
    // the GPU has unambiguously already started (if not finished) the work.
    RenderYuvGpu(context);
    context->CopyResource(m_stagingYTex[writeIndex].Get(), m_gpuYTex.Get());
    context->CopyResource(m_stagingUVTex[writeIndex].Get(), m_gpuUVTex.Get());
    context->Flush();
    m_stagingTimestampNs[writeIndex] = targetTimestampNs;
    m_stagingHasData[writeIndex] = true;

    if (m_stagingHasData[readIndex]) {
        std::lock_guard<std::mutex> lock(m_mutex);
        if (!m_hasPendingFrame) {
            m_pendingStagingIndex = readIndex;
            m_pendingTimestampNs = m_stagingTimestampNs[readIndex];
            m_hasPendingFrame = true;
            m_stagingReserved[readIndex] = true;
            m_cv.notify_one();
        }
    }

    m_stagingWriteIndex = readIndex; // toggle: next call writes the other slot

    auto t1 = std::chrono::steady_clock::now();
    LogPeriod(
        "VideoEncoderJpegXs.Submit",
        "submit (GPU render + CopyResource enqueue, no GPU wait here) = %.2fms",
        std::chrono::duration<double, std::milli>(t1 - t0).count()
    );
}

void VideoEncoderJpegXs::ReadbackWorkerLoop() {
    while (true) {
        uint64_t timestampNs = 0;
        int stagingIndex = -1;

        // Stage 1 input: dequeue the staging-slot index Transmit handed
        // off (no pixel data crosses this mailbox -- see header comment).
        {
            std::unique_lock<std::mutex> lock(m_mutex);
            m_cv.wait(lock, [this] {
                return m_hasPendingFrame || m_exiting.load(std::memory_order_acquire);
            });
            if (!m_hasPendingFrame && m_exiting.load(std::memory_order_acquire)) {
                break;
            }
            stagingIndex = m_pendingStagingIndex;
            timestampNs = m_pendingTimestampNs;
            m_hasPendingFrame = false;
        }

        auto tReadback0 = std::chrono::steady_clock::now();

        // GPU readback: Map() blocks until the earlier render+CopyResource
        // have landed. Two separate, ordinary staging textures now (not one
        // NV12 texture's two subresources -- see the top-of-file comment),
        // both mapped at subresource 0. UV is still interleaved (R8G8) and
        // gets deinterleaved into the separate Cb/Cr planes SVT-JPEG-XS
        // wants. Landing in our own private scratch buffers here (not
        // directly in the yuv mailbox) lets us release the GPU staging slot
        // immediately below, instead of holding it reserved for however long
        // encode takes to free up the next mailbox slot.
        {
            auto* context = m_d3dRender->GetContext();

            D3D11_MAPPED_SUBRESOURCE mappedY;
            HRESULT hrY = context->Map(
                m_stagingYTex[stagingIndex].Get(), 0, D3D11_MAP_READ, 0, &mappedY
            );
            if (SUCCEEDED(hrY)) {
                const uint8_t* src = reinterpret_cast<const uint8_t*>(mappedY.pData);
                for (uint32_t y = 0; y < m_height; y++) {
                    memcpy(
                        m_readbackScratchY.data() + (size_t)y * m_width,
                        src + (size_t)y * mappedY.RowPitch,
                        m_width
                    );
                }
                context->Unmap(m_stagingYTex[stagingIndex].Get(), 0);
            } else {
                // TEMPORARY DIAGNOSTIC: this was previously failing silently
                // -- if Map() fails, the scratch buffer (zero-initialized at
                // Initialize() time) never gets touched again, which reads
                // as a permanent, deterministic all-zero plane forever. Log
                // the actual HRESULT plus device-removed status so we can
                // tell a Map() failure apart from "Map() succeeded but the
                // GPU genuinely produced zero data".
                HRESULT removedReason = m_d3dRender->GetDevice()->GetDeviceRemovedReason();
                LogPeriod(
                    "VideoEncoderJpegXs.Diag",
                    "Y staging Map() FAILED hr=0x%08lx, GetDeviceRemovedReason=0x%08lx",
                    (unsigned long)hrY,
                    (unsigned long)removedReason
                );
            }

            D3D11_MAPPED_SUBRESOURCE mappedUV;
            HRESULT hrUV = context->Map(
                m_stagingUVTex[stagingIndex].Get(), 0, D3D11_MAP_READ, 0, &mappedUV
            );
            if (SUCCEEDED(hrUV)) {
                const uint8_t* src = reinterpret_cast<const uint8_t*>(mappedUV.pData);
                for (uint32_t y = 0; y < m_chromaHeight; y++) {
                    const uint8_t* rowSrc = src + (size_t)y * mappedUV.RowPitch;
                    uint8_t* rowCb = m_readbackScratchCb.data() + (size_t)y * m_chromaWidth;
                    uint8_t* rowCr = m_readbackScratchCr.data() + (size_t)y * m_chromaWidth;
                    for (uint32_t x = 0; x < m_chromaWidth; x++) {
                        rowCb[x] = rowSrc[x * 2 + 0];
                        rowCr[x] = rowSrc[x * 2 + 1];
                    }
                }
                context->Unmap(m_stagingUVTex[stagingIndex].Get(), 0);
            } else {
                LogPeriod(
                    "VideoEncoderJpegXs.Diag", "UV staging Map() FAILED hr=0x%08lx", (unsigned long)hrUV
                );
            }
        }
        {
            std::lock_guard<std::mutex> lock(m_mutex);
            m_stagingReserved[stagingIndex] = false;
        }

        // TEMPORARY DIAGNOSTIC: min/max of each plane, throttled (LogPeriod
        // self-limits per tag, safe to call every frame). This tells us,
        // with actual numbers instead of guessing, whether the GPU render
        // is genuinely producing flat/zero data (render-side bug, keep
        // digging there) or real varying data that just happens to decode
        // green downstream (SVT-JPEG-XS params, muxing, or player-side
        // issue instead). Remove once the green issue is confirmed fixed.
        {
            uint8_t yMin = 255, yMax = 0, cbMin = 255, cbMax = 0, crMin = 255, crMax = 0;
            for (uint8_t v : m_readbackScratchY) {
                yMin = std::min(yMin, v);
                yMax = std::max(yMax, v);
            }
            for (uint8_t v : m_readbackScratchCb) {
                cbMin = std::min(cbMin, v);
                cbMax = std::max(cbMax, v);
            }
            for (uint8_t v : m_readbackScratchCr) {
                crMin = std::min(crMin, v);
                crMax = std::max(crMax, v);
            }
            LogPeriod(
                "VideoEncoderJpegXs.Diag",
                "readback planes: Y[%u..%u] Cb[%u..%u] Cr[%u..%u]",
                yMin,
                yMax,
                cbMin,
                cbMax,
                crMin,
                crMax
            );
        }

        auto tReadback1 = std::chrono::steady_clock::now();
        LogPeriod(
            "VideoEncoderJpegXs.Readback",
            "readback (Map Y+UV, deinterleave, own thread) = %.2fms",
            std::chrono::duration<double, std::milli>(tReadback1 - tReadback0).count()
        );

        // Handoff to encode: wait until it has picked up whatever we gave it
        // last time, so we never get more than one frame ahead of it. This
        // is what lets readback(N+1) genuinely overlap with encode(N)
        // instead of racing ahead unboundedly.
        {
            std::unique_lock<std::mutex> lock(m_yuvMutex);
            m_yuvCv.wait(lock, [this] {
                return !m_yuvReady || m_exiting.load(std::memory_order_acquire);
            });
            if (m_exiting.load(std::memory_order_acquire)) {
                break;
            }
            m_yuvMailboxY.swap(m_readbackScratchY);
            m_yuvMailboxCb.swap(m_readbackScratchCb);
            m_yuvMailboxCr.swap(m_readbackScratchCr);
            m_yuvTimestampNs = timestampNs;
            m_yuvReady = true;
        }
        m_yuvCv.notify_one();
    }
}

void VideoEncoderJpegXs::EncodeWorkerLoop() {
    while (true) {
        uint64_t timestampNs = 0;

        {
            std::unique_lock<std::mutex> lock(m_yuvMutex);
            m_yuvCv.wait(lock, [this] {
                return m_yuvReady || m_exiting.load(std::memory_order_acquire);
            });
            if (!m_yuvReady && m_exiting.load(std::memory_order_acquire)) {
                break;
            }
            m_planeY.swap(m_yuvMailboxY);
            m_planeCb.swap(m_yuvMailboxCb);
            m_planeCr.swap(m_yuvMailboxCr);
            timestampNs = m_yuvTimestampNs;
            m_yuvReady = false;
        }
        // Wake the readback stage immediately: the mailbox slot is free
        // again, it can hand off the next frame while we encode this one.
        m_yuvCv.notify_one();

        m_encodeBusy.store(true, std::memory_order_release);
        auto t0 = std::chrono::steady_clock::now();

        svt_jpeg_xs_image_buffer_t img = {};
        img.data_yuv[0] = m_planeY.data();
        img.data_yuv[1] = m_planeCb.data();
        img.data_yuv[2] = m_planeCr.data();
        img.stride[0] = m_width;
        img.stride[1] = m_chromaWidth;
        img.stride[2] = m_chromaWidth;
        img.alloc_size[0] = (uint32_t)m_planeY.size();
        img.alloc_size[1] = (uint32_t)m_planeCb.size();
        img.alloc_size[2] = (uint32_t)m_planeCr.size();

        svt_jpeg_xs_bitstream_buffer_t bitstream = {};
        bitstream.buffer = m_bitstreamBuf.data();
        bitstream.allocation_size = (uint32_t)m_bitstreamBuf.size();
        bitstream.used_size = 0;

        svt_jpeg_xs_frame_t frame = {};
        frame.image = img;
        frame.bitstream = bitstream;
        frame.user_prv_ctx_ptr = nullptr;

        SvtJxsErrorType_t err = svt_jpeg_xs_encoder_send_picture(&m_enc, &frame, 1 /*blocking*/);
        if (err != SvtJxsErrorNone) {
            m_encodeBusy.store(false, std::memory_order_release);
            continue;
        }

        svt_jpeg_xs_frame_t out = {};
        err = svt_jpeg_xs_encoder_get_packet(&m_enc, &out, 1 /*blocking*/);
        if (err != SvtJxsErrorNone) {
            m_encodeBusy.store(false, std::memory_order_release);
            continue;
        }

        auto t1 = std::chrono::steady_clock::now();
        LogPeriod(
            "VideoEncoderJpegXs.Encode",
            "encode=%.2fms",
            std::chrono::duration<double, std::milli>(t1 - t0).count()
        );

        if (out.bitstream.used_size > 0) {
            if (m_streamDumper) {
                m_streamDumper->WritePacket(out.bitstream.buffer, out.bitstream.used_size);
                // JPEG XS is intra-only: every frame is effectively a keyframe.
                m_streamDumper->OnFrameEncoded(out.bitstream.used_size, true);
            }

            // Real network send -- the same FFI calls H264/HEVC/AV1 use
            // (bindings.h), called directly rather than through
            // ParseFrameNals()/NalParsing.cpp: JPEG XS has no NAL-unit
            // structure at all, so there's nothing for that NAL scanner to
            // do here. SetVideoConfigNals() is sent once, empty -- JPEG
            // XS's codestream is self-describing per frame (like AV1's own
            // in-band sequence header), so there's no persistent SPS/PPS-
            // equivalent parameter set to extract up front.
            if (!m_sentConfig) {
                m_sentConfig = true;
                SetVideoConfigNals(nullptr, 0, ALVR_CODEC_JPEGXS);
            }
            VideoSend(timestampNs, out.bitstream.buffer, (int)out.bitstream.used_size, /*isIdr=*/true);
        }

        m_encodeBusy.store(false, std::memory_order_release);
    }
}

void VideoEncoderJpegXs::Shutdown() {
    if (m_exiting.exchange(true)) {
        return; // already shut down
    }
    m_cv.notify_all();
    m_yuvCv.notify_all();
    if (m_readbackThread.joinable()) {
        m_readbackThread.join();
    }
    if (m_encodeThread.joinable()) {
        m_encodeThread.join();
    }

    if (m_encInitialized) {
        svt_jpeg_xs_encoder_close(&m_enc);
        m_encInitialized = false;
    }

    if (m_streamDumper) {
        m_streamDumper->Close();
    }
}

#endif // ALVR_JPEGXS
