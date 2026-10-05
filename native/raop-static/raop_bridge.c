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
#include "strict_clock_shim.h"

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

struct sr_raop_handle {
    struct raopcl_s *client;
    uint32_t sample_rate;
    uint16_t bit_depth;
    uint16_t channels;
    int keepalive_compat_logged;
    size_t packet_bytes;
    uint8_t *packed24;
    uint64_t first_start_audible_ms;
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

    /* Wire counters are enabled only in the isolated strict-NTP DLL instance.
     * Normal SOtM/RAOP sessions keep this diagnostic path dormant. */
    raopcl_diag_enable(handle->client, sr_raop_strict_ntp_clock_enabled() != 0);

    if (!raopcl_connect(handle->client, player, config->port, config->volume > 0)) {
        sr_set_open_error(ready, SR_RAOP_OPEN_RAOPCL_CONNECT);
        raopcl_destroy(handle->client);
        free(handle->packed24);
        DeleteCriticalSection(&handle->lock);
        free(handle);
        return NULL;
    }

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

static const char *sr_raop_state_name(raop_state_t state)
{
    switch (state) {
    case RAOP_DOWN: return "down";
    case RAOP_FLUSHING: return "flushing";
    case RAOP_FLUSHED: return "flushed";
    case RAOP_STREAMING: return "streaming";
    default: return "unknown";
    }
}

static void sr_raop_log_health_failure(sr_raop_handle *handle, const char *where)
{
    int connected;
    int sane;
    raop_state_t state;
    const char *failure_class;

    if (!handle || !handle->client) {
        fprintf(stderr, "MSA-RAOP HEALTH failure where=%s class=no-client\n",
                where ? where : "unknown");
        fflush(stderr);
        return;
    }

    state = raopcl_state(handle->client);
    connected = raopcl_is_connected(handle->client) ? 1 : 0;
    sane = raopcl_is_sane(handle->client) ? 1 : 0;
    failure_class = !connected ? "rtsp-control"
        : !sane ? "rtp-media-control-timing"
        : "unknown";

    fprintf(stderr,
            "MSA-RAOP HEALTH failure where=%s class=%s state=%s(%d) connected=%d sane=%d head_audible_ms=%llu\n",
            where ? where : "unknown",
            failure_class,
            sr_raop_state_name(state),
            (int)state,
            connected,
            sane,
            (unsigned long long)handle->head_audible_ms);
    fflush(stderr);
}

int sr_raop_healthy(sr_raop_handle *handle)
{
    int connected = 0;
    int sane = 0;
    int ok = 0;
    if (!handle) return 0;
    EnterCriticalSection(&handle->lock);
    if (handle->client) {
        connected = raopcl_is_connected(handle->client) ? 1 : 0;
        sane = raopcl_is_sane(handle->client) ? 1 : 0;
        ok = connected && sane;
        if (!ok) sr_raop_log_health_failure(handle, "health-monitor");
    }
    LeaveCriticalSection(&handle->lock);
    return ok;
}

int sr_raop_keepalive(sr_raop_handle *handle)
{
    int ok = 0;
    if (!handle) return 0;
    EnterCriticalSection(&handle->lock);
    if (handle->client) {
        ok = raopcl_keepalive(handle->client) ? 1 : 0;
        /* Some RAOP receivers accept SETUP/RECORD and stream media correctly
         * but do not implement RTSP OPTIONS keepalive reliably. Detect that
         * compatibility case by runtime transport state, never by model/port:
         * an OPTIONS-only failure is tolerated only while the pinned transport
         * still reports connected + sane. Healthy receivers keep the exact
         * normal path because this branch is dormant unless OPTIONS fails;
         * real RTSP/RTP failure still trips sr_raop_healthy() immediately. */
        if (!ok
                && raopcl_is_connected(handle->client)
                && raopcl_is_sane(handle->client)) {
            if (!handle->keepalive_compat_logged) {
                fprintf(stderr,
                        "MSA-RAOP COMPAT: RTSP OPTIONS keepalive unsupported/unacknowledged; transport still connected/sane, ignoring OPTIONS-only failure.\n");
                fflush(stderr);
                handle->keepalive_compat_logged = 1;
            }
            ok = 1;
        }
    }
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
    uint64_t resolved_at_ms = 0;
    raop_state_t state;

    if (!handle) return 0;
    EnterCriticalSection(&handle->lock);
    if (!handle->client) goto done;

    state = raopcl_state(handle->client);
    if (state != RAOP_STREAMING && state != RAOP_FLUSHED) goto done;

    audible = sr_resolve_start(requested_unix_ms, &resolved_at_ms);
    if (at_unix_ms) *at_unix_ms = resolved_at_ms;
    raopcl_stop(handle->client);
    if (state == RAOP_STREAMING && !raopcl_flush(handle->client)) goto done;
    latency = TS2NTP(raopcl_latency(handle->client), raopcl_sample_rate(handle->client));
    handle->head_audible_ms = 0;
    ok = raopcl_start_at(handle->client, audible - latency) ? 1 : 0;
    handle->first_start_audible_ms = ok ? resolved_at_ms : 0;

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

    handle->first_start_audible_ms = 0;
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
    if (ok) {
        handle->first_start_audible_ms = 0;
        handle->head_audible_ms = 0;
    }

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
        handle->first_start_audible_ms = 0;
        ok = 1;
        goto done;
    }
    if (state != RAOP_STREAMING) goto done;
    raopcl_pause(handle->client);
    ok = raopcl_flush(handle->client) ? 1 : 0;
    if (ok) {
        handle->first_start_audible_ms = 0;
        handle->head_audible_ms = 0;
    }

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
    handle->first_start_audible_ms = 0;
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
    handle->first_start_audible_ms = 0;
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
            sr_raop_log_health_failure(handle, "write-preflight");
            LeaveCriticalSection(&handle->lock);
            return 0;
        }

        /* Some receivers can stall the mandatory initial metadata RTSP request
         * long enough that the first START's audible target is already in the
         * past before PCM delivery opens.  Re-arm only that stale first START,
         * while libraop is still FLUSHED and before any audio packet is sent.
         * Healthy receivers (including the locked SOtM baseline) never enter
         * this branch, so their START/pacing semantics remain byte-for-byte
         * equivalent after the condition check. */
        if (state == RAOP_FLUSHED && handle->first_start_audible_ms != 0) {
            uint64_t now_unix_ms = sr_unix_now_ms();
            if (handle->first_start_audible_ms <= now_unix_ms) {
                uint32_t sample_rate = raopcl_sample_rate(handle->client);
                uint32_t latency_frames = raopcl_latency(handle->client);
                uint64_t latency_ms = sample_rate
                    ? ((uint64_t)latency_frames * 1000ULL) / (uint64_t)sample_rate
                    : 0;
                uint64_t stale_by_ms = now_unix_ms - handle->first_start_audible_ms;
                uint64_t source_start = raopcl_get_ntp(NULL) + MS2NTP(SR_START_LEAD_MS);
                uint64_t rearmed_audible_ms = now_unix_ms
                    + latency_ms
                    + SR_START_LEAD_MS;

                if (!raopcl_start_at(handle->client, source_start)) {
                    sr_raop_log_health_failure(handle, "first-start-rearm");
                    LeaveCriticalSection(&handle->lock);
                    return 0;
                }
                handle->first_start_audible_ms = rearmed_audible_ms;
                fprintf(stderr,
                        "MSA-RAOP COMPAT: stale first START re-armed before first PCM; stale_by=%llums new_audible=%llu latency=%llums guard=%ums.\n",
                        (unsigned long long)stale_by_ms,
                        (unsigned long long)rearmed_audible_ms,
                        (unsigned long long)latency_ms,
                        (unsigned)SR_START_LEAD_MS);
                fflush(stderr);
            }
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
            sr_raop_log_health_failure(handle, "send-chunk");
            LeaveCriticalSection(&handle->lock);
            return 0;
        }

        handle->first_start_audible_ms = 0;
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

int sr_raop_diag_snapshot(sr_raop_handle *handle, sr_raop_wire_diag *out)
{
    raop_diag_snapshot_t raw;
    int ok = 0;
    if (!handle || !out) return 0;
    memset(out, 0, sizeof(*out));

    EnterCriticalSection(&handle->lock);
    if (handle->client && raopcl_diag_snapshot(handle->client, &raw)) {
        out->audio_lport = raw.audio_lport;
        out->audio_rport = raw.audio_rport;
        out->control_lport = raw.control_lport;
        out->control_rport = raw.control_rport;
        out->timing_lport = raw.timing_lport;
        out->timing_rport = raw.timing_rport;
        out->state = raw.state;
        out->seq_number = raw.seq_number;
        out->sane_ctrl = raw.sane_ctrl;
        out->sane_time = raw.sane_time;
        out->sane_audio_avail = raw.sane_audio_avail;
        out->sane_audio_select = raw.sane_audio_select;
        out->sane_audio_send = raw.sane_audio_send;
        out->audio_send_ok = raw.audio_send_ok;
        out->audio_send_fail = raw.audio_send_fail;
        out->sync_send_ok = raw.sync_send_ok;
        out->sync_send_fail = raw.sync_send_fail;
        out->timing_requests = raw.timing_requests;
        out->timing_responses = raw.timing_responses;
        out->timing_response_fail = raw.timing_response_fail;
        out->control_requests = raw.control_requests;
        out->retransmit = raw.retransmit;
        out->first_audio_timestamp = raw.first_audio_timestamp;
        out->last_audio_timestamp = raw.last_audio_timestamp;
        out->first_audio_seq = raw.first_audio_seq;
        out->last_audio_seq = raw.last_audio_seq;
        ok = 1;
    }
    LeaveCriticalSection(&handle->lock);
    return ok;
}
