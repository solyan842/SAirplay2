/*
 * In-process Generic RAOP / AirPlay 1 bridge for SAirplay2.
 *
 * Lifecycle source of truth:
 *   music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128
 * Transport source of truth:
 *   philippe44/libraop@81c2182649da8645ac2a58b78e9f370c79a4165b
 *
 * This is the in-process form of the Windows adapter already present at the
 * #1397 baseline. It does not introduce a second RAOP protocol engine.
 */

#include "raop_bridge.h"

#include <limits.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "platform.h"
#include "raop_client.h"
#include "cross_net.h"
#include "cross_ssl.h"

#if !WIN
#error "SAirplay2 raop-static bridge is currently a Windows-only adapter"
#endif

#include <windows.h>

#define SR_FRAMES_PER_CHUNK 352
#define SR_START_LEAD_MS 200
#define SR_HAPPYCAST_DIAG_PORT 52266
#define SR_FEEDBACK_PROBE_TIMEOUT_MS 1500

enum sr_feedback_probe_class {
    SR_FEEDBACK_PROBE_NONE = 0,
    SR_FEEDBACK_PROBE_RTSP_RESPONSE = 1,
    SR_FEEDBACK_PROBE_SOCKET_ERROR = 2,
    SR_FEEDBACK_PROBE_CONNECT_ERROR = 3,
    SR_FEEDBACK_PROBE_REQUEST_ERROR = 4,
    SR_FEEDBACK_PROBE_SEND_ERROR = 5,
    SR_FEEDBACK_PROBE_RECV_ERROR = 6,
    SR_FEEDBACK_PROBE_PEER_CLOSED = 7
};

static volatile LONG g_feedback_probe_attempted = 0;
static volatile LONG g_feedback_probe_status = 0;
static volatile LONG g_feedback_probe_class = SR_FEEDBACK_PROBE_NONE;

struct sr_raop_handle {
    struct raopcl_s *client;
    uint32_t sample_rate;
    uint16_t bit_depth;
    uint16_t channels;
    size_t packet_bytes;
    uint8_t *packed24;
    uint64_t head_audible_ms;
    CRITICAL_SECTION lock;
};

/* libraop/crosstools own process-global socket/SSL state. Initialise it once
 * and keep it alive for the process lifetime so multiple RAOP handles cannot
 * unload shared transport state underneath one another. */
static INIT_ONCE g_runtime_once = INIT_ONCE_STATIC_INIT;

static BOOL CALLBACK sr_runtime_init_once(PINIT_ONCE once, PVOID parameter, PVOID *context)
{
    (void)once;
    (void)parameter;
    (void)context;
    netsock_init();
    cross_ssl_load();
    return TRUE;
}

static int sr_runtime_init(void)
{
    return InitOnceExecuteOnce(&g_runtime_once, sr_runtime_init_once, NULL, NULL) ? 1 : 0;
}

static void sr_set_open_error(sr_raop_ready *ready, uint32_t stage)
{
    if (ready) ready->open_error_stage = stage;
}

static void sr_set_feedback_probe_result(LONG attempted, LONG status, LONG result_class)
{
    InterlockedExchange(&g_feedback_probe_attempted, attempted);
    InterlockedExchange(&g_feedback_probe_status, status);
    InterlockedExchange(&g_feedback_probe_class, result_class);
}

/* Pinned MSA treats raopcl_get_ntp(NULL) as Unix 32.32 on its normal POSIX
 * deployment. The pinned libraop Windows clock is FILETIME-derived, so the
 * #1397 Windows adapter bridges only at this boundary using a relative delta.
 * Public START/HEAD values remain Unix milliseconds. */
static uint64_t sr_unix_now_ms(void)
{
    FILETIME ft;
    ULARGE_INTEGER ticks;
    const uint64_t unix_epoch_filetime = 116444736000000000ULL;

    GetSystemTimeAsFileTime(&ft);
    ticks.LowPart = ft.dwLowDateTime;
    ticks.HighPart = ft.dwHighDateTime;
    if (ticks.QuadPart <= unix_epoch_filetime) return 0;
    return (ticks.QuadPart - unix_epoch_filetime) / 10000ULL;
}

static uint64_t sr_ms_to_source_delta(uint64_t ms)
{
    return ((ms / 1000ULL) << 32) | (((ms % 1000ULL) << 32) / 1000ULL);
}

static uint64_t sr_source_delta_to_ms(uint64_t delta)
{
    return (delta >> 32) * 1000ULL
        + (((delta & 0xffffffffULL) * 1000ULL) >> 32);
}

static uint64_t sr_source_ntp_to_unix_ms(uint64_t source_ntp)
{
    uint64_t source_now = raopcl_get_ntp(NULL);
    uint64_t unix_now = sr_unix_now_ms();

    if (source_ntp >= source_now) {
        return unix_now + sr_source_delta_to_ms(source_ntp - source_now);
    }

    {
        uint64_t back = sr_source_delta_to_ms(source_now - source_ntp);
        return unix_now > back ? unix_now - back : 0;
    }
}

static uint64_t sr_resolve_start(uint64_t requested_ms, uint64_t *at_ms)
{
    uint64_t source_now = raopcl_get_ntp(NULL);
    uint64_t unix_now = sr_unix_now_ms();
    uint64_t lead_ms = SR_START_LEAD_MS;

    if (requested_ms && requested_ms >= unix_now + lead_ms) {
        if (at_ms) *at_ms = requested_ms;
        return source_now + sr_ms_to_source_delta(requested_ms - unix_now);
    }

    /* Match MSA raop_session.c: stale explicit requests get one extra lead of
     * retry slack; START(0) takes the minimum feasible floor. */
    {
        uint64_t corrected_ms = requested_ms ? lead_ms * 2ULL : lead_ms;
        if (at_ms) *at_ms = unix_now + corrected_ms;
        return source_now + sr_ms_to_source_delta(corrected_ms);
    }
}

static int sr_resolve_ipv4(const char *host_name, struct in_addr *out)
{
    struct hostent *he;
    if (!host_name || !*host_name || !out) return 0;
    he = gethostbyname(host_name);
    if (!he || !he->h_addr_list || !he->h_addr_list[0]) return 0;
    memset(out, 0, sizeof(*out));
    memcpy(&out->s_addr, he->h_addr_list[0], he->h_length);
    return 1;
}

/* Diagnostic only. This is deliberately NOT receiver classification and does
 * not alter the active libraop RTSP session. The observed Xiaomi SmartShare /
 * HappyCast endpoint uses 52266; probe POST /feedback on a short-lived sidecar
 * connection so the next hardware run tells us whether that endpoint exposes
 * the AirPlay-v1 feedback capability used by pyatv/Apple senders. */
static void sr_probe_happycast_feedback(const sr_raop_config *config,
                                        struct in_addr player)
{
    SOCKET fd = INVALID_SOCKET;
    struct sockaddr_in addr;
    DWORD timeout = SR_FEEDBACK_PROBE_TIMEOUT_MS;
    char request[768];
    char response[2048];
    int request_len;
    int sent = 0;
    int received;
    int status = 0;

    if (!config || config->port != SR_HAPPYCAST_DIAG_PORT) return;

    sr_set_feedback_probe_result(1, 0, SR_FEEDBACK_PROBE_NONE);

    fd = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (fd == INVALID_SOCKET) {
        sr_set_feedback_probe_result(1, 0, SR_FEEDBACK_PROBE_SOCKET_ERROR);
        return;
    }

    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO,
               (const char *)&timeout, sizeof(timeout));
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO,
               (const char *)&timeout, sizeof(timeout));

    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr = player;
    addr.sin_port = htons(config->port);

    if (connect(fd, (const struct sockaddr *)&addr, sizeof(addr)) == SOCKET_ERROR) {
        sr_set_feedback_probe_result(1, 0, SR_FEEDBACK_PROBE_CONNECT_ERROR);
        closesocket(fd);
        return;
    }

    request_len = snprintf(
        request, sizeof(request),
        "POST /feedback RTSP/1.0\r\n"
        "CSeq: 1\r\n"
        "DACP-ID: %s\r\n"
        "Active-Remote: %s\r\n"
        "Client-Instance: %s\r\n"
        "User-Agent: AirPlay/550.10\r\n"
        "Content-Length: 0\r\n\r\n",
        config->dacp_id ? config->dacp_id : "1A2B3D4EA1B2C3D4",
        config->active_remote ? config->active_remote : "0",
        config->dacp_id ? config->dacp_id : "1A2B3D4EA1B2C3D4");
    if (request_len <= 0 || request_len >= (int)sizeof(request)) {
        sr_set_feedback_probe_result(1, 0, SR_FEEDBACK_PROBE_REQUEST_ERROR);
        closesocket(fd);
        return;
    }

    while (sent < request_len) {
        int n = send(fd, request + sent, request_len - sent, 0);
        if (n == SOCKET_ERROR || n == 0) {
            sr_set_feedback_probe_result(1, 0, SR_FEEDBACK_PROBE_SEND_ERROR);
            closesocket(fd);
            return;
        }
        sent += n;
    }

    received = recv(fd, response, (int)sizeof(response) - 1, 0);
    if (received == SOCKET_ERROR) {
        sr_set_feedback_probe_result(1, 0, SR_FEEDBACK_PROBE_RECV_ERROR);
        closesocket(fd);
        return;
    }
    if (received == 0) {
        sr_set_feedback_probe_result(1, 0, SR_FEEDBACK_PROBE_PEER_CLOSED);
        closesocket(fd);
        return;
    }

    response[received] = '\0';
    if (sscanf(response, "RTSP/%*s %d", &status) != 1) {
        (void)sscanf(response, "HTTP/%*s %d", &status);
    }
    sr_set_feedback_probe_result(1, status, SR_FEEDBACK_PROBE_RTSP_RESPONSE);
    closesocket(fd);
}

/* Diagnostic FFI only. Rust reads this after sr_raop_open() so the result is
 * routed through the normal GUI startup-event logger. It does not mutate the
 * active RAOP session or enable periodic feedback. */
__declspec(dllexport)
int sr_raop_last_feedback_probe(int *attempted, int *status, int *result_class)
{
    if (!attempted || !status || !result_class) return 0;
    *attempted = (int)InterlockedCompareExchange(&g_feedback_probe_attempted, 0, 0);
    *status = (int)InterlockedCompareExchange(&g_feedback_probe_status, 0, 0);
    *result_class = (int)InterlockedCompareExchange(&g_feedback_probe_class, 0, 0);
    return 1;
}

static int sr_validate_config(const sr_raop_config *config)
{
    if (!config || !config->host || !*config->host) return 0;
    if (config->sample_rate != 44100 && config->sample_rate != 48000) return 0;
    if (config->bit_depth != 16 && config->bit_depth != 24) return 0;
    if (config->channels == 0) return 0;
    return 1;
}

sr_raop_handle *sr_raop_open(const sr_raop_config *config, sr_raop_ready *ready)
{
    struct in_addr player = {0};
    struct in_addr local = {0};
    raop_crypto_t crypto;
    raop_codec_t codec;
    int latency_frames;
    float initial_volume;
    sr_raop_handle *handle;

    if (ready) memset(ready, 0, sizeof(*ready));

    if (!sr_validate_config(config)) {
        sr_set_open_error(ready, SR_RAOP_OPEN_CONFIG);
        return NULL;
    }
    if (!sr_runtime_init()) {
        sr_set_open_error(ready, SR_RAOP_OPEN_RUNTIME);
        return NULL;
    }
    if (!sr_resolve_ipv4(config->host, &player)) {
        sr_set_open_error(ready, SR_RAOP_OPEN_RESOLVE_IPV4);
        return NULL;
    }

    if (config->bind_ip && *config->bind_ip) {
        if (inet_pton(AF_INET, config->bind_ip, &local) != 1) {
            sr_set_open_error(ready, SR_RAOP_OPEN_BIND_IP);
            return NULL;
        }
    }

    handle = (sr_raop_handle *)calloc(1, sizeof(*handle));
    if (!handle) {
        sr_set_open_error(ready, SR_RAOP_OPEN_HANDLE_ALLOC);
        return NULL;
    }

    InitializeCriticalSection(&handle->lock);
    handle->sample_rate = config->sample_rate;
    handle->bit_depth = config->bit_depth;
    handle->channels = config->channels;
    handle->packet_bytes = (size_t)SR_FRAMES_PER_CHUNK
        * (size_t)(config->bit_depth <= 16 ? 2 : 4)
        * (size_t)config->channels;

    if (config->bit_depth > 16) {
        handle->packed24 = (uint8_t *)malloc(
            (size_t)SR_FRAMES_PER_CHUNK * 3U * (size_t)config->channels);
        if (!handle->packed24) {
            sr_set_open_error(ready, SR_RAOP_OPEN_PACKED24_ALLOC);
            DeleteCriticalSection(&handle->lock);
            free(handle);
            return NULL;
        }
    }

    crypto = (config->encrypt && config->et && strchr(config->et, '1'))
        ? RAOP_RSA : RAOP_CLEAR;
    codec = config->compressed_alac ? RAOP_ALAC : RAOP_ALAC_RAW;
    latency_frames = (int)MS2TS(config->lead_ms, config->sample_rate);
    initial_volume = config->volume > 0
        ? raopcl_float_volume((int)config->volume)
        : -144.0f;

    handle->client = raopcl_create(
        local,
        0,
        0,
        (char *)(config->dacp_id ? config->dacp_id : "1A2B3D4EA1B2C3D4"),
        (char *)(config->active_remote ? config->active_remote : "0"),
        codec,
        SR_FRAMES_PER_CHUNK,
        latency_frames,
        crypto,
        config->mfi_auth != 0,
        (char *)(config->secret ? config->secret : ""),
        (char *)config->password,
        (char *)(config->et ? config->et : "0,4"),
        (char *)(config->md ? config->md : "0,1,2"),
        (int)config->sample_rate,
        (int)config->bit_depth,
        (int)config->channels,
        initial_volume);

    if (!handle->client) {
        sr_set_open_error(ready, SR_RAOP_OPEN_RAOPCL_CREATE);
        free(handle->packed24);
        DeleteCriticalSection(&handle->lock);
        free(handle);
        return NULL;
    }

    if (!raopcl_connect(handle->client, player, config->port, config->volume > 0)) {
        sr_set_open_error(ready, SR_RAOP_OPEN_RAOPCL_CONNECT);
        raopcl_destroy(handle->client);
        free(handle->packed24);
        DeleteCriticalSection(&handle->lock);
        free(handle);
        return NULL;
    }

    sr_probe_happycast_feedback(config, player);

    if (ready) {
        ready->latency_frames = raopcl_latency(handle->client);
        ready->sample_rate = raopcl_sample_rate(handle->client);
        ready->bit_depth = config->bit_depth;
        ready->channels = config->channels;
        ready->open_error_stage = SR_RAOP_OPEN_OK;
    }

    return handle;
}

void sr_raop_close(sr_raop_handle *handle)
{
    if (!handle) return;
    EnterCriticalSection(&handle->lock);
    if (handle->client) {
        raopcl_stop(handle->client);
        raopcl_disconnect(handle->client);
        raopcl_destroy(handle->client);
        handle->client = NULL;
    }
    LeaveCriticalSection(&handle->lock);
    DeleteCriticalSection(&handle->lock);
    free(handle->packed24);
    free(handle);
}

int sr_raop_healthy(sr_raop_handle *handle)
{
    int ok = 0;
    if (!handle) return 0;
    EnterCriticalSection(&handle->lock);
    if (handle->client) {
        ok = raopcl_is_connected(handle->client) && raopcl_is_sane(handle->client);
    }
    LeaveCriticalSection(&handle->lock);
    return ok;
}

int sr_raop_keepalive(sr_raop_handle *handle)
{
    int ok = 0;
    if (!handle) return 0;
    EnterCriticalSection(&handle->lock);
    if (handle->client) ok = raopcl_keepalive(handle->client) ? 1 : 0;
    LeaveCriticalSection(&handle->lock);
    return ok;
}

int sr_raop_commit_start(sr_raop_handle *handle,
                         uint64_t requested_unix_ms,
                         uint64_t *at_unix_ms)
{
    int ok = 0;
    uint64_t audible;
    uint64_t latency;
    raop_state_t state;

    if (!handle) return 0;
    EnterCriticalSection(&handle->lock);
    if (!handle->client) goto done;

    state = raopcl_state(handle->client);
    if (state != RAOP_STREAMING && state != RAOP_FLUSHED) goto done;

    audible = sr_resolve_start(requested_unix_ms, at_unix_ms);
    raopcl_stop(handle->client);
    if (state == RAOP_STREAMING && !raopcl_flush(handle->client)) goto done;
    latency = TS2NTP(raopcl_latency(handle->client), raopcl_sample_rate(handle->client));
    handle->head_audible_ms = 0;
    ok = raopcl_start_at(handle->client, audible - latency) ? 1 : 0;

done:
    LeaveCriticalSection(&handle->lock);
    return ok;
}

int sr_raop_start_after_flush(sr_raop_handle *handle,
                              uint64_t requested_unix_ms,
                              uint64_t *at_unix_ms)
{
    int ok = 0;
    uint64_t audible;
    uint64_t latency;

    if (!handle) return 0;
    EnterCriticalSection(&handle->lock);
    if (!handle->client || raopcl_state(handle->client) != RAOP_FLUSHED) goto done;

    audible = sr_resolve_start(requested_unix_ms, at_unix_ms);
    latency = TS2NTP(raopcl_latency(handle->client), raopcl_sample_rate(handle->client));
    handle->head_audible_ms = 0;
    ok = raopcl_start_at(handle->client, audible - latency) ? 1 : 0;
done:
    LeaveCriticalSection(&handle->lock);
    return ok;
}

int sr_raop_flush(sr_raop_handle *handle)
{
    int ok = 0;
    raop_state_t state;
    if (!handle) return 0;

    EnterCriticalSection(&handle->lock);
    if (!handle->client) goto done;
    state = raopcl_state(handle->client);
    if (state != RAOP_STREAMING && state != RAOP_FLUSHED) goto done;
    raopcl_stop(handle->client);
    ok = state == RAOP_FLUSHED ? 1 : (raopcl_flush(handle->client) ? 1 : 0);
    if (ok) handle->head_audible_ms = 0;

done:
    LeaveCriticalSection(&handle->lock);
    return ok;
}

int sr_raop_standby(sr_raop_handle *handle)
{
    return sr_raop_flush(handle);
}

int sr_raop_pause(sr_raop_handle *handle)
{
    int ok = 0;
    raop_state_t state;
    if (!handle) return 0;

    EnterCriticalSection(&handle->lock);
    if (!handle->client) goto done;
    state = raopcl_state(handle->client);
    if (state == RAOP_FLUSHED) {
        ok = 1;
        goto done;
    }
    if (state != RAOP_STREAMING) goto done;
    raopcl_pause(handle->client);
    ok = raopcl_flush(handle->client) ? 1 : 0;
    if (ok) handle->head_audible_ms = 0;

done:
    LeaveCriticalSection(&handle->lock);
    return ok;
}

int sr_raop_play(sr_raop_handle *handle)
{
    int ok = 0;
    uint64_t audible;
    uint64_t latency;
    raop_state_t state;
    if (!handle) return 0;

    EnterCriticalSection(&handle->lock);
    if (!handle->client) goto done;
    state = raopcl_state(handle->client);
    if (state != RAOP_FLUSHED && state != RAOP_STREAMING) goto done;
    audible = raopcl_get_ntp(NULL) + MS2NTP(SR_START_LEAD_MS);
    latency = TS2NTP(raopcl_latency(handle->client), raopcl_sample_rate(handle->client));
    handle->head_audible_ms = 0;
    ok = raopcl_start_at(handle->client, audible - latency) ? 1 : 0;

done:
    LeaveCriticalSection(&handle->lock);
    return ok;
}

int sr_raop_stop(sr_raop_handle *handle)
{
    if (!handle) return 0;
    EnterCriticalSection(&handle->lock);
    if (!handle->client) {
        LeaveCriticalSection(&handle->lock);
        return 0;
    }
    raopcl_stop(handle->client);
    handle->head_audible_ms = 0;
    LeaveCriticalSection(&handle->lock);
    return 1;
}

int sr_raop_set_volume(sr_raop_handle *handle, uint8_t percent)
{
    int ok = 0;
    if (!handle) return 0;
    if (percent > 100) percent = 100;
    EnterCriticalSection(&handle->lock);
    if (handle->client) {
        ok = raopcl_set_volume(handle->client, raopcl_float_volume((int)percent)) ? 1 : 0;
    }
    LeaveCriticalSection(&handle->lock);
    return ok;
}

int sr_raop_set_progress(sr_raop_handle *handle,
                         uint32_t elapsed_s,
                         uint32_t duration_s)
{
    int ok = 0;
    if (!handle) return 0;
    EnterCriticalSection(&handle->lock);
    if (handle->client) {
        ok = raopcl_set_progress_ms(handle->client,
                                    elapsed_s * 1000U,
                                    duration_s * 1000U) ? 1 : 0;
    }
    LeaveCriticalSection(&handle->lock);
    return ok;
}

int sr_raop_set_metadata(sr_raop_handle *handle,
                         const char *title,
                         const char *artist,
                         const char *album)
{
    int ok = 0;
    if (!handle) return 0;
    EnterCriticalSection(&handle->lock);
    if (handle->client) {
        ok = raopcl_set_daap(handle->client, 4,
                             "minm", 's', (char *)(title ? title : ""),
                             "asar", 's', (char *)(artist ? artist : ""),
                             "asal", 's', (char *)(album ? album : ""),
                             "astn", 'i', 1) ? 1 : 0;
    }
    LeaveCriticalSection(&handle->lock);
    return ok;
}

int sr_raop_set_artwork(sr_raop_handle *handle,
                        const char *content_type,
                        const uint8_t *data,
                        size_t size)
{
    int ok = 0;
    if (!handle || !content_type || (!data && size) || size > INT_MAX) return 0;
    EnterCriticalSection(&handle->lock);
    if (handle->client) {
        ok = raopcl_set_artwork(handle->client,
                                (char *)content_type,
                                (int)size,
                                (char *)data) ? 1 : 0;
    }
    LeaveCriticalSection(&handle->lock);
    return ok;
}

static void sr_pack_32_to_24(const uint8_t *input, size_t input_bytes, uint8_t *output)
{
    size_t samples = input_bytes / 4U;
    size_t i;
    for (i = 0; i < samples; ++i) {
        output[i * 3U + 0U] = input[i * 4U + 1U];
        output[i * 3U + 1U] = input[i * 4U + 2U];
        output[i * 3U + 2U] = input[i * 4U + 3U];
    }
}

int sr_raop_write_packet(sr_raop_handle *handle,
                         const uint8_t *packet,
                         size_t packet_bytes)
{
    if (!handle || !packet || packet_bytes != handle->packet_bytes) return 0;

    for (;;) {
        uint8_t *send_buffer;
        uint64_t playtime = 0;
        uint64_t next_head;
        raop_state_t state;

        EnterCriticalSection(&handle->lock);
        if (!handle->client) {
            LeaveCriticalSection(&handle->lock);
            return 0;
        }

        /* A packet that races a FLUSH belongs to the old generation. The Rust
         * generation barrier owns the discard; consuming it here without
         * treating the transport as failed preserves that contract. A FLUSHED
         * session after START must still reach raopcl_accept_frames(), because
         * pinned libraop performs FLUSHED -> STREAMING at that pacing gate. */
        state = raopcl_state(handle->client);
        if (state != RAOP_STREAMING && state != RAOP_FLUSHED) {
            LeaveCriticalSection(&handle->lock);
            return 1;
        }

        if (!raopcl_is_connected(handle->client) || !raopcl_is_sane(handle->client)) {
            LeaveCriticalSection(&handle->lock);
            return 0;
        }

        if (!raopcl_accept_frames(handle->client)) {
            LeaveCriticalSection(&handle->lock);
            Sleep(1);
            continue;
        }

        send_buffer = (uint8_t *)packet;
        if (handle->bit_depth > 16) {
            sr_pack_32_to_24(packet, packet_bytes, handle->packed24);
            send_buffer = handle->packed24;
        }

        if (!raopcl_send_chunk(handle->client,
                               send_buffer,
                               SR_FRAMES_PER_CHUNK,
                               &playtime)) {
            LeaveCriticalSection(&handle->lock);
            return 0;
        }

        next_head = playtime
            + TS2NTP(SR_FRAMES_PER_CHUNK, raopcl_sample_rate(handle->client));
        handle->head_audible_ms = sr_source_ntp_to_unix_ms(next_head);
        LeaveCriticalSection(&handle->lock);
        return 1;
    }
}

uint64_t sr_raop_head_audible_ms(sr_raop_handle *handle)
{
    uint64_t value = 0;
    if (!handle) return 0;
    EnterCriticalSection(&handle->lock);
    value = handle->head_audible_ms;
    LeaveCriticalSection(&handle->lock);
    return value;
}
