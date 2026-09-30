/*
 * Independent MSA SOLO RAOP helper.
 * Source behavior: music-assistant/airplay-cli@431c5c5 raop_session.c
 * Transport: philippe44/libraop@81c2182649da8645ac2a58b78e9f370c79a4165b
 *
 * This helper keeps one raopcl_s alive across START/FLUSH/STANDBY/PAUSE/PLAY.
 * stdin is PCM16LE stereo/44.1k; lifecycle commands arrive through an atomic
 * command file and replies are written to an ack file. It is deliberately a
 * separate executable from the frozen legacy cliraop helper.
 */

#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <stdbool.h>
#include <string.h>
#include <fcntl.h>
#include <sys/stat.h>
#include "platform.h"

#if WIN
#include <windows.h>
#include <io.h>
#else
#include <unistd.h>
#endif

#include "raop_client.h"
#include "cross_net.h"
#include "cross_ssl.h"
#include "cross_log.h"

log_level util_loglevel;
log_level raop_loglevel;
log_level main_log;
log_level *loglevel = &main_log;

#define FRAMES_PER_CHUNK 352
#define PCM_BYTES (FRAMES_PER_CHUNK * 4)
#define KEEPALIVE_MS 20000
#define START_LEAD_MS 200

static uint64_t unix_ms_to_ntp(uint64_t ms) {
    return ((ms / 1000ULL) << 32) | (((ms % 1000ULL) << 32) / 1000ULL);
}
static uint64_t ntp_to_unix_ms(uint64_t ntp) {
    return (ntp >> 32) * 1000ULL + (((ntp & 0xffffffffULL) * 1000ULL) >> 32);
}
static uint64_t resolve_start(uint64_t requested_ms) {
    uint64_t lead = MS2NTP(START_LEAD_MS);
    uint64_t floor = raopcl_get_ntp(NULL) + lead;
    uint64_t requested = requested_ms ? unix_ms_to_ntp(requested_ms) : 0;
    if (requested_ms && requested >= floor) return requested;
    return requested_ms ? floor + lead : floor;
}

static void ack_write(const char *path, uint64_t seq, bool ok, uint64_t at_ms, const char *detail) {
    char tmp[MAX_PATH * 2];
    snprintf(tmp, sizeof(tmp), "%s.tmp", path);
    FILE *f = fopen(tmp, "wb");
    if (!f) return;
    fprintf(f, "%llu %s %llu %s\n",
            (unsigned long long)seq, ok ? "OK" : "ERR",
            (unsigned long long)at_ms, detail ? detail : "");
    fclose(f);
#if WIN
    MoveFileExA(tmp, path, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH);
#else
    rename(tmp, path);
#endif
}

static bool session_commit(struct raopcl_s *p, uint64_t requested_ms, uint64_t *at_ms) {
    raop_state_t state = raopcl_state(p);
    if (state != RAOP_STREAMING && state != RAOP_FLUSHED) return false;
    uint64_t audible = resolve_start(requested_ms);
    *at_ms = ntp_to_unix_ms(audible);
    raopcl_stop(p);
    if (state == RAOP_STREAMING && !raopcl_flush(p)) return false;
    uint64_t latency = TS2NTP(raopcl_latency(p), raopcl_sample_rate(p));
    return raopcl_start_at(p, audible - latency);
}

static bool session_start_after_flush(struct raopcl_s *p, uint64_t requested_ms, uint64_t *at_ms) {
    if (raopcl_state(p) != RAOP_FLUSHED) return false;
    uint64_t audible = resolve_start(requested_ms);
    *at_ms = ntp_to_unix_ms(audible);
    uint64_t latency = TS2NTP(raopcl_latency(p), raopcl_sample_rate(p));
    return raopcl_start_at(p, audible - latency);
}

static bool session_flush(struct raopcl_s *p) {
    raop_state_t state = raopcl_state(p);
    if (state != RAOP_STREAMING && state != RAOP_FLUSHED) return false;
    raopcl_stop(p);
    return state == RAOP_FLUSHED ? true : raopcl_flush(p);
}

static bool session_pause(struct raopcl_s *p) {
    raop_state_t state = raopcl_state(p);
    if (state == RAOP_FLUSHED) return true;
    if (state != RAOP_STREAMING) return false;
    raopcl_pause(p);
    return raopcl_flush(p);
}

static bool session_play(struct raopcl_s *p) {
    raop_state_t state = raopcl_state(p);
    if (state != RAOP_FLUSHED && state != RAOP_STREAMING) return false;
    uint64_t audible = raopcl_get_ntp(NULL) + MS2NTP(START_LEAD_MS);
    uint64_t latency = TS2NTP(raopcl_latency(p), raopcl_sample_rate(p));
    return raopcl_start_at(p, audible - latency);
}

static int available_stdin(void) {
#if WIN
    DWORD avail = 0;
    HANDLE h = GetStdHandle(STD_INPUT_HANDLE);
    if (!h || h == INVALID_HANDLE_VALUE) return 0;
    if (!PeekNamedPipe(h, NULL, 0, NULL, &avail, NULL)) return 0;
    return (int)avail;
#else
    return 0;
#endif
}

static bool process_command(const char *path, const char *ack_path,
                            uint64_t *last_seq, struct raopcl_s *p, bool *quit) {
    FILE *f = fopen(path, "rb");
    if (!f) return true;

    unsigned long long seq_raw = 0, arg1 = 0, arg2 = 0;
    char cmd[40] = {0};
    int n = fscanf(f, "%llu %39s %llu %llu", &seq_raw, cmd, &arg1, &arg2);
    fclose(f);
    if (n < 2) return true;

    uint64_t seq = (uint64_t)seq_raw;
    if (seq == 0 || seq == *last_seq) return true;
    *last_seq = seq;

    bool ok = false;
    uint64_t at_ms = 0;
    const char *detail = cmd;

    if (!strcmp(cmd, "START")) {
        ok = session_commit(p, (uint64_t)arg1, &at_ms);
    } else if (!strcmp(cmd, "START_AFTER_FLUSH")) {
        ok = session_start_after_flush(p, (uint64_t)arg1, &at_ms);
    } else if (!strcmp(cmd, "FLUSH") || !strcmp(cmd, "STANDBY")) {
        ok = session_flush(p);
    } else if (!strcmp(cmd, "PAUSE")) {
        ok = session_pause(p);
    } else if (!strcmp(cmd, "PLAY")) {
        ok = session_play(p);
    } else if (!strcmp(cmd, "STOP")) {
        raopcl_stop(p);
        ok = true;
    } else if (!strcmp(cmd, "VOLUME")) {
        unsigned vol = (unsigned)arg1 > 100 ? 100 : (unsigned)arg1;
        ok = raopcl_set_volume(p, raopcl_float_volume((int)vol));
    } else if (!strcmp(cmd, "PROGRESS")) {
        ok = raopcl_set_progress_ms(p, (uint32_t)arg1 * 1000U, (uint32_t)arg2 * 1000U);
    } else if (!strcmp(cmd, "KEEPALIVE")) {
        ok = raopcl_keepalive(p);
    } else if (!strcmp(cmd, "QUIT")) {
        ok = true;
        *quit = true;
    } else {
        detail = "unknown_command";
    }

    ack_write(ack_path, seq, ok, at_ms, detail);
    return ok;
}

int main(int argc, char **argv) {
    int port = 5000, volume = 50, latency = MS2TS(1000, 44100);
    bool alac = true, auth = false;
    char *secret = NULL, *password = NULL, *et = NULL, *md = NULL;
    char *control = NULL, *ack = NULL, *host_name = NULL;

    for (int i = 1; i < argc; ++i) {
        if (!strcmp(argv[i], "--control") && i + 1 < argc) control = argv[++i];
        else if (!strcmp(argv[i], "--ack") && i + 1 < argc) ack = argv[++i];
        else if (!strcmp(argv[i], "-p") && i + 1 < argc) port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-v") && i + 1 < argc) volume = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-l") && i + 1 < argc) latency = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-s") && i + 1 < argc) secret = argv[++i];
        else if (!strcmp(argv[i], "-P") && i + 1 < argc) password = argv[++i];
        else if (!strcmp(argv[i], "-t") && i + 1 < argc) et = argv[++i];
        else if (!strcmp(argv[i], "-m") && i + 1 < argc) md = argv[++i];
        else if (!strcmp(argv[i], "-u")) auth = true;
        else if (!strcmp(argv[i], "--pcm")) alac = false;
        else if (argv[i][0] != '-') host_name = argv[i];
    }

    if (!control || !ack || !host_name) {
        fprintf(stderr, "usage: cliraop-msa-solo --control FILE --ack FILE [opts] host\n");
        return 2;
    }

#if WIN
    _setmode(_fileno(stdin), _O_BINARY);
#endif
    netsock_init();
    cross_ssl_load();
    util_loglevel = lERROR;
    raop_loglevel = lINFO;
    main_log = lINFO;

    struct hostent *he = gethostbyname(host_name);
    if (!he) {
        fprintf(stderr, "MSA-RAOP ERROR resolve\n");
        return 3;
    }
    struct in_addr player = {0}, local = {0};
    memcpy(&player.s_addr, he->h_addr_list[0], he->h_length);

    struct raopcl_s *p = raopcl_create(
        local, 0, 0, NULL, NULL, alac ? RAOP_ALAC : RAOP_PCM,
        FRAMES_PER_CHUNK, latency, RAOP_CLEAR, auth, secret, password,
        et, md, 44100, 16, 2, raopcl_float_volume(volume)
    );
    if (!p) {
        fprintf(stderr, "MSA-RAOP ERROR create\n");
        return 4;
    }
    if (!raopcl_connect(p, player, (uint16_t)port, true)) {
        fprintf(stderr, "MSA-RAOP ERROR connect\n");
        raopcl_destroy(p);
        return 5;
    }

    fprintf(stderr, "MSA-RAOP READY latency=%u sample_rate=%u\n",
            raopcl_latency(p), raopcl_sample_rate(p));
    fflush(stderr);

    uint8_t pcm[PCM_BYTES];
    size_t pcm_len = 0;
    uint64_t last_seq = 0;
    uint64_t last_keepalive = raopcl_get_ntp(NULL);
    bool quit = false;

    while (!quit) {
        process_command(control, ack, &last_seq, p, &quit);
        if (quit) break;

        uint64_t now = raopcl_get_ntp(NULL);
        if (now - last_keepalive >= MS2NTP(KEEPALIVE_MS)) {
            raopcl_keepalive(p);
            last_keepalive = now;
        }

        if (raopcl_state(p) == RAOP_STREAMING && raopcl_accept_frames(p)) {
            int avail = available_stdin();
            if (avail > 0 && pcm_len < PCM_BYTES) {
                int want = (int)(PCM_BYTES - pcm_len);
                if (want > avail) want = avail;
#if WIN
                int got = _read(_fileno(stdin), pcm + pcm_len, want);
#else
                int got = read(fileno(stdin), pcm + pcm_len, want);
#endif
                if (got > 0) pcm_len += (size_t)got;
            }
            if (pcm_len == PCM_BYTES) {
                uint64_t playtime = 0;
                if (!raopcl_send_chunk(p, pcm, FRAMES_PER_CHUNK, &playtime)) {
                    fprintf(stderr, "MSA-RAOP ERROR send\n");
                    break;
                }
                pcm_len = 0;
            }
        }

#if WIN
        Sleep(1);
#else
        usleep(1000);
#endif
    }

    raopcl_disconnect(p);
    raopcl_destroy(p);
    cross_ssl_free();
    netsock_close();
    return 0;
}
