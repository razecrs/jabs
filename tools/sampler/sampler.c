/* A sampling profiler for a running native JABS program on Windows: every interval it suspends
 * each thread, walks its stack (dbghelp StackWalk64, which reads the program's .pdata unwind
 * tables, so no debug info is needed), resumes it, and writes one line per thread. It is a
 * development tool; it never ships in a JABS program. symbolize.py turns its output into a
 * profile of Java methods.
 *
 * Build:  clang -O2 sampler.c -o sampler.exe -ldbghelp -lpsapi -lwinmm
 * Run:    sampler <pid> <seconds> <interval ms> <out file> [thread name filter]
 *
 * Output: "# base 0x<main module base>", then "<thread name>\t<pc> <return address> ..." per
 * thread per sample, addresses absolute and in hex. A thread without a description is named
 * "tid-<id>". Taking the snapshot of every thread costs tens of milliseconds, so the real rate
 * is lower than the interval asks for. Found the 1.7.10 server's getClass scan (public 9e0cad3)
 * and its interface dispatch cost (31a49d2). */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <dbghelp.h>
#include <psapi.h>
#include <tlhelp32.h>
#include <timeapi.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>

#define MAX_THREADS 512
#define MAX_FRAMES 96

typedef HRESULT(WINAPI *get_description_fn)(HANDLE, PWSTR *);

static get_description_fn get_description;

static void thread_name(HANDLE thread, DWORD tid, char *out, size_t size) {
    snprintf(out, size, "tid-%lu", (unsigned long)tid);
    if (!get_description) return;
    PWSTR wide = NULL;
    if (SUCCEEDED(get_description(thread, &wide)) && wide && wide[0]) {
        WideCharToMultiByte(CP_UTF8, 0, wide, -1, out, (int)size, NULL, NULL);
        for (char *c = out; *c; c++) if (*c == '\t' || *c == '\n') *c = ' ';
    }
    if (wide) LocalFree(wide);
}

int main(int argc, char **argv) {
    if (argc < 5) {
        fprintf(stderr, "usage: sampler <pid> <seconds> <interval ms> <out file> [thread name filter]\n");
        return 2;
    }
    DWORD pid = (DWORD)strtoul(argv[1], NULL, 10);
    double seconds = atof(argv[2]);
    DWORD interval = (DWORD)strtoul(argv[3], NULL, 10);
    const char *filter = argc > 5 ? argv[5] : NULL;
    get_description = (get_description_fn)(void *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "GetThreadDescription");

    HANDLE process = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, FALSE, pid);
    if (!process) { fprintf(stderr, "OpenProcess(%lu) failed: %lu\n", (unsigned long)pid, GetLastError()); return 1; }
    if (!SymInitialize(process, NULL, TRUE)) { fprintf(stderr, "SymInitialize failed: %lu\n", GetLastError()); return 1; }
    HMODULE main_module = NULL;
    DWORD needed = 0;
    if (!EnumProcessModules(process, &main_module, sizeof main_module, &needed)) {
        fprintf(stderr, "EnumProcessModules failed: %lu\n", GetLastError());
        return 1;
    }
    FILE *out = fopen(argv[4], "w");
    if (!out) { perror(argv[4]); return 1; }
    fprintf(out, "# base 0x%llx\n", (unsigned long long)(uintptr_t)main_module);

    LARGE_INTEGER freq, start, now;
    QueryPerformanceFrequency(&freq);
    QueryPerformanceCounter(&start);
    timeBeginPeriod(1);
    unsigned long samples = 0, stacks = 0;
    for (;;) {
        QueryPerformanceCounter(&now);
        if ((double)(now.QuadPart - start.QuadPart) / (double)freq.QuadPart >= seconds) break;
        HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if (snap == INVALID_HANDLE_VALUE) break;
        THREADENTRY32 entry;
        entry.dwSize = sizeof entry;
        int alive = 0;
        for (BOOL ok = Thread32First(snap, &entry); ok; ok = Thread32Next(snap, &entry)) {
            if (entry.th32OwnerProcessID != pid) continue;
            alive = 1;
            HANDLE thread = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_LIMITED_INFORMATION,
                                       FALSE, entry.th32ThreadID);
            if (!thread) continue;
            char name[256];
            thread_name(thread, entry.th32ThreadID, name, sizeof name);
            if (filter && !strstr(name, filter)) { CloseHandle(thread); continue; }
            if (SuspendThread(thread) == (DWORD)-1) { CloseHandle(thread); continue; }
            CONTEXT ctx;
            memset(&ctx, 0, sizeof ctx);
            ctx.ContextFlags = CONTEXT_FULL;
            DWORD64 frames[MAX_FRAMES];
            int depth = 0;
            if (GetThreadContext(thread, &ctx)) {
                STACKFRAME64 frame;
                memset(&frame, 0, sizeof frame);
                frame.AddrPC.Offset = ctx.Rip;
                frame.AddrPC.Mode = AddrModeFlat;
                frame.AddrFrame.Offset = ctx.Rbp;
                frame.AddrFrame.Mode = AddrModeFlat;
                frame.AddrStack.Offset = ctx.Rsp;
                frame.AddrStack.Mode = AddrModeFlat;
                while (depth < MAX_FRAMES &&
                       StackWalk64(IMAGE_FILE_MACHINE_AMD64, process, thread, &frame, &ctx, NULL,
                                   SymFunctionTableAccess64, SymGetModuleBase64, NULL)) {
                    if (!frame.AddrPC.Offset) break;
                    frames[depth++] = frame.AddrPC.Offset;
                }
            }
            ResumeThread(thread);
            CloseHandle(thread);
            if (depth == 0) continue;
            fprintf(out, "%s\t", name);
            for (int i = 0; i < depth; i++) fprintf(out, i ? " %llx" : "%llx", (unsigned long long)frames[i]);
            fputc('\n', out);
            stacks++;
        }
        CloseHandle(snap);
        if (!alive) break;
        samples++;
        Sleep(interval);
    }
    timeEndPeriod(1);
    fclose(out);
    fprintf(stderr, "%lu samples, %lu stacks\n", samples, stacks);
    return 0;
}
