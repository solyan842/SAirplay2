#include <cstdint>
#include <cstdlib>
#include <cstring>

#include "ALACEncoder.h"
#include "ALACBitUtilities.h"

#if defined(_WIN32)
#define SAIRPLAY_EXPORT __declspec(dllexport)
#else
#define SAIRPLAY_EXPORT
#endif

struct sairplay_alac24_encoder {
    AudioFormatDescription input_format;
    AudioFormatDescription output_format;
    ALACEncoder* encoder;
};

extern "C" SAIRPLAY_EXPORT void* sairplay_alac24_create(int sample_rate) {
    if (sample_rate != 44100 && sample_rate != 48000) {
        return nullptr;
    }

    auto* state = static_cast<sairplay_alac24_encoder*>(
        std::calloc(1, sizeof(sairplay_alac24_encoder))
    );
    if (!state) {
        return nullptr;
    }

    state->encoder = new (std::nothrow) ALACEncoder();
    if (!state->encoder) {
        std::free(state);
        return nullptr;
    }

    // Mirrors music-assistant/airplay-cli src/alac_ext.cpp exactly for
    // native 24-bit stereo ALAC. The Rust side has already converted
    // s32le carriers to packed s24le before this boundary.
    state->input_format.mFormatID = kALACFormatLinearPCM;
    state->input_format.mSampleRate = sample_rate;
    state->input_format.mBitsPerChannel = 24;
    state->input_format.mFramesPerPacket = 1;
    state->input_format.mChannelsPerFrame = 2;
    state->input_format.mBytesPerFrame = 6;
    state->input_format.mBytesPerPacket = 6;
    state->input_format.mFormatFlags =
        kALACFormatFlagsNativeEndian | kALACFormatFlagIsSignedInteger;
    state->input_format.mReserved = 0;

    state->output_format.mFormatID = kALACFormatAppleLossless;
    state->output_format.mSampleRate = sample_rate;
    state->output_format.mFramesPerPacket = 352;
    state->output_format.mChannelsPerFrame = 2;
    state->output_format.mBytesPerPacket = 0;
    state->output_format.mBytesPerFrame = 0;
    state->output_format.mBitsPerChannel = 0;
    state->output_format.mFormatFlags = 3; // 24-bit, source alac_ext.cpp contract
    state->output_format.mReserved = 0;

    state->encoder->SetFrameSize(352);
    state->encoder->SetFastMode(false);
    if (state->encoder->InitializeEncoder(state->output_format) != ALAC_noErr) {
        delete state->encoder;
        std::free(state);
        return nullptr;
    }

    return state;
}

extern "C" SAIRPLAY_EXPORT int sairplay_alac24_encode_352(
    void* opaque,
    const uint8_t* packed_s24le,
    int input_bytes,
    uint8_t* output,
    int output_capacity
) {
    if (!opaque || !packed_s24le || !output) {
        return -1;
    }
    if (input_bytes != 352 * 2 * 3 || output_capacity < 8192) {
        return -2;
    }

    auto* state = static_cast<sairplay_alac24_encoder*>(opaque);
    int32_t io_bytes = input_bytes;
    const int32_t status = state->encoder->Encode(
        state->input_format,
        state->output_format,
        const_cast<uint8_t*>(packed_s24le),
        output,
        &io_bytes
    );
    if (status != ALAC_noErr) {
        return -3;
    }
    if (io_bytes <= 0 || io_bytes > output_capacity) {
        return -4;
    }
    return io_bytes;
}

extern "C" SAIRPLAY_EXPORT void sairplay_alac24_destroy(void* opaque) {
    if (!opaque) {
        return;
    }
    auto* state = static_cast<sairplay_alac24_encoder*>(opaque);
    delete state->encoder;
    std::free(state);
}
