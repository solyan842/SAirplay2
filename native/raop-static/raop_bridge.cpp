// SAirplay2 in-process RAOP bridge.
// Transport source of truth: music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128
// libraop pin: 81c2182649da8645ac2a58b78e9f370c79a4165b
//
// This file is intentionally only a C ABI adapter around pinned libraop. It
// does not implement a second RAOP protocol contract.

#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <atomic>
#include <vector>

#include "platform.h"
#if WIN
#include <windows.h>
#endif
#include "raop_client.h"
#include "cross_net.h"
#include "cross_ssl.h"
#include "cross_log.h"

log_level util_loglevel;
log_level raop_loglevel;

static const uint32_t SR_FRAMES_PER_CHUNK = 352;
static const uint64_t SR_START_LEAD_MS = 200;

struct sr_raop_ready {
    uint32_t latency_frames;
    uint32_t sample_rate;
    uint16_t bit_depth;
    uint16_t channels;
};

struct sr_raop_handle {
    struct raopcl_s *raop;
    uint16_t bit_depth;
    uint16_t channels;
    std::atomic<uint64_t> head_audible_ms;
};

static void set_error(char *dst, size_t cap, const char *text) {
    if (!dst || cap == 0) return;
    if (!text) text = "unknown";
#if WIN
    strncpy_s(dst, cap, text, _TRUNCATE);
#else
    snprintf(dst, cap, "%s", text);
#endif
}

static uint64_t unix_now_ms(void) {
#if WIN
    FILETIME ft;
    ULARGE_INTEGER ticks;
    const uint64_t unix_epoch_filetime = 116444736000000000ULL;
    GetSystemTimeAsFileTime(&ft);
    ticks.LowPart = ft.dwLowDateTime;
    ticks.HighPart = ft.dwHighDateTime;
    if (ticks.QuadPart <= unix_epoch_filetime) return 0;
    return (ticks.QuadPart - unix_epoch_filetime) / 10000ULL;
#else
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (uint64_t)tv.tv_sec * 1000ULL + (uint64_t)tv.tv_usec / 1000ULL;
#endif
}

static uint64_t ms_to_source_delta(uint64_t ms) {
    return ((ms / 1000ULL) << 32) | (((ms % 1000ULL) << 32) / 1000ULL);
}

static uint64_t source_delta_to_ms(uint64_t delta) {
    return (delta >> 32) * 1000ULL
        + (((delta & 0xffffffffULL) * 1000ULL) >> 32);
}

static uint64_t source_ntp_to_unix_ms(uint64_t source_ntp) {
    uint64_t source_now = raopcl_get_ntp(NULL);
    uint64_t unix_now = unix_now_ms();
    if (source_ntp >= source_now) {
        return unix_now + source_delta_to_ms(source_ntp - source_now);
    }
    uint64_t back = source_delta_to_ms(source_now - source_ntp);
    return unix_now > back ? unix_now - back : 0;
}

static uint64_t resolve_start(uint64_t requested_ms, uint64_t *at_ms) {
    uint64_t source_now = raopcl_get_ntp(NULL);
    uint64_t unix_now = unix_now_ms();
    if (requested_ms && requested_ms >= unix_now + SR_START_LEAD_MS) {
        *at_ms = requested_ms;
        return source_now + ms_to_source_delta(requested_ms - unix_now);
    }
    uint64_t corrected = requested_ms ? SR_START_LEAD_MS * 2ULL : SR_START_LEAD_MS;
    *at_ms = unix_now + corrected;
    return source_now + ms_to_source_delta(corrected);
}

static bool commit_start(struct raopcl_s *p, uint64_t requested_ms, uint64_t *at_ms) {
    raop_state_t state = raopcl_state(p);
    if (state != RAOP_STREAMING && state != RAOP_FLUSHED) return false;
    uint64_t audible = resolve_start(requested_ms, at_ms);
    raopcl_stop(p);
    if (state == RAOP_STREAMING && !raopcl_flush(p)) return false;
    uint64_t latency = TS2NTP(raopcl_latency(p), raopcl_sample_rate(p));
    return raopcl_start_at(p, audible - latency);
}

static bool start_after_flush(struct raopcl_s *p, uint64_t requested_ms, uint64_t *at_ms) {
    if (raopcl_state(p) != RAOP_FLUSHED) return false;
    uint64_t audible = resolve_start(requested_ms, at_ms);
    uint64_t latency = TS2NTP(raopcl_latency(p), raopcl_sample_rate(p));
    return raopcl_start_at(p, audible - latency);
}

static bool flush_session(struct raopcl_s *p) {
    raop_state_t state = raopcl_state(p);
    if (state != RAOP_STREAMING && state != RAOP_FLUSHED) return false;
    raopcl_stop(p);
    return state == RAOP_FLUSHED ? true : raopcl_flush(p);
}

static bool pause_session(struct raopcl_s *p) {
    raop_state_t state = raopcl_state(p);
    if (state == RAOP_FLUSHED) return true;
    if (state != RAOP_STREAMING) return false;
    raopcl_pause(p);
    return raopcl_flush(p);
}

static bool play_session(struct raopcl_s *p) {
    raop_state_t state = raopcl_state(p);
    if (state != RAOP_FLUSHED && state != RAOP_STREAMING) return false;
    uint64_t audible = raopcl_get_ntp(NULL) + MS2NTP(SR_START_LEAD_MS);
    uint64_t latency = TS2NTP(raopcl_latency(p), raopcl_sample_rate(p));
    return raopcl_start_at(p, audible - latency);
}

static void pack_32_to_24(const uint8_t *in, size_t in_bytes, std::vector<uint8_t> &out) {
    size_t samples = in_bytes / 4;
    out.resize(samples * 3);
    for (size_t i = 0; i < samples; ++i) {
        out[i * 3 + 0] = in[i * 4 + 1];
        out[i * 3 + 1] = in[i * 4 + 2];
        out[i * 3 + 2] = in[i * 4 + 3];
    }
}

extern "C" {

int sr_raop_open(
    const char *host_name,
    uint16_t port,
    uint8_t volume,
    uint32_t lead_ms,
    uint32_t sample_rate,
    uint16_t bit_depth,
    uint16_t channels,
    int compressed_alac,
    int auth,
    int encrypt,
    const char *secret,
    const char *password,
    const char *et,
    const char *md,
    const char *dacp_id,
    const char *active_remote,
    const char *bind_ip,
    sr_raop_handle **out_handle,
    sr_raop_ready *out_ready,
    char *error,
    size_t error_cap
) {
    if (!host_name || !out_handle || !out_ready) {
        set_error(error, error_cap, "invalid RAOP bridge arguments");
        return 1;
    }
    *out_handle = NULL;

    if (channels == 0 || (bit_depth != 16 && bit_depth != 24)
        || (sample_rate != 44100 && sample_rate != 48000)) {
        set_error(error, error_cap, "unsupported RAOP format");
        return 2;
    }

    if (netsock_init() != 0) {
        set_error(error, error_cap, "netsock_init failed");
        return 3;
    }
    if (!cross_ssl_load()) {
        netsock_close();
        set_error(error, error_cap, "cross_ssl_load failed");
        return 4;
    }

    util_loglevel = lERROR;
    raop_loglevel = lINFO;

    struct hostent *he = gethostbyname(host_name);
    if (!he) {
        cross_ssl_free();
        netsock_close();
        set_error(error, error_cap, "cannot resolve RAOP receiver");
        return 5;
    }

    struct in_addr player = {0}, local = {0};
    memcpy(&player.s_addr, he->h_addr_list[0], he->h_length);
    if (bind_ip && *bind_ip && inet_pton(AF_INET, bind_ip, &local) != 1) {
        cross_ssl_free();
        netsock_close();
        set_error(error, error_cap, "invalid RAOP bind address");
        return 6;
    }

    int latency = (int)MS2TS(lead_ms, sample_rate);
    bool rsa_allowed = encrypt && et && strchr(et, '1');
    raop_crypto_t crypto = rsa_allowed ? RAOP_RSA : RAOP_CLEAR;
    struct raopcl_s *p = raopcl_create(
        local, 0, 0,
        const_cast<char *>(dacp_id && *dacp_id ? dacp_id : "1A2B3D4EA1B2C3D4"),
        const_cast<char *>(active_remote && *active_remote ? active_remote : "0"),
        compressed_alac ? RAOP_ALAC : RAOP_ALAC_RAW,
        SR_FRAMES_PER_CHUNK, latency, crypto, auth != 0,
        const_cast<char *>(secret ? secret : ""),
        const_cast<char *>(password && *password ? password : NULL),
        const_cast<char *>(et && *et ? et : "0,4"),
        const_cast<char *>(md && *md ? md : "0,1,2"),
        (int)sample_rate, (int)bit_depth, (int)channels,
        volume > 0 ? raopcl_float_volume((int)volume) : -144.0f
    );
    if (!p) {
        cross_ssl_free();
        netsock_close();
        set_error(error, error_cap, "raopcl_create failed");
        return 7;
    }
    if (!raopcl_connect(p, player, port, volume > 0)) {
        raopcl_destroy(p);
        cross_ssl_free();
        netsock_close();
        set_error(error, error_cap, "raopcl_connect failed");
        return 8;
    }

    sr_raop_handle *handle = new (std::nothrow) sr_raop_handle();
    if (!handle) {
        raopcl_disconnect(p);
        raopcl_destroy(p);
        cross_ssl_free();
        netsock_close();
        set_error(error, error_cap, "RAOP bridge allocation failed");
        return 9;
    }
    handle->raop = p;
    handle->bit_depth = bit_depth;
    handle->channels = channels;
    handle->head_audible_ms.store(0);

    out_ready->latency_frames = raopcl_latency(p);
    out_ready->sample_rate = raopcl_sample_rate(p);
    out_ready->bit_depth = bit_depth;
    out_ready->channels = channels;
    *out_handle = handle;
    return 0;
}

void sr_raop_close(sr_raop_handle *handle) {
    if (!handle) return;
    if (handle->raop) {
        raopcl_disconnect(handle->raop);
        raopcl_destroy(handle->raop);
        handle->raop = NULL;
    }
    delete handle;
    cross_ssl_free();
    netsock_close();
}

int sr_raop_healthy(sr_raop_handle *handle) {
    return handle && handle->raop
        && raopcl_is_connected(handle->raop)
        && raopcl_is_sane(handle->raop);
}

int sr_raop_keepalive(sr_raop_handle *handle) {
    return handle && handle->raop && raopcl_keepalive(handle->raop);
}

int sr_raop_commit_start(sr_raop_handle *handle, uint64_t requested_ms, uint64_t *at_ms) {
    return handle && handle->raop && at_ms && commit_start(handle->raop, requested_ms, at_ms);
}

int sr_raop_start_after_flush(sr_raop_handle *handle, uint64_t requested_ms, uint64_t *at_ms) {
    return handle && handle->raop && at_ms && start_after_flush(handle->raop, requested_ms, at_ms);
}

int sr_raop_flush(sr_raop_handle *handle) {
    return handle && handle->raop && flush_session(handle->raop);
}

int sr_raop_standby(sr_raop_handle *handle) {
    return sr_raop_flush(handle);
}

int sr_raop_pause(sr_raop_handle *handle) {
    return handle && handle->raop && pause_session(handle->raop);
}

int sr_raop_play(sr_raop_handle *handle) {
    return handle && handle->raop && play_session(handle->raop);
}

int sr_raop_stop(sr_raop_handle *handle) {
    if (!handle || !handle->raop) return 0;
    raopcl_stop(handle->raop);
    handle->head_audible_ms.store(0);
    return 1;
}

int sr_raop_set_volume(sr_raop_handle *handle, uint8_t percent) {
    if (!handle || !handle->raop) return 0;
    if (percent > 100) percent = 100;
    return raopcl_set_volume(handle->raop, raopcl_float_volume((int)percent));
}

int sr_raop_set_progress(sr_raop_handle *handle, uint32_t elapsed_s, uint32_t duration_s) {
    return handle && handle->raop
        && raopcl_set_progress_ms(handle->raop, elapsed_s * 1000U, duration_s * 1000U);
}

int sr_raop_set_metadata(sr_raop_handle *handle, const char *title, const char *artist, const char *album) {
    if (!handle || !handle->raop) return 0;
    return raopcl_set_daap(handle->raop, 4,
        "minm", 's', title ? title : "",
        "asar", 's', artist ? artist : "",
        "asal", 's', album ? album : "",
        "astn", 'i', 1);
}

int sr_raop_set_artwork(sr_raop_handle *handle, const char *content_type, const uint8_t *data, size_t size) {
    if (!handle || !handle->raop || !content_type || (size && !data) || size > INT_MAX) return 0;
    return raopcl_set_artwork(handle->raop, const_cast<char *>(content_type), (int)size,
                              const_cast<char *>(reinterpret_cast<const char *>(data)));
}

int sr_raop_write(sr_raop_handle *handle, const uint8_t *packet, size_t packet_bytes) {
    if (!handle || !handle->raop || !packet) return 0;
    size_t input_bpf = (handle->bit_depth <= 16 ? 2U : 4U) * handle->channels;
    size_t expected = SR_FRAMES_PER_CHUNK * input_bpf;
    if (packet_bytes != expected) return 0;

    for (;;) {
        if (!sr_raop_healthy(handle)) return 0;
        if (raopcl_state(handle->raop) != RAOP_STREAMING) return 0;
        if (raopcl_accept_frames(handle->raop)) break;
#if WIN
        Sleep(1);
#endif
    }

    const uint8_t *send_buf = packet;
    std::vector<uint8_t> packed;
    if (handle->bit_depth > 16) {
        pack_32_to_24(packet, packet_bytes, packed);
        send_buf = packed.data();
    }

    uint64_t playtime = 0;
    if (!raopcl_send_chunk(handle->raop, const_cast<uint8_t *>(send_buf),
                           SR_FRAMES_PER_CHUNK, &playtime)) {
        return 0;
    }
    uint64_t head = playtime + TS2NTP(SR_FRAMES_PER_CHUNK, raopcl_sample_rate(handle->raop));
    handle->head_audible_ms.store(source_ntp_to_unix_ms(head));
    return 1;
}

uint64_t sr_raop_head_audible_ms(sr_raop_handle *handle) {
    return handle ? handle->head_audible_ms.load() : 0;
}

} // extern "C"
