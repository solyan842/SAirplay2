/*
 * RAOP : Client to control an AirPlay device, cli simple example
 *
 * Copyright (C) 2004 Shiro Ninomiya <shiron@snino.com>
 * Philippe <philippe_44@outlook.com>
 *
 * See LICENSE
 *
 */

#include <stdio.h>
#include <signal.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include "platform.h"

#if WIN
#include <conio.h>
#include <time.h>
#include <windows.h>
#else
#include <unistd.h>
#include <termios.h>
#include <sys/param.h>
#include <sys/time.h>
#if OSX || FREEBDS
#include <sys/resource.h>
#endif
#endif

#include "raop_client.h"
#include "cross_net.h"
#include "cross_ssl.h"
#include "cross_util.h"
#include "cross_log.h"

#define RAOP_SEC(ntp) ((uint32_t) ((ntp) >> 32))
#define RAOP_FRAC(ntp) ((uint32_t) (ntp))
#define RAOP_SECNTP(ntp) RAOP_SEC(ntp),RAOP_FRAC(ntp)

// debug level from tools & other elements
log_level	util_loglevel;
log_level	raop_loglevel;
log_level 	main_log;

// our debug level
log_level *loglevel =&main_log;

// different combination of debug levels per channel
struct debug_s {
	int main, raop, util;
} debug[] = { { lSILENCE, lSILENCE, lSILENCE },
			{ lERROR, lERROR, lERROR },
			{ lINFO, lERROR, lERROR },
			{ lINFO, lINFO, lERROR },
			{ lDEBUG, lERROR, lERROR },
			{ lDEBUG, lINFO, lERROR },
			{ lDEBUG, lDEBUG, lERROR },
			{ lSDEBUG, lINFO, lERROR },
			{ lSDEBUG, lDEBUG, lERROR },
			{ lSDEBUG, lSDEBUG, lERROR },
		};

typedef enum {
	STOPPED = 0,
	PAUSED,
	PLAYING,
} playback_status_t;

/*----------------------------------------------------------------------------*/
static int print_usage(char *argv[])
{
	char *name = strrchr(argv[0], '\\');

	name = (name) ? name + 1 :argv[0];

	printf("usage: %s <options> <server_ip> <filename ('-' for stdin)>\n"
			   "\t[-ntp <file>] write current NTP in <file> and exit\n"
			   "\t[-p <port number>]\n"
			   "\t[-v <volume> (0-100)]\n"
			   "\t[-V <runtime volume file>]\n"
			   "\t[-C <runtime command pipe>]\n"
			   "\t[-l <latency> (frames]\n"
			   "\t[-w <wait>]  (start after <wait> milliseconds)\n"
			   "\t[-n <start>] (start at NTP <start> + <wait>)\n"
			   "\t[-nf <start>] (start at NTP in <file> + <wait>)\n"
			   "\t[-e] audio payload encryption\n"
			   "\t[-u] for authentication (only if crypto present in TXT record)\n"
   			   "\t[-a] send ALAC compressed audio\n"
			   "\t[-s <secret>] (valid secret for AppleTV)\n"
			   "\t[-P <password>] (device password)\n"
			   "\t[-r] do AppleTV pairing\n"
			   "\t[-t <et>] (et field in mDNS - 4 for airport-express and used to detect MFi)\n"
			   "\t[-m <[0][,1][,2]>] (md in mDNS: metadata capabilties 0=text, 1=artwork, 2=progress)\n"
			   "\t[-d <debug level>] (0 = silent)\n"
			   "\t[-i] (interactive commands: 'p'=pause, 'r'=(re)start, 's'=stop, 'q'=exit, ' '=block)\n",
			   name);
	return -1;
}

#if !WIN
/*----------------------------------------------------------------------------*/
static int kbhit()
{
	struct timeval tv;

	fd_set fds;
	tv.tv_sec = 0;
	tv.tv_usec = 0;
	FD_ZERO(&fds);
	FD_SET(STDIN_FILENO, &fds); //STDIN_FILENO is 0
	select(STDIN_FILENO + 1, &fds, NULL, NULL, &tv);
	return FD_ISSET(STDIN_FILENO, &fds);
}

/*----------------------------------------------------------------------------*/
static void set_termio(bool canon) {
	struct termios ios = {0};

	tcgetattr(0, &ios);

	if (canon) {
		ios.c_lflag |= ICANON;
		ios.c_lflag |= ECHO;
		tcsetattr(0, TCSADRAIN, &ios);
	}
	else {
		ios.c_lflag &= ~ICANON;
		ios.c_lflag &= ~ECHO;
		ios.c_cc[VMIN] = 1;
		ios.c_cc[VTIME] = 0;
		tcsetattr(0, TCSANOW, &ios);
	}
}

/*----------------------------------------------------------------------------*/
static char _getch() {
	char buf;

	if (read(0, &buf, 1)) return buf;

	return '\0';
}
#endif


/*----------------------------------------------------------------------------*/
static void init_platform(bool interactive) {
	netsock_init();
#if !WIN
	if (interactive) set_termio(false);
#endif
	cross_ssl_load();
}

/*----------------------------------------------------------------------------*/
static void close_platform(bool interactive) {
	netsock_close();
#if !WIN
	if (interactive) set_termio(true);
#endif
	cross_ssl_free();
}

#if WIN
#define RAOP_SESSION_MIN_START_LEAD_MS 200
#define COMMAND_BUFFER_BYTES 8192

static uint64_t unix_ms_to_ntp(uint64_t ms) {
	return ((ms / 1000) << 32) | (((ms % 1000) << 32) / 1000);
}

static uint64_t ntp_to_unix_ms(uint64_t ntp) {
	return (ntp >> 32) * 1000ULL + (((ntp & 0xFFFFFFFFULL) * 1000ULL) >> 32);
}

static void resolve_commanded_start(uint64_t requested_unix_ms,
									uint64_t *audible_ntp,
									uint64_t *at_unix_ms) {
	uint64_t lead = MS2NTP(RAOP_SESSION_MIN_START_LEAD_MS);
	uint64_t floor = raopcl_get_ntp(NULL) + lead;
	uint64_t requested = requested_unix_ms ? unix_ms_to_ntp(requested_unix_ms) : 0;
	if (requested_unix_ms && requested >= floor) {
		*audible_ntp = requested;
		*at_unix_ms = requested_unix_ms;
		return;
	}
	/* Pinned Music Assistant raop_session.c: a non-zero infeasible request
	 * gets one extra lead of retry slack; zero takes the floor directly. */
	*audible_ntp = requested_unix_ms ? floor + lead : floor;
	*at_unix_ms = ntp_to_unix_ms(*audible_ntp);
}

static bool command_start_raop(struct raopcl_s *raopcl,
							   uint64_t requested_unix_ms,
							   bool *first_start_done,
							   playback_status_t *status) {
	raop_state_t state = raopcl_state(raopcl);
	if (!*first_start_done) {
		/* Exact first-START semantics of pinned raop_session_commit(). */
		if (state != RAOP_STREAMING && state != RAOP_FLUSHED) return false;
		raopcl_stop(raopcl);
		if (state == RAOP_STREAMING && !raopcl_flush(raopcl)) return false;
	} else {
		/* Exact warm START semantics: only valid after a real FLUSH. */
		if (state != RAOP_FLUSHED) return false;
	}

	uint64_t audible_ntp = 0, at_unix_ms = 0;
	resolve_commanded_start(requested_unix_ms, &audible_ntp, &at_unix_ms);
	uint64_t latency_ntp =
		TS2NTP(raopcl_latency(raopcl), raopcl_sample_rate(raopcl));
	if (!raopcl_start_at(raopcl, audible_ntp - latency_ntp)) return false;

	*first_start_done = true;
	*status = PLAYING;
	fprintf(stderr,
			"[STATUS] started requested_unix_ms=%" PRIu64 " at_unix_ms=%" PRIu64 "\n",
			requested_unix_ms, at_unix_ms);
	fflush(stderr);
	return true;
}

static bool service_command_pipe(HANDLE pipe,
								 struct raopcl_s *raopcl,
								 uint64_t *pending_start_unix_ms,
								 bool *first_start_done,
								 playback_status_t *status,
								 char *buffer,
								 size_t *used) {
	DWORD available = 0;
	if (!PeekNamedPipe(pipe, NULL, 0, NULL, &available, NULL)) {
		DWORD err = GetLastError();
		if (err == ERROR_BROKEN_PIPE || err == ERROR_NO_DATA) return false;
		return true;
	}
	if (!available) return true;

	while (available && *used < COMMAND_BUFFER_BYTES - 1) {
		DWORD room = (DWORD)(COMMAND_BUFFER_BYTES - 1 - *used);
		DWORD take = available < room ? available : room;
		DWORD got = 0;
		if (!ReadFile(pipe, buffer + *used, take, &got, NULL)) {
			DWORD err = GetLastError();
			if (err == ERROR_BROKEN_PIPE || err == ERROR_NO_DATA) return false;
			break;
		}
		*used += got;
		buffer[*used] = '\0';

		char *line = buffer;
		char *end = NULL;
		while ((end = strchr(line, '\n')) != NULL) {
			*end = '\0';
			if (end > line && end[-1] == '\r') end[-1] = '\0';
			char *sep = strchr(line, '=');
			if (sep && sep != line) {
				*sep = '\0';
				const char *key = line;
				const char *value = sep + 1;
				if (!strcmp(key, "START_UNIX_MS")) {
					*pending_start_unix_ms = _strtoui64(value, NULL, 10);
				} else if (!strcmp(key, "ACTION") && !strcmp(value, "START")) {
					if (!command_start_raop(
							raopcl, *pending_start_unix_ms,
							first_start_done, status)) {
						fprintf(stderr,
								"[STATUS] error code=start_failed http=0 detail=\"RAOP START rejected\"\n");
						fflush(stderr);
					}
					*pending_start_unix_ms = 0;
				}
			}
			line = end + 1;
		}
		size_t remaining = *used - (size_t)(line - buffer);
		memmove(buffer, line, remaining);
		*used = remaining;
		buffer[*used] = '\0';

		if (!PeekNamedPipe(pipe, NULL, 0, NULL, &available, NULL)) {
			DWORD err = GetLastError();
			if (err == ERROR_BROKEN_PIPE || err == ERROR_NO_DATA) return false;
			break;
		}
	}
	return true;
}
#endif

/*----------------------------------------------------------------------------*/
/*																			  */
/*----------------------------------------------------------------------------*/
int main(int argc, char *argv[]) {
	struct raopcl_s *raopcl;
	char *fname = NULL;
	int port = 5000;
	char* passwd = NULL;
	int volume = 50, runtime_volume = 50, wait = 0, latency = MS2TS(1000, 44100);
	struct {
		struct hostent *hostent;
		char *name;
		struct in_addr addr;
	} player = { 0 };
	int infile;
	uint8_t *buf;
	int i, n = -1, level = 2;
	playback_status_t status;
	raop_crypto_t crypto = RAOP_CLEAR;
	uint64_t start = 0, start_at = 0, last = 0, frames = 0, last_volume_check = 0;
	bool interactive = false, alac = false, pairing = false;
	char *secret = NULL, *md = NULL, *et = NULL, *volume_file = NULL;
	char *command_pipe_name = NULL;
#if WIN
	HANDLE command_pipe = INVALID_HANDLE_VALUE;
	char command_buf[COMMAND_BUFFER_BYTES] = {0};
	size_t command_used = 0;
	uint64_t pending_start_unix_ms = 0;
	bool first_start_done = false;
#endif
	bool auth = false;
	struct in_addr host = { INADDR_ANY };

	for(i = 1; i < argc; i++){
		if(!strcmp(argv[i],"-ntp")){
			FILE *out;

			out = fopen(argv[2], "w");
			fprintf(out, "%" PRIu64, raopcl_get_ntp(NULL));
			fclose(out);
			exit(0);
		}

		if (!strcmp(argv[i],"-p")) {
			port=atoi(argv[++i]);
		} else if (!strcmp(argv[i],"-v")) {
			volume=atoi(argv[++i]);
		} else if (!strcmp(argv[i],"-V")) {
			volume_file=argv[++i];
		} else if (!strcmp(argv[i],"-C")) {
			command_pipe_name=argv[++i];
		} else if (!strcmp(argv[i],"-w")) {
			wait=atoi(argv[++i]);
		} else if(!strcmp(argv[i],"-l")) {
			latency=atoi(argv[++i]);
		} else if (!strcmp(argv[i],"-i")) {
			interactive = true;
		} else if(!strcmp(argv[i],"-s")) {
			secret = argv[++i];
		} else if (!strcmp(argv[i],"-m")) {
			md = argv[++i];
		} else if(!strcmp(argv[i],"-t")) {
			et = argv[++i];
		} else if (!strcmp(argv[i], "-u")) {
			auth = true;
		} else if (!strcmp(argv[i],"-a")) {
			alac = true;
		} else if (!strcmp(argv[i], "-r")) {
			pairing = true;
		} else if(!strcmp(argv[i],"-n")) {
			sscanf(argv[++i], "%" PRIu64, &start);
		} else if (!strcmp(argv[i],"-nf")) {
			FILE *in;

			in = fopen(argv[++i], "r");
			if (!in || !fscanf(in, "%" PRIu64, &start)) {
				LOG_ERROR("Cannot read NTP from file %s", argv[i]);
			}
			fclose(in);
		} else if(!strcmp(argv[i],"-d")) {
			level = atoi(argv[++i]);
			if (level >= sizeof(debug) / sizeof(struct debug_s)) {
				level = sizeof(debug) / sizeof(struct debug_s) - 1;
			}
		} else if(!strcmp(argv[i],"-e")) {
			crypto = RAOP_RSA;
			continue;
		}
		else if (!strcmp(argv[i], "-P")) {
			passwd = argv[++i];
			continue;
		} else if(!strcmp(argv[i],"--help") || !strcmp(argv[i],"-h")) {
			return print_usage(argv);
		} else if (!player.name) {
			player.name = argv[i];
		} else if (!fname) {
			fname=argv[i];
		}
	}

	util_loglevel = debug[level].util;
	raop_loglevel = debug[level].raop;
	main_log = debug[level].main;

	if (!player.name && !pairing) return print_usage(argv);
	if (!fname && !pairing) return print_usage(argv);

	if (!strcmp(fname, "-")) {
		infile = fileno(stdin);
		interactive = false;
	} else if ((infile = open(fname, O_RDONLY)) == -1) {
		LOG_ERROR("cannot open file %s", fname);
		close_platform(interactive);
		exit(1);
	}

#if WIN
	setmode(infile, O_BINARY);
#endif

	init_platform(interactive);

	// if required, pair with appleTV
	if (pairing) AppleTVpairing(NULL, NULL, &secret);
	
	// create the raop context
	if ((raopcl = raopcl_create(host, 0, 0, NULL, NULL, alac ? RAOP_ALAC : RAOP_PCM, DEFAULT_FRAMES_PER_CHUNK,
								latency, crypto, auth, secret, passwd, et, md,
								44100, 16, 2,
								raopcl_float_volume(volume))) == NULL) {
		LOG_ERROR("Cannot init RAOP %p", raopcl);
		close_platform(interactive);
		exit(1);
	}

	// get player's address
	player.hostent = gethostbyname(player.name);
	if (!player.hostent) {
		LOG_ERROR("Cannot resolve name %s", player.name);
		goto exit;
	}

	memcpy(&player.addr.s_addr, player.hostent->h_addr_list[0], player.hostent->h_length);

#if WIN
	if (command_pipe_name) {
		/* Windows equivalent of Music Assistant's POSIX --cmdpipe FIFO.
		 * Audio remains exclusively on stdin; only newline-delimited control
		 * commands travel here. Create it before receiver connect so the Rust
		 * controller can attach without serialising member RTSP connects. */
		command_pipe = CreateNamedPipeA(
			command_pipe_name,
			PIPE_ACCESS_INBOUND,
			PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
			1, 0, COMMAND_BUFFER_BYTES, 0, NULL);
		if (command_pipe == INVALID_HANDLE_VALUE) {
			LOG_ERROR("Cannot create runtime command pipe %s (%lu)",
					  command_pipe_name, GetLastError());
			goto exit;
		}
		BOOL pipe_connected = ConnectNamedPipe(command_pipe, NULL) ?
			TRUE : (GetLastError() == ERROR_PIPE_CONNECTED);
		if (!pipe_connected) {
			LOG_ERROR("Cannot connect runtime command pipe %s (%lu)",
					  command_pipe_name, GetLastError());
			CloseHandle(command_pipe);
			command_pipe = INVALID_HANDLE_VALUE;
			goto exit;
		}
	}
#endif

	// connect to player
	if (!raopcl_connect(raopcl, player.addr, port, true)) {
		LOG_ERROR("Cannot connect to AirPlay device %s:%hu, check firewall & port", inet_ntoa(player.addr), port);
		goto exit;
	}

	latency = raopcl_latency(raopcl);

	LOG_INFO("connected to %s on port %d, player latency is %d ms", inet_ntoa(player.addr),
			 port, (int) TS2MS(latency, raopcl_sample_rate(raopcl)));

	if (start || wait) {
		uint64_t now = raopcl_get_ntp(NULL);

		start_at = (start ? start : now) + MS2NTP(wait) -
					TS2NTP(latency, raopcl_sample_rate(raopcl));

		LOG_INFO("now %u.%u, audio starts at NTP %u.%u (in %u ms)", RAOP_SECNTP(now), RAOP_SECNTP(start_at),
				 (start_at + TS2NTP(latency, raopcl_sample_rate(raopcl)) > now) ?
				  (uint32_t) NTP2MS(start_at - now + TS2NTP(latency, raopcl_sample_rate(raopcl))) :
				  0);

		raopcl_start_at(raopcl, start_at);
	}

	start = raopcl_get_ntp(NULL);
	/* MSA command-session shape: when a command pipe exists, connection and
	 * audio input may be ready but nothing is sent until ACTION=START. */
	status = command_pipe_name ? PAUSED : PLAYING;
	runtime_volume = volume;

	buf = malloc(DEFAULT_FRAMES_PER_CHUNK * 4);
	
	do {
		uint64_t playtime, now;

		now = raopcl_get_ntp(NULL);

#if WIN
		if (command_pipe != INVALID_HANDLE_VALUE) {
			if (!service_command_pipe(
					command_pipe, raopcl, &pending_start_unix_ms,
					&first_start_done, &status,
					command_buf, &command_used)) {
				status = STOPPED;
				break;
			}
		}
#endif

		if (volume_file && now - last_volume_check >= MS2NTP(100)) {
			FILE *vf;
			int next_volume;
			last_volume_check = now;
			vf = fopen(volume_file, "r");
			if (vf) {
				if (fscanf(vf, "%d", &next_volume) == 1 &&
					next_volume >= 0 && next_volume <= 100 &&
					next_volume != runtime_volume) {
					runtime_volume = next_volume;
					raopcl_set_volume(raopcl, raopcl_float_volume(runtime_volume));
					LOG_INFO("runtime volume %d%%", runtime_volume);
				}
				fclose(vf);
			}
		}

		if (now - last > MS2NTP(1000)) {
			last = now;
			if (frames && frames > raopcl_latency(raopcl)) {
				LOG_INFO("at %u.%u (%" PRIu64 " ms after start), played %" PRIu64 " ms",
						  RAOP_SECNTP(now), NTP2MS(now - start),
						  TS2MS(frames - raopcl_latency(raopcl), raopcl_sample_rate(raopcl)));
			}
		}

		if (status == PLAYING && raopcl_accept_frames(raopcl)) {
			n = read(infile, buf, DEFAULT_FRAMES_PER_CHUNK * 4);
			if (!n)	continue;
			raopcl_send_chunk(raopcl, buf, n / 4, &playtime);
			frames += n / 4;
		}

		if (interactive && kbhit()) {
			char c = _getch();

			switch (c) {
			case 'p':
				if (status == PLAYING) {
					raopcl_pause(raopcl);
					raopcl_flush(raopcl);
					status = PAUSED;
					LOG_INFO("Pause at : %u.%u", RAOP_SECNTP(raopcl_get_ntp(NULL)));
				}
				break;
			case 's':
				raopcl_stop(raopcl);
				raopcl_flush(raopcl);
				status = STOPPED;
				LOG_INFO("Stopped at : %u.%u", RAOP_SECNTP(raopcl_get_ntp(NULL)));
				break;
			case 'r': {
				uint64_t now = raopcl_get_ntp(NULL);
				uint64_t start_at = now + MS2NTP(200) - TS2NTP(latency, raopcl_sample_rate(raopcl));

				status = PLAYING;
				raopcl_start_at(raopcl, start_at);
				LOG_INFO("Re-started at : %u.%u", RAOP_SECNTP(start_at));
				}
				break;
			case 'q':
				raopcl_disconnect(raopcl);
				raopcl_destroy(raopcl);
				free(buf);
				close_platform(interactive);
				exit(0);
			case ' ':
				_getch();
				break;
			default: break;
			}
		}

	} while (n || raopcl_is_playing(raopcl));

	free(buf);
	raopcl_disconnect(raopcl);

exit:
#if WIN
	if (command_pipe != INVALID_HANDLE_VALUE) {
		DisconnectNamedPipe(command_pipe);
		CloseHandle(command_pipe);
	}
#endif
	raopcl_destroy(raopcl);
	close_platform(interactive);
	return 0;
}
