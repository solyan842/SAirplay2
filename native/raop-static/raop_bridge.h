#ifndef SAIRPLAY_RAOP_BRIDGE_H
#define SAIRPLAY_RAOP_BRIDGE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * Thin in-process ABI for the Generic RAOP / AirPlay 1 lane.
 *
 * Source of truth:
 *   music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128
 *   philippe44/libraop@81c2182649da8645ac2a58b78e9f370c79a4165b
 *
 * This boundary exists only to replace the Windows helper-process/file-IPC
 * transport used at the #1397 baseline. Protocol/timing behavior stays owned
 * by the pinned MSA raop_session/libraop sources; this is not a new RAOP engine.
 */

typedef struct sr_raop_handle sr_raop_handle;

typedef struct sr_raop_config {
    const char *host;
    const char *bind_ip;
    uint16_t port;
    uint8_t volume;
    const char *et;
    const char *md;
    const char *secret;
    const char *password;
    int compressed_alac;
    int mfi_auth;
    int encrypt;
    const char *dacp_id;
    const char *active_remote;
    uint32_t sample_rate;
    uint16_t bit_depth;
    uint16_t channels;
    uint32_t lead_ms;
} sr_raop_config;

typedef struct sr_raop_ready {
    uint32_t latency_frames;
    uint32_t sample_rate;
    uint16_t bit_depth;
    uint16_t channels;
} sr_raop_ready;

sr_raop_handle *sr_raop_open(const sr_raop_config *config, sr_raop_ready *ready);
void sr_raop_close(sr_raop_handle *handle);

int sr_raop_healthy(sr_raop_handle *handle);
int sr_raop_keepalive(sr_raop_handle *handle);

int sr_raop_commit_start(sr_raop_handle *handle,
                         uint64_t requested_unix_ms,
                         uint64_t *at_unix_ms);
int sr_raop_start_after_flush(sr_raop_handle *handle,
                              uint64_t requested_unix_ms,
                              uint64_t *at_unix_ms);
int sr_raop_flush(sr_raop_handle *handle);
int sr_raop_standby(sr_raop_handle *handle);
int sr_raop_pause(sr_raop_handle *handle);
int sr_raop_play(sr_raop_handle *handle);
int sr_raop_stop(sr_raop_handle *handle);

int sr_raop_set_volume(sr_raop_handle *handle, uint8_t percent);
int sr_raop_set_progress(sr_raop_handle *handle,
                         uint32_t elapsed_s,
                         uint32_t duration_s);
int sr_raop_set_metadata(sr_raop_handle *handle,
                         const char *title,
                         const char *artist,
                         const char *album);
int sr_raop_set_artwork(sr_raop_handle *handle,
                        const char *content_type,
                        const uint8_t *data,
                        size_t size);

/* Exactly one MSA/libraop packet: 352 frames at the negotiated format. */
int sr_raop_write_packet(sr_raop_handle *handle,
                         const uint8_t *packet,
                         size_t packet_bytes);

uint64_t sr_raop_head_audible_ms(sr_raop_handle *handle);

#ifdef __cplusplus
}
#endif

#endif /* SAIRPLAY_RAOP_BRIDGE_H */
