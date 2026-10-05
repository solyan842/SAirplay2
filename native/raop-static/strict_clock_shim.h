#ifndef SAIRPLAY_RAOP_STRICT_CLOCK_SHIM_H
#define SAIRPLAY_RAOP_STRICT_CLOCK_SHIM_H

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>

#ifdef __cplusplus
extern "C" {
#endif

/* crosstools cross_util.c is compiled with its Windows FILETIME call redirected
 * here. Default mode returns the raw FILETIME value exactly, preserving the
 * hardware-locked SOtM path. A separately loaded strict DLL instance can opt in
 * to standards-correct NTP by subtracting the FILETIME->Unix epoch offset. */
void WINAPI sr_clock_GetSystemTimeAsFileTime(LPFILETIME file_time);
__declspec(dllexport) void sr_raop_set_strict_ntp_clock(int enabled);

#ifdef __cplusplus
}
#endif

#endif /* SAIRPLAY_RAOP_STRICT_CLOCK_SHIM_H */
