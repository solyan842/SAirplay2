#include "strict_clock_shim.h"

#include <stdint.h>

static volatile LONG g_strict_ntp_clock = 0;

__declspec(dllexport) void sr_raop_set_strict_ntp_clock(int enabled)
{
    InterlockedExchange(&g_strict_ntp_clock, enabled ? 1 : 0);
}

void WINAPI sr_clock_GetSystemTimeAsFileTime(LPFILETIME file_time)
{
    FILETIME raw;
    ULARGE_INTEGER ticks;
    const ULONGLONG unix_epoch_filetime = 116444736000000000ULL;

    GetSystemTimeAsFileTime(&raw);
    if (!file_time) return;

    if (InterlockedCompareExchange(&g_strict_ntp_clock, 0, 0) == 0) {
        *file_time = raw;
        return;
    }

    ticks.LowPart = raw.dwLowDateTime;
    ticks.HighPart = raw.dwHighDateTime;
    if (ticks.QuadPart > unix_epoch_filetime) {
        ticks.QuadPart -= unix_epoch_filetime;
    } else {
        ticks.QuadPart = 0;
    }
    file_time->dwLowDateTime = ticks.LowPart;
    file_time->dwHighDateTime = ticks.HighPart;
}
