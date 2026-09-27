#pragma once

#include "ALVR-common/packet_types.h"
#include <string>

class Settings {
    static Settings m_Instance;
    bool m_loaded;

    Settings();
    virtual ~Settings();

public:
    void Load();
    static Settings& Instance() { return m_Instance; }

    bool IsLoaded() { return m_loaded; }

    int m_refreshRate;
    uint32_t m_renderWidth;
    uint32_t m_renderHeight;
    int32_t m_recommendedTargetWidth;
    int32_t m_recommendedTargetHeight;
    int32_t m_nAdapterIndex;
    std::string m_captureFrameDir;

    bool m_enableFoveatedEncoding;
    float m_foveationCenterSizeX;
    float m_foveationCenterSizeY;
    float m_foveationCenterShiftX;
    float m_foveationCenterShiftY;
    float m_foveationEdgeRatioX;
    float m_foveationEdgeRatioY;

    bool m_enableColorCorrection;
    float m_brightness;
    float m_contrast;
    float m_saturation;
    float m_gamma;
    float m_sharpening;

    int m_codec;
    int m_h264Profile;
    bool m_use10bitEncoder;
    bool m_useFullRangeEncoding;
    double m_encodingGamma;
    bool m_enableHdr;
    bool m_forceHdrSrgbCorrection;
    bool m_clampHdrExtendedRange;
    bool m_enableAmfPreAnalysis;
    bool m_enableVbaq;
    bool m_enableAmfHmqb;
    bool m_useAmfPreproc;
    uint32_t m_amfPreProcSigma;
    uint32_t m_amfPreProcTor;
    uint32_t m_encoderQualityPreset;
    bool m_amdBitrateCorruptionFix;
    uint32_t m_nvencQualityPreset;
    uint32_t m_rateControlMode;
    bool m_fillerData;
    uint32_t m_entropyCoding;
    bool m_force_sw_encoding;
    uint32_t m_swThreadCount;

    // JPEG XS encoder settings (see platform/win32/VideoEncoderJpegXs.h).
    // Only consumed when compiled with ALVR_JPEGXS and m_codec ==
    // ALVR_CODEC_JPEGXS. Codec selection itself lives in m_codec, not a
    // separate enable flag -- see CEncoder::Initialize()'s dispatch.
    float m_jpegXsBitsPerPixel;
    uint32_t m_jpegXsThreadCount;
    bool m_jpegXsDumpToDisk;
    // Gates VideoEncoderNVENC.cpp/AMF.cpp's own StreamDumper (dumps
    // whichever of H264/HEVC/AV1 is actually selected to disk) -- a
    // general "dump the real stream" toggle, unconditionally checked by
    // both classes' own Initialize(), so it still works normally whenever
    // one of them is the active encoder. Only inert specifically when
    // JPEG XS is the selected codec, since AMF/NVENC aren't constructed at
    // all in that case.
    bool m_jpegXsDumpReferenceStream;
    std::string m_jpegXsOutputDirectory;

    uint32_t m_nvencTuningPreset;
    uint32_t m_nvencMultiPass;
    uint32_t m_nvencAdaptiveQuantizationMode;
    int64_t m_nvencLowDelayKeyFrameScale;
    int64_t m_nvencRefreshRate;
    bool m_nvencEnableIntraRefresh;
    int64_t m_nvencIntraRefreshPeriod;
    int64_t m_nvencIntraRefreshCount;
    int64_t m_nvencMaxNumRefFrames;
    int64_t m_nvencGopLength;
    int64_t m_nvencPFrameStrategy;
    int64_t m_nvencRateControlMode;
    int64_t m_nvencRcBufferSize;
    int64_t m_nvencRcInitialDelay;
    int64_t m_nvencRcMaxBitrate;
    int64_t m_nvencRcAverageBitrate;
    bool m_nvencEnableWeightedPrediction;

    uint64_t m_minimumIdrIntervalMs;

    bool m_enableViveTrackerProxy = false;
    bool m_TrackingRefOnly = false;
    bool m_enableLinuxVulkanAsyncCompute;
    bool m_enableLinuxAsyncReprojection;

    bool m_enableControllers;
    int m_controllerIsTracker = false;
    int m_enableBodyTrackingFakeVive = false;
    int m_bodyTrackingHasLegs = false;
    bool m_useSeparateHandTrackers = false;
};
