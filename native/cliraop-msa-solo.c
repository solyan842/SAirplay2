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
#define KEEPALIVE_MS 20000
#define START_LEAD_MS 200

static int truncate_32to24(const uint8_t *in, int in_bytes, uint8_t *out) {
    int samples = in_bytes / 4;
    for (int i = 0; i < samples; i++) {
        out[i * 3 + 0] = in[i * 4 + 1];
        out[i * 3 + 1] = in[i * 4 + 2];
        out[i * 3 + 2] = in[i * 4 + 3];
    }
    return samples * 3;
}

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


static bool read_u32_le(FILE *f, uint32_t *out) {
    uint8_t b[4];
    if (fread(b, 1, 4, f) != 4) return false;
    *out = (uint32_t)b[0] | ((uint32_t)b[1] << 8) |
           ((uint32_t)b[2] << 16) | ((uint32_t)b[3] << 24);
    return true;
}

static char *read_lp_string(FILE *f) {
    uint32_t n = 0;
    if (!read_u32_le(f, &n) || n > (1u << 20)) return NULL;
    char *s = calloc((size_t)n + 1, 1);
    if (!s) return NULL;
    if (n && fread(s, 1, n, f) != n) { free(s); return NULL; }
    s[n] = '\0';
    return s;
}

static bool set_metadata_from_file(struct raopcl_s *p, const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) return false;
    char *title = read_lp_string(f);
    char *artist = title ? read_lp_string(f) : NULL;
    char *album = artist ? read_lp_string(f) : NULL;
    fclose(f);
    if (!title || !artist || !album) {
        free(title); free(artist); free(album);
        return false;
    }
    bool ok = raopcl_set_daap(p, 4,
                              "minm", 's', title,
                              "asar", 's', artist,
                              "asal", 's', album,
                              "astn", 'i', 1);
    free(title); free(artist); free(album);
    return ok;
}

static bool set_artwork_from_file(struct raopcl_s *p, const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) return false;
    char *mime = read_lp_string(f);
    uint32_t n = 0;
    if (!mime || !read_u32_le(f, &n) || n > (16u << 20)) {
        free(mime); fclose(f); return false;
    }
    char *data = malloc(n ? n : 1);
    if (!data) { free(mime); fclose(f); return false; }
    bool ok = (!n || fread(data, 1, n, f) == n);
    fclose(f);
    if (ok) ok = raopcl_set_artwork(p, mime, (int)n, data);
    free(mime); free(data);
    return ok;
}

static bool process_command(const char *path, const char *ack_path,
                            const char *metadata_path, const char *artwork_path,
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
    } else if (!strcmp(cmd, "METADATA")) {
        ok = metadata_path && set_metadata_from_file(p, metadata_path);
    } else if (!strcmp(cmd, "ARTWORK")) {
        ok = artwork_path && set_artwork_from_file(p, artwork_path);
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
    int port = 5000, volume = 50, lead_ms = 2000, sample_rate = 44100, bit_depth = 16, channels = 2;
    bool alac = true, auth = false, encrypt = false;
    char *secret = NULL, *password = NULL, *et = NULL, *md = NULL;
    char *dacp_id = NULL, *active_remote = NULL, *bind_ip = NULL;
    char *control = NULL, *ack = NULL, *metadata_path = NULL, *artwork_path = NULL, *host_name = NULL;

    for (int i = 1; i < argc; ++i) {
        if (!strcmp(argv[i], "--control") && i + 1 < argc) control = argv[++i];
        else if (!strcmp(argv[i], "--ack") && i + 1 < argc) ack = argv[++i];
        else if (!strcmp(argv[i], "--metadata") && i + 1 < argc) metadata_path = argv[++i];
        else if (!strcmp(argv[i], "--artwork") && i + 1 < argc) artwork_path = argv[++i];
        else if (!strcmp(argv[i], "-p") && i + 1 < argc) port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-v") && i + 1 < argc) volume = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-l") && i + 1 < argc) lead_ms = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-r") && i + 1 < argc) sample_rate = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-b") && i + 1 < argc) bit_depth = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-c") && i + 1 < argc) channels = atoi(argv[++i]);
        else if (!strcmp(argv[i], "-s") && i + 1 < argc) secret = argv[++i];
        else if (!strcmp(argv[i], "-P") && i + 1 < argc) password = argv[++i];
        else if (!strcmp(argv[i], "-t") && i + 1 < argc) et = argv[++i];
        else if (!strcmp(argv[i], "-m") && i + 1 < argc) md = argv[++i];
        else if (!strcmp(argv[i], "-D") && i + 1 < argc) dacp_id = argv[++i];
        else if (!strcmp(argv[i], "-R") && i + 1 < argc) active_remote = argv[++i];
        else if (!strcmp(argv[i], "--bind") && i + 1 < argc) bind_ip = argv[++i];
        else if (!strcmp(argv[i], "-e")) encrypt = true;
        else if (!strcmp(argv[i], "-u")) auth = true;
        else if (!strcmp(argv[i], "--pcm")) alac = false;
        else if (argv[i][0] != '-') host_name = argv[i];
    }

    if (!control || !ack || !metadata_path || !artwork_path || !host_name) {
        fprintf(stderr, "usage: cliraop-msa-solo --control FILE --ack FILE --metadata FILE --artwork FILE [opts] host\n");
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
    if (bind_ip && *bind_ip && inet_pton(AF_INET, bind_ip, &local) != 1) {
        fprintf(stderr, "MSA-RAOP ERROR bind_ip\n");
        return 3;
    }

    if (channels <= 0 || (bit_depth != 16 && bit_depth != 24) ||
        (sample_rate != 44100 && sample_rate != 48000)) {
        fprintf(stderr, "MSA-RAOP ERROR unsupported_format\n");
        return 2;
    }
    int latency = MS2TS(lead_ms, sample_rate);
    raop_crypto_t crypto = (encrypt && et && strchr(et, '1')) ? RAOP_RSA : RAOP_CLEAR;
    struct raopcl_s *p = raopcl_create(
        local, 0, 0,
        dacp_id ? dacp_id : "1A2B3D4EA1B2C3D4",
        active_remote ? active_remote : "0",
        alac ? RAOP_ALAC : RAOP_ALAC_RAW,
        FRAMES_PER_CHUNK, latency, crypto, auth,
        secret ? secret : "", password,
        et ? et : "0,4", md ? md : "0,1,2",
        sample_rate, bit_depth, channels,
        volume > 0 ? raopcl_float_volume(volume) : -144.0f
    );
    if (!p) {
        fprintf(stderr, "MSA-RAOP ERROR create\n");
        return 4;
    }
    if (!raopcl_connect(p, player, (uint16_t)port, volume > 0)) {
        fprintf(stderr, "MSA-RAOP ERROR connect\n");
        raopcl_destroy(p);
        return 5;
    }

    fprintf(stderr, "MSA-RAOP READY latency=%u sample_rate=%u bit_depth=%d channels=%d\n",
            raopcl_latency(p), raopcl_sample_rate(p), bit_depth, channels);
    fflush(stderr);

    int input_bpf = (bit_depth <= 16 ? 2 : 4) * channels;
    int alac_bpf = (bit_depth <= 16 ? 2 : 3) * channels;
    size_t pcm_bytes = (size_t)FRAMES_PER_CHUNK * (size_t)input_bpf;
    size_t alac_bytes = (size_t)FRAMES_PER_CHUNK * (size_t)alac_bpf;
    uint8_t *pcm = malloc(pcm_bytes);
    uint8_t *alac_buf = bit_depth > 16 ? malloc(alac_bytes) : NULL;
    if (!pcm || (bit_depth > 16 && !alac_buf)) {
        fprintf(stderr, "MSA-RAOP ERROR alloc\n");
        free(pcm); free(alac_buf); raopcl_disconnect(p); raopcl_destroy(p);
        return 6;
    }
    size_t pcm_len = 0;
    uint64_t last_seq = 0;
    uint64_t last_keepalive = raopcl_get_ntp(NULL);
    bool quit = false;

    while (!quit) {
        process_command(control, ack, metadata_path, artwork_path, &last_seq, p, &quit);
        if (quit) break;

        if (!raopcl_is_connected(p) || !raopcl_is_sane(p)) {
            fprintf(stderr, "MSA-RAOP ERROR health\n");
            fflush(stderr);
            break;
        }

        uint64_t now = raopcl_get_ntp(NULL);
        if (now - last_keepalive >= MS2NTP(KEEPALIVE_MS)) {
            raopcl_keepalive(p);
            last_keepalive = now;
        }

        if (raopcl_state(p) == RAOP_STREAMING && raopcl_accept_frames(p)) {
            int avail = available_stdin();
            if (avail > 0 && pcm_len < pcm_bytes) {
                int want = (int)(pcm_bytes - pcm_len);
                if (want > avail) want = avail;
#if WIN
                int got = _read(_fileno(stdin), pcm + pcm_len, want);
#else
                int got = read(fileno(stdin), pcm + pcm_len, want);
#endif
                if (got > 0) pcm_len += (size_t)got;
            }
            if (pcm_len == pcm_bytes) {
                uint8_t *send_buf = pcm;
                if (bit_depth > 16) {
                    truncate_32to24(pcm, (int)pcm_len, alac_buf);
                    send_buf = alac_buf;
                }
                uint64_t playtime = 0;
                if (!raopcl_send_chunk(p, send_buf, FRAMES_PER_CHUNK, &playtime)) {
                    fprintf(stderr, "MSA-RAOP ERROR send\n");
                    break;
                }
                uint64_t head = playtime + TS2NTP(FRAMES_PER_CHUNK, raopcl_sample_rate(p));
                fprintf(stderr, "MSA-RAOP HEAD audible_ms=%llu\n",
                        (unsigned long long)ntp_to_unix_ms(head));
                fflush(stderr);
                pcm_len = 0;
            }
        }

#if WIN
        Sleep(1);
#else
        usleep(1000);
#endif
    }

    free(pcm);
    free(alac_buf);
    raopcl_disconnect(p);
    raopcl_destroy(p);
    cross_ssl_free();
    netsock_close();
    return 0;
}
