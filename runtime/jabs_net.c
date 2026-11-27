/* java.net and java.nio natives (the bodies are in src/cgen/net.rs): the name resolver behind
 * InetAddress, and NIO's sockets and selectors. Each function mirrors the JDK's own native
 * (libnet, libnio) on the same OS: the same OS calls, the same result order, the same exception
 * classes and messages, so a program sees what it sees on the JVM. Java objects are built and
 * read through generated glue (jabs_net_new_inet4, jabs_net_inet_read, jabs_net_raise, ...), so
 * this file knows no field layout. Windows uses Winsock 2 (link ws2_32), started once. */
/* POSIX 2008: getaddrinfo, getnameinfo and gethostname are hidden by -std=c11 unless asked for. */
#ifndef _WIN32
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#endif
#ifdef _WIN32
/* rand_s (NativeSeedGenerator's seed) is declared only when asked for, before stdlib.h. */
#ifndef _CRT_RAND_S
#define _CRT_RAND_S
#endif
/* winsock2.h must come before any windows.h (which would pull in the old winsock.h). */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <mstcpip.h>
#include <windows.h>
#else
#include <errno.h>
#include <netdb.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>
#endif
#include <limits.h>
#include "jabs_runtime.h"

/* Longest host name the resolver hands back (NI_MAXHOST, which strict POSIX headers hide). */
#define JABS_NET_HOST_CAP 1025
/* Most addresses one lookup returns that JABS keeps; a resolver answering more is broken. */
#define JABS_NET_ADDRESS_CAP 4096

/* InetAddress lookup characteristics (java.net.spi.InetAddressResolver.LookupPolicy). */
#define JABS_LOOKUP_IPV4 1
#define JABS_LOOKUP_IPV6 2
#define JABS_LOOKUP_IPV4_FIRST 4
#define JABS_LOOKUP_IPV6_FIRST 8

/* A String argument as malloc'd UTF-8; NULL with NullPointerException(message) for null. */
static char *net_utf8_arg(jabs_object *s, const char *message) {
    if (!s) { jabs_raise_message(JABS_VM_NULL_POINTER, message); return NULL; }
    char *p = jabs_string_to_utf8(s);
    if (!p) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
    return p;
}

#ifdef _WIN32
/* ---- Winsock: started once, as the JDK's libnet does when it loads ---- */

static INIT_ONCE winsock_once = INIT_ONCE_STATIC_INIT;
static int winsock_status = -1;

static BOOL CALLBACK winsock_start(PINIT_ONCE once, PVOID parameter, PVOID *context) {
    (void)once; (void)parameter; (void)context;
    WSADATA data;
    winsock_status = WSAStartup(MAKEWORD(2, 2), &data);
    return TRUE;
}

/* Every Winsock call goes through here first. Without Winsock no socket can work at all, and
 * the JDK's libnet would not have loaded: stop. */
static void net_ready(void) {
    if (!InitOnceExecuteOnce(&winsock_once, winsock_start, NULL, NULL)) jabs_not_compiled("WSAStartup could not run (InitOnceExecuteOnce failed; jabs_net.c)");
    if (winsock_status != 0) jabs_not_compiled("WSAStartup failed: Winsock 2.2 is not available (jabs_net.c)");
}

/* A malloc'd UTF-8 copy of a wide string; NULL when it cannot be converted. */
static char *net_narrow(const wchar_t *w) {
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, NULL, 0, NULL, NULL);
    if (n <= 0) return NULL;
    char *s = (char *)malloc((size_t)n);
    if (s && WideCharToMultiByte(CP_UTF8, 0, w, -1, s, n, NULL, NULL) != n) { free(s); return NULL; }
    return s;
}

/* A malloc'd wide copy of a UTF-8 string; NULL when it cannot be converted. */
static wchar_t *net_wide(const char *s) {
    int n = MultiByteToWideChar(CP_UTF8, 0, s, -1, NULL, 0);
    if (n <= 0) return NULL;
    wchar_t *w = (wchar_t *)malloc((size_t)n * sizeof(wchar_t));
    if (w && MultiByteToWideChar(CP_UTF8, 0, s, -1, w, n) != n) { free(w); return NULL; }
    return w;
}

/* The system's text for a Win32/Winsock error as UTF-8 in buf, trimmed as the JDK's
 * getLastErrorString trims it (one final LF, CR and '.'); 0 when there is none. */
static size_t net_error_text(DWORD code, char *buf, size_t size) {
    wchar_t text[512];
    DWORD n = FormatMessageW(FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS, NULL, code, 0, text, 512, NULL);
    if (n == 0 || n >= 512) { if (size > 0) buf[0] = 0; return 0; }
    if (n > 3) {
        if (text[n - 1] == L'\n') n--;
        if (text[n - 1] == L'\r') n--;
        if (text[n - 1] == L'.') n--;
    }
    text[n] = 0;
    int written = WideCharToMultiByte(CP_UTF8, 0, text, -1, buf, (int)size, NULL, NULL);
    if (written <= 0) { if (size > 0) buf[0] = 0; return 0; }
    return strlen(buf);
}

/* JNU_ThrowByNameWithMessageAndLastError: "<last error> (<detail>)", the error alone without
 * a detail, the detail alone without an error text. */
static void net_raise_last_error(int32_t kind, DWORD code, const char *detail) {
    char text[1024];
    size_t n = net_error_text(code, text, sizeof text);
    size_t detail_length = detail ? strlen(detail) : 0;
    if (n == 0) { jabs_net_raise(kind, detail); return; }
    if (detail_length == 0) { jabs_net_raise(kind, text); return; }
    if (detail_length > SIZE_MAX - n - 4) jabs_not_compiled("an exception detail too long to format (net_raise_last_error, jabs_net.c)");
    size_t size = n + detail_length + 4;
    char *message = (char *)malloc(size);
    if (!message) { jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return; }
    snprintf(message, size, "%s (%s)", text, detail);
    jabs_net_raise(kind, message);
    free(message);
}

/* Whether a socket of this family can be created (the JDK's IPv4_supported/IPv6_supported). */
static int net_family_works(int family) {
    net_ready();
    SOCKET s = socket(family, SOCK_STREAM, 0);
    if (s == INVALID_SOCKET) return 0;
    closesocket(s);
    return 1;
}
#else
/* Whether a socket of this family can be created; for IPv6 on Linux, also whether any
 * interface has an IPv6 address (/proc/net/if_inet6 has a line), as the JDK's IPv6_supported. */
static int net_family_works(int family) {
    int fd = socket(family, SOCK_STREAM, 0);
    if (fd < 0) return 0;
    close(fd);
#ifdef __linux__
    if (family == AF_INET6) {
        FILE *f = fopen("/proc/net/if_inet6", "r");
        if (!f) return 0;
        char line[255];
        char *got = fgets(line, sizeof line, f);
        fclose(f);
        if (!got) return 0;
    }
#endif
    return 1;
}
#endif

/* ---- Address families: decided once, as libnet's JNI_OnLoad does ---- */

/* -1 until decided; then 0 or 1. Two threads may both decide; they reach the same answer. */
static _Atomic int32_t ipv4_state = -1;
static _Atomic int32_t ipv6_state = -1;

int32_t jabs_net_ipv4_available(void) {
    int32_t state = atomic_load(&ipv4_state);
    if (state >= 0) return state;
    state = net_family_works(AF_INET);
    atomic_store(&ipv4_state, state);
    return state;
}

/* IPv6 is available when the OS supports it and java.net.preferIPv4Stack is not true. */
int32_t jabs_net_ipv6_available(void) {
    int32_t state = atomic_load(&ipv6_state);
    if (state >= 0) return state;
    int32_t prefer_ipv4 = jabs_net_prefer_ipv4_stack();
    if (prefer_ipv4 < 0) return 0;
    state = !prefer_ipv4 && net_family_works(AF_INET6);
    atomic_store(&ipv6_state, state);
    return state;
}

/* ---- The resolver: Inet4AddressImpl and Inet6AddressImpl ---- */

/* InetAddressImpl.getLocalHostName: gethostname, or "localhost" when it fails. */
jabs_object *jabs_net_local_host_name(void) {
#ifdef _WIN32
    net_ready();
    char name[256];
    if (gethostname(name, (int)sizeof name) == SOCKET_ERROR) strcpy(name, "localhost");
    name[sizeof name - 1] = 0;
    /* The JDK makes a String in the platform (ANSI) code page; go through UTF-16 to UTF-8. */
    int n = MultiByteToWideChar(CP_ACP, 0, name, -1, NULL, 0);
    if (n <= 0) jabs_not_compiled("a host name the ANSI code page cannot decode (jabs_net_local_host_name, jabs_net.c)");
    wchar_t *wide = (wchar_t *)malloc((size_t)n * sizeof(wchar_t));
    if (!wide) { jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }
    if (MultiByteToWideChar(CP_ACP, 0, name, -1, wide, n) != n) jabs_not_compiled("MultiByteToWideChar failed on the host name (jabs_net_local_host_name, jabs_net.c)");
    char *utf8 = net_narrow(wide);
    free(wide);
    if (!utf8) { jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }
    jabs_object *s = jabs_string_from_utf8(utf8);
    free(utf8);
    return s;
#else
    char name[JABS_NET_HOST_CAP + 1];
    name[0] = 0;
    if (gethostname(name, sizeof name) != 0) strcpy(name, "localhost");
    name[JABS_NET_HOST_CAP] = 0;
    return jabs_string_from_utf8(name);
#endif
}

/* One resolved address: family (AF_INET or AF_INET6), bytes in network order, IPv6 scope. */
typedef struct {
    int family;
    uint8_t bytes[16];
    uint32_t scope;
} net_address;

/* Append a getaddrinfo result unless it repeats one already kept (the JDK keeps the first of
 * each). Returns 0 when the cap is reached. */
static int keep_address(net_address *kept, int32_t *count, int family, const struct sockaddr *sa) {
    net_address a;
    memset(&a, 0, sizeof a);
    a.family = family;
    if (family == AF_INET) {
        const struct sockaddr_in *v4 = (const struct sockaddr_in *)sa;
        memcpy(a.bytes, &v4->sin_addr, 4);
    } else {
        const struct sockaddr_in6 *v6 = (const struct sockaddr_in6 *)sa;
        memcpy(a.bytes, &v6->sin6_addr, 16);
        a.scope = (uint32_t)v6->sin6_scope_id;
    }
    size_t length = family == AF_INET ? 4 : 16;
    for (int32_t i = 0; i < *count; i++) {
        if (kept[i].family == family && memcmp(kept[i].bytes, a.bytes, length) == 0) return 1;
    }
    if (*count >= JABS_NET_ADDRESS_CAP) return 0;
    kept[(*count)++] = a;
    return 1;
}

/* The InetAddress[] of the kept addresses, each named `host`. IPv4 first, IPv6 first, or in
 * the resolver's order (lookupCharacteristicsToAddressPreference). */
static jabs_object *address_array(const net_address *kept, int32_t count, int32_t characteristics, jabs_object *host) {
    int32_t ipv4_first = (characteristics & JABS_LOOKUP_IPV4_FIRST) != 0, ipv6_first = (characteristics & JABS_LOOKUP_IPV6_FIRST) != 0;
    int32_t v4_count = 0;
    for (int32_t i = 0; i < count; i++) v4_count += kept[i].family == AF_INET;
    int32_t v4_at = 0, v6_at = 0;
    if (ipv4_first && !ipv6_first) v6_at = v4_count;
    else if (ipv6_first && !ipv4_first) v4_at = count - v4_count;
    int system_order = ipv4_first == ipv6_first;
    jabs_object *array = jabs_net_new_inet_array(count);
    if (!array) return NULL;
    jabs_object **slots = (jabs_object **)jabs_array_data((jabs_array *)array);
    for (int32_t i = 0; i < count; i++) {
        jabs_object *a;
        int32_t at;
        if (kept[i].family == AF_INET) {
            uint32_t v = ((uint32_t)kept[i].bytes[0] << 24) | ((uint32_t)kept[i].bytes[1] << 16) | ((uint32_t)kept[i].bytes[2] << 8) | kept[i].bytes[3];
            a = jabs_net_new_inet4((int32_t)v, host);
            at = system_order ? i : v4_at++;
        } else {
            if (kept[i].scope > INT32_MAX) jabs_not_compiled("an IPv6 scope id above int (address_array, jabs_net.c)");
            a = jabs_net_new_inet6(kept[i].bytes, (int32_t)kept[i].scope, host);
            at = system_order ? i : v6_at++;
        }
        if (!a) return NULL;
        if (at < 0 || at >= count) jabs_not_compiled("an address slot outside the array (address_array, jabs_net.c)");
        slots[at] = a;
    }
    return array;
}

/* lookupCharacteristicsToAddressFamily: one of IPV4/IPV6 alone picks that family. */
static int lookup_family(int32_t characteristics) {
    int32_t ipv4 = characteristics & JABS_LOOKUP_IPV4, ipv6 = characteristics & JABS_LOOKUP_IPV6;
    if (ipv4 && !ipv6) return AF_INET;
    if (!ipv4 && ipv6) return AF_INET6;
    return AF_UNSPEC;
}

/* InetAddressImpl.lookupAllHostAddr: getaddrinfo (AI_CANONNAME, the family the characteristics
 * ask for), duplicates dropped, ordered as asked. A failure is an UnknownHostException with the
 * JDK's message for this OS; the host is named only under jdk.includeInExceptions=hostInfo. */
jabs_object *jabs_net_lookup(jabs_object *host, int32_t characteristics) {
    char *name = net_utf8_arg(host, "host argument is null");
    if (!name) return NULL;
    net_address *kept = (net_address *)malloc(sizeof(net_address) * JABS_NET_ADDRESS_CAP);
    if (!kept) { free(name); jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }
    int32_t count = 0;
    jabs_object *result = NULL;
#ifdef _WIN32
    net_ready();
    /* The wide API, so a name outside the ANSI code page is looked up as written. */
    wchar_t *wide = net_wide(name);
    if (!wide) { free(kept); free(name); jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }
    ADDRINFOW hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_flags = AI_CANONNAME;
    hints.ai_family = lookup_family(characteristics);
    int error = GetAddrInfoW(wide, NULL, &hints, &res);
    free(wide);
    if (error != 0) {
        /* NET_ThrowByNameWithLastError: the system's text for the error, then the host. */
        int32_t enhanced = jabs_net_enhanced_host_info();
        if (enhanced >= 0) net_raise_last_error(JABS_NET_UNKNOWN_HOST, (DWORD)error, enhanced ? name : NULL);
        free(kept);
        free(name);
        return NULL;
    }
    int complete = 1;
    for (ADDRINFOW *it = res; it && complete; it = it->ai_next) {
        if (it->ai_family != AF_INET && it->ai_family != AF_INET6) continue;
        if (!it->ai_addr) continue;
        complete = keep_address(kept, &count, it->ai_family, it->ai_addr);
    }
    FreeAddrInfoW(res);
#else
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_flags = AI_CANONNAME;
    hints.ai_family = lookup_family(characteristics);
    int error = getaddrinfo(name, NULL, &hints, &res);
    if (error != 0) {
        /* NET_ThrowUnknownHostExceptionWithGaiError: "<host>: <gai text>", or " <gai text>". */
        int32_t enhanced = jabs_net_enhanced_host_info();
        if (enhanced >= 0) {
            const char *why = gai_strerror(error);
            if (!why) why = "unknown error";
            size_t size = strlen(name) + strlen(why) + 3;
            char *message = (char *)malloc(size);
            if (!message) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
            else {
                if (enhanced) snprintf(message, size, "%s: %s", name, why);
                else snprintf(message, size, " %s", why);
                jabs_net_raise(JABS_NET_UNKNOWN_HOST, message);
                free(message);
            }
        }
        free(kept);
        free(name);
        return NULL;
    }
    int complete = 1;
    for (struct addrinfo *it = res; it && complete; it = it->ai_next) {
        if (it->ai_family != AF_INET && it->ai_family != AF_INET6) continue;
        if (!it->ai_addr) continue;
        complete = keep_address(kept, &count, it->ai_family, it->ai_addr);
    }
    freeaddrinfo(res);
#endif
    free(name);
    if (!complete) jabs_not_compiled("a host lookup with more than JABS_NET_ADDRESS_CAP addresses (jabs_net_lookup, jabs_net.c)");
    result = address_array(kept, count, characteristics, host);
    free(kept);
    return result;
}

/* InetAddressImpl.getHostByAddr: getnameinfo with NI_NAMEREQD; no name is an
 * UnknownHostException without a message. */
jabs_object *jabs_net_host_by_addr(jabs_object *address) {
    if (!address) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }
    jabs_array *bytes = (jabs_array *)address;
    if (bytes->length != 4 && bytes->length != 16) jabs_not_compiled("getHostByAddr of an address that is neither 4 nor 16 bytes (jabs_net_host_by_addr, jabs_net.c)");
    struct sockaddr_storage storage;
    memset(&storage, 0, sizeof storage);
    int length;
    if (bytes->length == 4) {
        struct sockaddr_in *v4 = (struct sockaddr_in *)&storage;
        v4->sin_family = AF_INET;
        memcpy(&v4->sin_addr, jabs_array_data(bytes), 4);
        length = (int)sizeof *v4;
    } else {
        struct sockaddr_in6 *v6 = (struct sockaddr_in6 *)&storage;
        v6->sin6_family = AF_INET6;
        memcpy(&v6->sin6_addr, jabs_array_data(bytes), 16);
        length = (int)sizeof *v6;
    }
#ifdef _WIN32
    net_ready();
    wchar_t wide[JABS_NET_HOST_CAP];
    if (GetNameInfoW((const struct sockaddr *)&storage, length, wide, JABS_NET_HOST_CAP, NULL, 0, NI_NAMEREQD) != 0) {
        jabs_net_raise(JABS_NET_UNKNOWN_HOST, NULL);
        return NULL;
    }
    wide[JABS_NET_HOST_CAP - 1] = 0;
    char *name = net_narrow(wide);
    if (!name) { jabs_net_raise(JABS_NET_UNKNOWN_HOST, NULL); return NULL; }
    jabs_object *s = jabs_string_from_utf8(name);
    free(name);
    return s;
#else
    char name[JABS_NET_HOST_CAP + 1];
    if (getnameinfo((const struct sockaddr *)&storage, (socklen_t)length, name, sizeof name, NULL, 0, NI_NAMEREQD) != 0) {
        jabs_net_raise(JABS_NET_UNKNOWN_HOST, NULL);
        return NULL;
    }
    name[JABS_NET_HOST_CAP] = 0;
    return jabs_string_from_utf8(name);
#endif
}

/* ---- NIO sockets: sun.nio.ch.Net, IOUtil, SocketDispatcher ---- */

/* sun.nio.ch.IOStatus: what a native returns instead of a count. */
#define JABS_IOS_EOF (-1)
#define JABS_IOS_UNAVAILABLE (-2)
#define JABS_IOS_INTERRUPTED (-3)
#define JABS_IOS_THROWN (-5)

/* sun.nio.ch.Net.SHUT_RD / SHUT_WR / SHUT_RDWR. */
#define JABS_NET_SHUT_RD 0
#define JABS_NET_SHUT_WR 1

/* Any socket address JABS hands the OS or reads back. */
typedef union {
    struct sockaddr sa;
    struct sockaddr_in v4;
    struct sockaddr_in6 v6;
    struct sockaddr_storage storage;
} net_sockaddr;

/* The descriptor a FileDescriptor holds (NIO keeps sockets in its fd field, on Windows too). */
static int32_t fd_of(jabs_object *fdo) {
    if (!fdo) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return -1; }
    return *jabs_descriptor_fd_slot(fdo);
}

static void net_raise_io(const char *message) { jabs_raise_message(JABS_VM_IO, message); }

#ifdef _WIN32
typedef SOCKET net_socket;
/* A FileDescriptor's int back to a SOCKET (-1 becomes INVALID_SOCKET, as the JDK's cast). */
static SOCKET as_socket(int32_t fd) { return (SOCKET)(intptr_t)fd; }

/* A new SOCKET as the int Java keeps; the JDK assumes every socket handle fits, so a handle
 * that does not is a stop rather than a truncated descriptor. */
static int32_t socket_value(SOCKET s, const char *where) {
    if ((uintptr_t)s > (uintptr_t)INT32_MAX) jabs_not_compiled(where);
    return (int32_t)(uintptr_t)s;
}

/* The JDK's winsock_errors table (net_util_md.c): exception kind and text per Winsock error;
 * kind -1 is SocketException. */
static const struct { int code; int32_t kind; const char *text; } winsock_errors[] = {
    { WSAEACCES, -1, "Permission denied" },
    { WSAEADDRINUSE, JABS_NET_BIND, "Address already in use" },
    { WSAEADDRNOTAVAIL, JABS_NET_BIND, "Cannot assign requested address" },
    { WSAEAFNOSUPPORT, -1, "Address family not supported by protocol family" },
    { WSAEALREADY, -1, "Operation already in progress" },
    { WSAECONNABORTED, -1, "Software caused connection abort" },
    { WSAECONNREFUSED, JABS_NET_CONNECT, "Connection refused" },
    { WSAECONNRESET, -1, "Connection reset by peer" },
    { WSAEDESTADDRREQ, -1, "Destination address required" },
    { WSAEFAULT, -1, "Bad address" },
    { WSAEHOSTDOWN, -1, "Host is down" },
    { WSAEHOSTUNREACH, JABS_NET_NO_ROUTE, "No route to host" },
    { WSAEINPROGRESS, -1, "Operation now in progress" },
    { WSAEINTR, -1, "Interrupted function call" },
    { WSAEINVAL, -1, "Invalid argument" },
    { WSAEISCONN, -1, "Socket is already connected" },
    { WSAEMFILE, -1, "Too many open files" },
    { WSAEMSGSIZE, -1, "The message is larger than the maximum supported by the underlying transport" },
    { WSAENETDOWN, -1, "Network is down" },
    { WSAENETRESET, -1, "Network dropped connection on reset" },
    { WSAENETUNREACH, -1, "Network is unreachable" },
    { WSAENOBUFS, -1, "No buffer space available (maximum connections reached?)" },
    { WSAENOPROTOOPT, -1, "Bad protocol option" },
    { WSAENOTCONN, -1, "Socket is not connected" },
    { WSAENOTSOCK, -1, "Socket operation on nonsocket" },
    { WSAEOPNOTSUPP, -1, "Operation not supported" },
    { WSAEPFNOSUPPORT, -1, "Protocol family not supported" },
    { WSAEPROCLIM, -1, "Too many processes" },
    { WSAEPROTONOSUPPORT, -1, "Protocol not supported" },
    { WSAEPROTOTYPE, -1, "Protocol wrong type for socket" },
    { WSAESHUTDOWN, -1, "Cannot send after socket shutdown" },
    { WSAESOCKTNOSUPPORT, -1, "Socket type not supported" },
    { WSAETIMEDOUT, JABS_NET_CONNECT, "Connection timed out" },
    { WSATYPE_NOT_FOUND, -1, "Class type not found" },
    { WSAEWOULDBLOCK, -1, "Resource temporarily unavailable" },
    { WSAHOST_NOT_FOUND, -1, "Host not found" },
    { WSA_NOT_ENOUGH_MEMORY, -1, "Insufficient memory available" },
    { WSANOTINITIALISED, -1, "Successful WSAStartup not yet performed" },
    { WSANO_DATA, -1, "Valid name, no data record of requested type" },
    { WSANO_RECOVERY, -1, "This is a nonrecoverable error" },
    { WSASYSCALLFAILURE, -1, "System call failure" },
    { WSASYSNOTREADY, -1, "Network subsystem is unavailable" },
    { WSATRY_AGAIN, -1, "This is usually a temporary error during hostname resolution and means that the local server did not receive a response from an authoritative server" },
    { WSAVERNOTSUPPORTED, -1, "Winsock.dll version out of range" },
    { WSAEDISCON, -1, "Graceful shutdown in progress" },
    { WSA_OPERATION_ABORTED, -1, "Overlapped operation aborted" },
};

/* NET_ThrowNew: "<text>: <detail>" with the table's exception, or SocketException with
 * "Unrecognized Windows Sockets error: <n>: <detail>"; a NULL detail is "no further information". */
static void net_throw_new(int code, const char *detail) {
    if (jabs_pending) return;
    if (!detail) detail = "no further information";
    char message[256];
    int32_t kind = JABS_NET_SOCKET;
    size_t count = sizeof winsock_errors / sizeof winsock_errors[0];
    size_t i = 0;
    while (i < count && winsock_errors[i].code != code) i++;
    if (i < count) {
        snprintf(message, sizeof message, "%s: %s", winsock_errors[i].text, detail);
        if (winsock_errors[i].kind >= 0) kind = winsock_errors[i].kind;
    } else {
        snprintf(message, sizeof message, "Unrecognized Windows Sockets error: %d: %s", code, detail);
    }
    jabs_net_raise(kind, message);
}

/* handleSocketError (Windows nio): NET_ThrowNew without a detail; IOS_THROWN. */
static int32_t socket_error(int code) {
    net_throw_new(code, NULL);
    return JABS_IOS_THROWN;
}

/* JNU_ThrowIOExceptionWithLastError: the system's text for the error, else the default. */
static void raise_io_last_error(DWORD code, const char *fallback) {
    char text[1024];
    if (net_error_text(code, text, sizeof text) > 0) net_raise_io(text);
    else net_raise_io(fallback);
}
#else
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/resource.h>
#include <sys/uio.h>
#include <pthread.h>
#include <limits.h>

/* The system's text for an errno value (thread-safe; handles GNU vs XSI strerror_r). */
static const char *errno_text_gnu(char *text, char *buf, size_t size, int code) {
    (void)buf; (void)size; (void)code;
    return text;
}

static const char *errno_text_xsi(int rc, char *buf, size_t size, int code) {
    if (rc != 0) snprintf(buf, size, "error %d", code);
    return buf;
}

/* Which strerror_r <string.h> declared depends on feature macros set before the first include
   anywhere, so pick by its return type rather than by _GNU_SOURCE here. */
static const char *errno_text(int code, char *buf, size_t size) {
    return _Generic(strerror_r(code, buf, size),
                    char *: errno_text_gnu,
                    default: errno_text_xsi)(strerror_r(code, buf, size), buf, size, code);
}

/* JNU_ThrowByNameWithLastError: the errno text, or the default when errno is 0. */
static void raise_errno(int32_t kind, int code, const char *fallback) {
    char text[256];
    const char *message = code != 0 ? errno_text(code, text, sizeof text) : fallback;
    if (kind < 0) net_raise_io(message);
    else jabs_net_raise(kind, message);
}

/* handleSocketError (Unix nio): the exception the errno maps to, with its text. */
static int32_t socket_error(int code) {
    if (code == EINPROGRESS) return 0;
    int32_t kind = JABS_NET_SOCKET;
    switch (code) {
#ifdef EPROTO
    case EPROTO: kind = JABS_NET_PROTOCOL; break;
#endif
    case ECONNREFUSED: case ETIMEDOUT: case ENOTCONN: kind = JABS_NET_CONNECT; break;
    case EHOSTUNREACH: kind = JABS_NET_NO_ROUTE; break;
    case EADDRINUSE: case EADDRNOTAVAIL: case EACCES: kind = JABS_NET_BIND; break;
    default: break;
    }
    raise_errno(kind, code, "NioSocketError");
    return JABS_IOS_THROWN;
}
#endif

/* NET_InetAddressToSockaddr: an IPv6 address (IPv4 ones mapped, except the wildcard) when IPv6
 * is available and the caller does not ask for plain IPv4; else IPv4, where an IPv6 address
 * is SocketException("Protocol family unavailable"). Returns 0, or -1 with an exception pending. */
static int inet_to_sockaddr(jabs_object *address, int32_t port, net_sockaddr *out, int *length, int32_t v4_mapped) {
    uint8_t bytes[16];
    int32_t scope = 0;
    int32_t family = jabs_net_inet_read(address, bytes, &scope);
    if (family == 0) return -1;
    memset(out, 0, sizeof *out);
    if (jabs_net_ipv6_available() && !(family == 1 && !v4_mapped)) {
        uint8_t mapped[16];
        memset(mapped, 0, sizeof mapped);
        if (family == 1) {
            /* The IPv4 wildcard stays the IPv6 wildcard, as the JDK prefers. */
            if (bytes[0] | bytes[1] | bytes[2] | bytes[3]) {
                mapped[10] = 0xff;
                mapped[11] = 0xff;
                memcpy(mapped + 12, bytes, 4);
            }
        } else {
            memcpy(mapped, bytes, 16);
        }
        out->v6.sin6_family = AF_INET6;
        out->v6.sin6_port = htons((uint16_t)port);
        memcpy(&out->v6.sin6_addr, mapped, 16);
        if (family == 2) out->v6.sin6_scope_id = (uint32_t)scope;
        *length = (int)sizeof out->v6;
        return 0;
    }
    if (family != 1) { jabs_net_raise(JABS_NET_SOCKET, "Protocol family unavailable"); return -1; }
    out->v4.sin_family = AF_INET;
    out->v4.sin_port = htons((uint16_t)port);
    memcpy(&out->v4.sin_addr, bytes, 4);
    *length = (int)sizeof out->v4;
    return 0;
}

/* NET_SockaddrToInetAddress: an Inet4Address for IPv4 and IPv4-mapped IPv6 addresses, else an
 * Inet6Address with its scope id; no host name. The port goes to *port. */
static jabs_object *sockaddr_to_inet(const net_sockaddr *from, int32_t *port) {
    if (from->sa.sa_family == AF_INET6) {
        const uint8_t *b = (const uint8_t *)&from->v6.sin6_addr;
        static const uint8_t prefix[12] = { 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff };
        *port = ntohs(from->v6.sin6_port);
        if (memcmp(b, prefix, 12) == 0) {
            uint32_t v = ((uint32_t)b[12] << 24) | ((uint32_t)b[13] << 16) | ((uint32_t)b[14] << 8) | b[15];
            return jabs_net_new_inet4((int32_t)v, NULL);
        }
        if (from->v6.sin6_scope_id > INT32_MAX) jabs_not_compiled("an IPv6 scope id above int (sockaddr_to_inet, jabs_net.c)");
        return jabs_net_new_inet6(b, (int32_t)from->v6.sin6_scope_id, NULL);
    }
    if (from->sa.sa_family == AF_INET) {
        const uint8_t *b = (const uint8_t *)&from->v4.sin_addr;
        uint32_t v = ((uint32_t)b[0] << 24) | ((uint32_t)b[1] << 16) | ((uint32_t)b[2] << 8) | b[3];
        *port = ntohs(from->v4.sin_port);
        return jabs_net_new_inet4((int32_t)v, NULL);
    }
    jabs_not_compiled("a socket address of neither IPv4 nor IPv6 (sockaddr_to_inet, jabs_net.c)");
}

/* Net.socket0: a TCP or UDP socket, IPv6 (dual-stack) when asked and available. */
int32_t jabs_net_socket(int32_t prefer_ipv6, int32_t stream, int32_t reuse, int32_t fast_loopback) {
    int type = stream ? SOCK_STREAM : SOCK_DGRAM;
#ifdef _WIN32
    net_ready();
    (void)reuse; /* the JDK's Windows socket0 ignores it too */
    int domain = prefer_ipv6 ? AF_INET6 : AF_INET;
    SOCKET s = socket(domain, type, 0);
    if (s == INVALID_SOCKET) { net_throw_new(WSAGetLastError(), "socket"); return -1; }
    int32_t value = socket_value(s, "a socket handle above int (jabs_net_socket, jabs_net.c)");
    if (!SetHandleInformation((HANDLE)s, HANDLE_FLAG_INHERIT, 0)) jabs_not_compiled("SetHandleInformation failed on a new socket (jabs_net_socket, jabs_net.c)");
    /* IPV6_V6ONLY is on by default on Windows: a dual-stack socket needs it off. */
    if (domain == AF_INET6) {
        int off = 0;
        setsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY, (const char *)&off, sizeof off);
    }
    if (!stream) {
        closesocket(s);
        jabs_not_compiled("Net.socket0 for a datagram socket (SIO_UDP_CONNRESET handling is not implemented; jabs_net_socket, jabs_net.c)");
    }
    if (stream && fast_loopback) {
        /* SIO_LOOPBACK_FAST_PATH, asked by jdk.net.useFastTcpLoopback; failure is ignored. */
        int on = 1;
        DWORD returned = 0;
        WSAIoctl(s, SIO_LOOPBACK_FAST_PATH, &on, sizeof on, NULL, 0, &returned, NULL, NULL);
    }
    return value;
#else
    (void)fast_loopback;
    int domain = (jabs_net_ipv6_available() && prefer_ipv6) ? AF_INET6 : AF_INET;
    int fd = socket(domain, type, 0);
    if (fd < 0) return socket_error(errno);
    /* With IPv4 available, a dual-stack socket. */
    if (domain == AF_INET6 && jabs_net_ipv4_available()) {
        int off = 0;
        if (setsockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &off, sizeof off) < 0) {
            int e = errno;
            close(fd);
            raise_errno(JABS_NET_SOCKET, e, "Unable to set IPV6_V6ONLY");
            return -1;
        }
    }
    if (reuse) {
        int on = 1;
        if (setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &on, sizeof on) < 0) {
            int e = errno;
            close(fd);
            raise_errno(JABS_NET_SOCKET, e, "Unable to set SO_REUSEADDR");
            return -1;
        }
    }
    if (type == SOCK_DGRAM) {
        close(fd);
        jabs_not_compiled("Net.socket0 for a datagram socket (its multicast defaults are not implemented; jabs_net_socket, jabs_net.c)");
    }
    return fd;
#endif
}

/* Net.bind0: bind, with SO_EXCLUSIVEADDRUSE first on Windows when exclusive (NET_WinBind). */
void jabs_net_bind(jabs_object *fdo, int32_t prefer_ipv6, int32_t exclusive, jabs_object *address, int32_t port) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return;
    net_sockaddr sa;
    int length = 0;
    if (inet_to_sockaddr(address, port, &sa, &length, prefer_ipv6) != 0) return;
#ifdef _WIN32
    net_ready();
    SOCKET s = as_socket(fd);
    if (exclusive) {
        int on = 1;
        if (setsockopt(s, SOL_SOCKET, SO_EXCLUSIVEADDRUSE, (const char *)&on, sizeof on) == SOCKET_ERROR) {
            net_throw_new(WSAGetLastError(), "bind");
            return;
        }
    }
    if (bind(s, &sa.sa, length) == SOCKET_ERROR) net_throw_new(WSAGetLastError(), "bind");
#else
    (void)exclusive;
    if (bind(fd, &sa.sa, (socklen_t)length) != 0) socket_error(errno);
#endif
}

void jabs_net_listen(jabs_object *fdo, int32_t backlog) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return;
#ifdef _WIN32
    net_ready();
    if (listen(as_socket(fd), backlog) == SOCKET_ERROR) net_throw_new(WSAGetLastError(), "listen");
#else
    if (listen(fd, backlog) < 0) socket_error(errno);
#endif
}

#ifdef _WIN32
/* NET_EnableFastTcpLoopbackConnect: no SYN retransmissions to loopback, so a refused connect
 * fails at once instead of after Windows's 2-second retries. Failure is ignored, as the JDK's. */
static void fast_loopback_connect(SOCKET s) {
    TCP_INITIAL_RTO_PARAMETERS rto = { TCP_INITIAL_RTO_UNSPECIFIED_RTT, TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS };
    DWORD returned = 0;
    if (WSAIoctl(s, SIO_TCP_INITIAL_RTO, &rto, sizeof rto, NULL, 0, &returned, NULL, NULL) == 0) return;
    /* Before Windows 10 2004 the minimum was one retransmission. */
    rto.MaxSynRetransmissions = 1;
    WSAIoctl(s, SIO_TCP_INITIAL_RTO, &rto, sizeof rto, NULL, 0, &returned, NULL, NULL);
}

static int is_loopback(const net_sockaddr *sa) {
    if (sa->sa.sa_family == AF_INET) return ((const uint8_t *)&sa->v4.sin_addr)[0] == 127;
    const uint8_t *b = (const uint8_t *)&sa->v6.sin6_addr;
    static const uint8_t loop6[16] = { 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1 };
    static const uint8_t mapped[12] = { 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff };
    return memcmp(b, loop6, 16) == 0 || (memcmp(b, mapped, 12) == 0 && b[12] == 127);
}
#endif

/* Net.connect0: 1 when connected, IOS_UNAVAILABLE when a non-blocking connect is under way. */
int32_t jabs_net_connect(int32_t prefer_ipv6, jabs_object *fdo, jabs_object *address, int32_t port) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return JABS_IOS_THROWN;
    net_sockaddr sa;
    int length = 0;
    if (inet_to_sockaddr(address, port, &sa, &length, prefer_ipv6) != 0) return JABS_IOS_THROWN;
#ifdef _WIN32
    net_ready();
    SOCKET s = as_socket(fd);
    int type = 0, type_length = sizeof type;
    int type_ok = getsockopt(s, SOL_SOCKET, SO_TYPE, (char *)&type, &type_length) == 0;
    if (type_ok && type == SOCK_STREAM && is_loopback(&sa)) fast_loopback_connect(s);
    if (connect(s, &sa.sa, length) != 0) {
        int e = WSAGetLastError();
        if (e == WSAEINPROGRESS || e == WSAEWOULDBLOCK) return JABS_IOS_UNAVAILABLE;
        net_throw_new(e, "connect");
        return JABS_IOS_THROWN;
    }
    return 1;
#else
    if (connect(fd, &sa.sa, (socklen_t)length) != 0) {
        if (errno == EINPROGRESS) return JABS_IOS_UNAVAILABLE;
        if (errno == EINTR) return JABS_IOS_INTERRUPTED;
        return socket_error(errno);
    }
    return 1;
#endif
}

/* Net.accept: the new socket into newfdo and the peer's InetSocketAddress into addresses[0];
 * 1, or IOS_UNAVAILABLE when none is pending on a non-blocking socket. */
int32_t jabs_net_accept(jabs_object *fdo, jabs_object *newfdo, jabs_object *addresses) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return JABS_IOS_THROWN;
    if (!newfdo || !addresses) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return JABS_IOS_THROWN; }
    if (((jabs_array *)addresses)->length < 1) { jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_INDEX); return JABS_IOS_THROWN; }
    net_sockaddr sa;
    memset(&sa, 0, sizeof sa);
    int32_t newfd;
#ifdef _WIN32
    net_ready();
    int length = (int)sizeof sa;
    SOCKET s = accept(as_socket(fd), &sa.sa, &length);
    if (s == INVALID_SOCKET) {
        int e = WSAGetLastError();
        if (e == WSAEWOULDBLOCK) return JABS_IOS_UNAVAILABLE;
        raise_io_last_error((DWORD)e, "Accept failed");
        return JABS_IOS_THROWN;
    }
    newfd = socket_value(s, "an accepted socket handle above int (jabs_net_accept, jabs_net.c)");
    if (!SetHandleInformation((HANDLE)s, HANDLE_FLAG_INHERIT, 0)) jabs_not_compiled("SetHandleInformation failed on an accepted socket (jabs_net_accept, jabs_net.c)");
#else
    socklen_t length = sizeof sa;
    int got = -1;
    /* ECONNABORTED: the peer gave up before accept; take the next one, as the JDK does. */
    for (;;) {
        got = accept(fd, &sa.sa, &length);
        if (got >= 0 || errno != ECONNABORTED) break;
        length = sizeof sa;
    }
    if (got < 0) {
        if (errno == EAGAIN || errno == EWOULDBLOCK) return JABS_IOS_UNAVAILABLE;
        if (errno == EINTR) return JABS_IOS_INTERRUPTED;
        raise_errno(-1, errno, "Accept failed");
        return JABS_IOS_THROWN;
    }
    newfd = got;
#endif
    *jabs_descriptor_fd_slot(newfdo) = newfd;
    int32_t port = 0;
    jabs_object *remote = sockaddr_to_inet(&sa, &port);
    if (!remote) return JABS_IOS_THROWN;
    jabs_object *isa = jabs_net_new_socket_address(remote, port);
    if (!isa) return JABS_IOS_THROWN;
    ((jabs_object **)jabs_array_data((jabs_array *)addresses))[0] = isa;
    return 1;
}

/* getsockname or getpeername of a descriptor; 0, or -1 with the exception raised. */
static int socket_name(jabs_object *fdo, int peer, net_sockaddr *sa) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return -1;
    memset(sa, 0, sizeof *sa);
#ifdef _WIN32
    net_ready();
    int length = (int)sizeof *sa;
    int r = peer ? getpeername(as_socket(fd), &sa->sa, &length) : getsockname(as_socket(fd), &sa->sa, &length);
    if (r == SOCKET_ERROR) { net_throw_new(WSAGetLastError(), peer ? "getpeername" : "getsockname"); return -1; }
#else
    socklen_t length = sizeof *sa;
    int r = peer ? getpeername(fd, &sa->sa, &length) : getsockname(fd, &sa->sa, &length);
    if (r < 0) { socket_error(errno); return -1; }
#endif
    return 0;
}

int32_t jabs_net_port(jabs_object *fdo, int32_t peer) {
    net_sockaddr sa;
    if (socket_name(fdo, peer, &sa) != 0) return JABS_IOS_THROWN;
    if (sa.sa.sa_family == AF_INET) return ntohs(sa.v4.sin_port);
    if (sa.sa.sa_family == AF_INET6) return ntohs(sa.v6.sin6_port);
    jabs_not_compiled("Net.localPort/remotePort of a socket of neither IPv4 nor IPv6 (jabs_net_port, jabs_net.c)");
}

jabs_object *jabs_net_address(jabs_object *fdo, int32_t peer) {
    net_sockaddr sa;
    if (socket_name(fdo, peer, &sa) != 0) return NULL;
    int32_t port = 0;
    return sockaddr_to_inet(&sa, &port);
}

/* Net.shutdown: how is SHUT_RD, SHUT_WR or SHUT_RDWR; on Linux a socket not yet connected is
 * not an error, as in the JDK. */
void jabs_net_shutdown(jabs_object *fdo, int32_t how) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return;
#ifdef _WIN32
    net_ready();
    int mode = how == JABS_NET_SHUT_RD ? SD_RECEIVE : how == JABS_NET_SHUT_WR ? SD_SEND : SD_BOTH;
    if (shutdown(as_socket(fd), mode) == SOCKET_ERROR) net_throw_new(WSAGetLastError(), "shutdown");
#else
    int mode = how == JABS_NET_SHUT_RD ? SHUT_RD : how == JABS_NET_SHUT_WR ? SHUT_WR : SHUT_RDWR;
    if (shutdown(fd, mode) < 0 && errno != ENOTCONN) socket_error(errno);
#endif
}

/* Net.getIntOption0: an int option, SO_LINGER as its seconds or -1 when off. The level and
 * option numbers are the OS's own (SocketOptionRegistry is generated per platform). */
int32_t jabs_net_get_option(jabs_object *fdo, int32_t may_need_conversion, int32_t level, int32_t option) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return -1;
    int result = 0;
    struct linger linger;
    memset(&linger, 0, sizeof linger);
    int is_linger = level == SOL_SOCKET && option == SO_LINGER;
#ifdef _WIN32
    net_ready();
    char *arg = is_linger ? (char *)&linger : (char *)&result;
    int length = is_linger ? (int)sizeof linger : (int)sizeof result;
    /* IP_TOS and IPV6_TCLASS go through NET_GetSockOpt on Windows whatever the caller said. */
    if ((level == IPPROTO_IP && option == IP_TOS) || (level == IPPROTO_IPV6 && option == IPV6_TCLASS)) may_need_conversion = 1;
    int r;
    if (may_need_conversion && level == IPPROTO_IPV6 && option == IPV6_TCLASS) {
        /* NET_GetSockOpt: traffic class is not supported, it reads as 0. */
        result = 0;
        r = 0;
    } else {
        r = getsockopt(as_socket(fd), level, option, arg, &length);
        if (r == SOCKET_ERROR && may_need_conversion && WSAGetLastError() == WSAENOPROTOOPT && level == IPPROTO_IP && option == IP_TOS) {
            jabs_not_compiled("Net.getIntOption0(IP_TOS) on a Windows edition without it (the JDK reads the registry's default TOS; jabs_net_get_option, jabs_net.c)");
        }
    }
    if (r == SOCKET_ERROR) { socket_error(WSAGetLastError()); return JABS_IOS_THROWN; }
#else
    unsigned char byte = 0;
    int is_byte = level == IPPROTO_IP && (option == IP_MULTICAST_TTL || option == IP_MULTICAST_LOOP);
    void *arg = is_linger ? (void *)&linger : is_byte ? (void *)&byte : (void *)&result;
    socklen_t length = is_linger ? sizeof linger : is_byte ? sizeof byte : sizeof result;
    if (getsockopt(fd, level, option, arg, &length) < 0) {
        raise_errno(JABS_NET_SOCKET, errno, "sun.nio.ch.Net.getIntOption");
        return -1;
    }
#ifdef __linux__
    /* NET_GetSockOpt: Linux reports buffer sizes doubled (its own bookkeeping); halve them. */
    if (may_need_conversion && level == SOL_SOCKET && (option == SO_SNDBUF || option == SO_RCVBUF)) result /= 2;
#endif
    if (is_byte) return byte;
#endif
    if (is_linger) return linger.l_onoff ? (int32_t)linger.l_linger : -1;
    return result;
}

/* Net.setIntOption0, with NET_SetSockOpt's adjustments when may_need_conversion. */
void jabs_net_set_option(jabs_object *fdo, int32_t may_need_conversion, int32_t level, int32_t option, int32_t value, int32_t is_ipv6) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return;
    int arg = value;
    struct linger linger;
    memset(&linger, 0, sizeof linger);
    int is_linger = level == SOL_SOCKET && option == SO_LINGER;
    if (is_linger && value >= 0) { linger.l_onoff = 1; linger.l_linger = (unsigned short)value; }
    /* IP_TOS sets only the TOS and precedence bits, not ECN. */
    if (may_need_conversion && level == IPPROTO_IP && option == IP_TOS) arg &= 0x1e | 0xe0;
#ifdef _WIN32
    net_ready();
    (void)is_ipv6;
    /* The JDK's Windows setIntOption0 ignores IPV6_TCLASS. */
    if (level == IPPROTO_IPV6 && option == IPV6_TCLASS) return;
    SOCKET s = as_socket(fd);
    const char *parg = is_linger ? (const char *)&linger : (const char *)&arg;
    int length = is_linger ? (int)sizeof linger : (int)sizeof arg;
    if (may_need_conversion && option == SO_REUSEADDR && level == SOL_SOCKET) {
        /* Not over SO_EXCLUSIVEADDRUSE: NET_SetSockOpt leaves the socket as it is. */
        int exclusive = 0, exclusive_length = sizeof exclusive;
        if (getsockopt(s, SOL_SOCKET, SO_EXCLUSIVEADDRUSE, (char *)&exclusive, &exclusive_length) == 0 && exclusive == 1) return;
    }
    if (setsockopt(s, level, option, parg, length) == SOCKET_ERROR) {
        int e = WSAGetLastError();
        /* NET_SetSockOpt: IP_TOS and IP_MULTICAST_LOOP may be unsupported, IP_TOS unsettable on
         * an unbound UDP socket; neither is an error. */
        if (may_need_conversion && level == IPPROTO_IP && ((e == WSAENOPROTOOPT && (option == IP_TOS || option == IP_MULTICAST_LOOP)) || (e == WSAEINVAL && option == IP_TOS))) return;
        socket_error(e);
    }
#else
    unsigned char byte = (unsigned char)value;
    int is_byte = level == IPPROTO_IP && (option == IP_MULTICAST_TTL || option == IP_MULTICAST_LOOP);
    const void *parg = is_linger ? (const void *)&linger : is_byte ? (const void *)&byte : (const void *)&arg;
    socklen_t length = is_linger ? sizeof linger : is_byte ? sizeof byte : sizeof arg;
#ifdef __linux__
    /* NET_SetSockOpt: Linux takes no receive buffer under 1024 bytes. */
    if (may_need_conversion && level == SOL_SOCKET && option == SO_RCVBUF && arg < 1024) arg = 1024;
#endif
    if (setsockopt(fd, level, option, parg, length) < 0) raise_errno(JABS_NET_SOCKET, errno, "sun.nio.ch.Net.setIntOption");
#ifdef __linux__
    /* IPV6_TCLASS on a dual-stack socket also sets the IPv4 TOS. */
    if (level == IPPROTO_IPV6 && option == IPV6_TCLASS && is_ipv6) setsockopt(fd, IPPROTO_IP, IP_TOS, parg, length);
#else
    (void)is_ipv6;
#endif
#endif
}

/* Net.poll: one descriptor's ready events (Net.POLL* values, which are the OS's), 0 on timeout.
 * Windows uses select, as the JDK's does there. */
int32_t jabs_net_poll(jabs_object *fdo, int32_t events, int64_t timeout) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return JABS_IOS_THROWN;
#ifdef _WIN32
    net_ready();
    SOCKET s = as_socket(fd);
    fd_set rd, wr, ex;
    FD_ZERO(&rd); FD_ZERO(&wr); FD_ZERO(&ex);
    if (events & POLLIN) FD_SET(s, &rd);
    if (events & POLLOUT) FD_SET(s, &wr);
    FD_SET(s, &ex);
    struct timeval t;
    t.tv_sec = (long)(timeout / 1000);
    t.tv_usec = (long)((timeout % 1000) * 1000);
    int rv = select(0, &rd, &wr, &ex, timeout >= 0 ? &t : NULL);
    if (rv == SOCKET_ERROR) return socket_error(WSAGetLastError());
    int32_t ready = 0;
    if (FD_ISSET(s, &rd)) ready |= POLLIN;
    if (FD_ISSET(s, &wr)) ready |= POLLOUT;
    if (FD_ISSET(s, &ex)) ready |= POLLERR;
    return ready;
#else
    struct pollfd p;
    p.fd = fd;
    p.events = (short)events;
    p.revents = 0;
    int wait = timeout < -1 ? -1 : timeout > INT_MAX ? INT_MAX : (int)timeout;
    int rv = poll(&p, 1, wait);
    if (rv >= 0) return p.revents;
    if (errno == EINTR) return 0;
    return socket_error(errno);
#endif
}

/* Net.pollConnect: whether a non-blocking connect finished within the timeout; a failed one
 * raises its error. */
int32_t jabs_net_poll_connect(jabs_object *fdo, int64_t timeout) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return 0;
#ifdef _WIN32
    net_ready();
    SOCKET s = as_socket(fd);
    fd_set wr, ex;
    FD_ZERO(&wr); FD_ZERO(&ex);
    FD_SET(s, &wr);
    FD_SET(s, &ex);
    struct timeval t;
    t.tv_sec = (long)(timeout / 1000);
    t.tv_usec = (long)((timeout % 1000) * 1000);
    int rv = select(0, NULL, &wr, &ex, timeout >= 0 ? &t : NULL);
    if (rv == SOCKET_ERROR) { socket_error(WSAGetLastError()); return 0; }
    if (rv == 0) return 0;
    if (FD_ISSET(s, &wr) && !FD_ISSET(s, &ex)) return 1;
    int error = 0, length = sizeof error;
    if (getsockopt(s, SOL_SOCKET, SO_ERROR, (char *)&error, &length) == SOCKET_ERROR) {
        int e = WSAGetLastError();
        if (e == WSAEINPROGRESS) return 0;
        net_throw_new(e, NULL);
        return 0;
    }
    if (error == 0) return 1;
    socket_error(error);
    return 0;
#else
    struct pollfd p;
    p.fd = fd;
    p.events = POLLOUT;
    p.revents = 0;
    int wait = timeout < -1 ? -1 : timeout > INT_MAX ? INT_MAX : (int)timeout;
    int rv = poll(&p, 1, wait);
    if (rv > 0) {
        int error = 0;
        socklen_t length = sizeof error;
        if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &length) < 0) { socket_error(errno); return 0; }
        if (error) { socket_error(error); return 0; }
        if (p.revents & POLLHUP) { socket_error(ENOTCONN); return 0; }
        return 1;
    }
    if (rv == 0 || errno == EINTR) return 0;
    raise_errno(-1, errno, "poll failed");
    return 0;
#endif
}

/* Net.available: bytes readable without blocking (FIONREAD). */
int32_t jabs_net_available(jabs_object *fdo) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return JABS_IOS_THROWN;
#ifdef _WIN32
    net_ready();
    u_long count = 0;
    if (ioctlsocket(as_socket(fd), FIONREAD, &count) == SOCKET_ERROR) return socket_error(WSAGetLastError());
    return count > INT32_MAX ? INT32_MAX : (int32_t)count;
#else
    int count = 0;
    if (ioctl(fd, FIONREAD, &count) < 0) return socket_error(errno);
    return count;
#endif
}

/* IOUtil.configureBlocking. */
void jabs_net_configure_blocking(jabs_object *fdo, int32_t blocking) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return;
#ifdef _WIN32
    net_ready();
    u_long nonblocking = blocking ? 0 : 1;
    if (ioctlsocket(as_socket(fd), FIONBIO, &nonblocking) == SOCKET_ERROR) net_throw_new(WSAGetLastError(), "ioctlsocket");
#else
    int flags = fcntl(fd, F_GETFL);
    int wanted = blocking ? (flags & ~O_NONBLOCK) : (flags | O_NONBLOCK);
    if (flags < 0 || (flags != wanted && fcntl(fd, F_SETFL, wanted) < 0)) raise_errno(-1, errno, "Configure blocking failed");
#endif
}

/* SocketDispatcher.read0: bytes read, IOS_EOF at end of stream, IOS_UNAVAILABLE when a
 * non-blocking socket has none. A reset is sun.net.ConnectionResetException("Connection reset"). */
int32_t jabs_net_read(jabs_object *fdo, int64_t address, int32_t length) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return JABS_IOS_THROWN;
    if (length < 0) jabs_not_compiled("SocketDispatcher.read0 with a negative length (jabs_net_read, jabs_net.c)");
    void *buffer = (void *)(intptr_t)address;
#ifdef _WIN32
    net_ready();
    WSABUF b;
    b.len = (ULONG)length;
    b.buf = (char *)buffer;
    DWORD got = 0, flags = 0;
    if (WSARecv(as_socket(fd), &b, 1, &got, &flags, NULL, NULL) == SOCKET_ERROR) {
        int e = WSAGetLastError();
        if (e == WSAEWOULDBLOCK) return JABS_IOS_UNAVAILABLE;
        if (e == WSAECONNRESET) jabs_net_raise(JABS_NET_CONNECTION_RESET, "Connection reset");
        else raise_io_last_error((DWORD)e, "WSARecv failed");
        return JABS_IOS_THROWN;
    }
    /* convertReturnVal: zero bytes read is the end of the stream (even for a zero length). */
    if (got > INT32_MAX) jabs_not_compiled("WSARecv returned more than asked (jabs_net_read, jabs_net.c)");
    return got > 0 ? (int32_t)got : JABS_IOS_EOF;
#else
    ssize_t n = read(fd, buffer, (size_t)length);
    if (n < 0 && (errno == ECONNRESET || errno == EPIPE)) {
        jabs_net_raise(JABS_NET_CONNECTION_RESET, "Connection reset");
        return JABS_IOS_THROWN;
    }
    if (n > 0) return (int32_t)n;
    if (n == 0) return JABS_IOS_EOF;
    if (errno == EAGAIN || errno == EWOULDBLOCK) return JABS_IOS_UNAVAILABLE;
    if (errno == EINTR) return JABS_IOS_INTERRUPTED;
    raise_errno(-1, errno, "Read failed");
    return JABS_IOS_THROWN;
#endif
}

/* SocketDispatcher.write0: bytes written, IOS_UNAVAILABLE when a non-blocking socket is full.
 * A reset is IOException("Connection reset by peer"). */
int32_t jabs_net_write(jabs_object *fdo, int64_t address, int32_t length) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return JABS_IOS_THROWN;
    if (length < 0) jabs_not_compiled("SocketDispatcher.write0 with a negative length (jabs_net_write, jabs_net.c)");
    void *buffer = (void *)(intptr_t)address;
#ifdef _WIN32
    net_ready();
    WSABUF b;
    b.len = (ULONG)length;
    b.buf = (char *)buffer;
    DWORD sent = 0;
    if (WSASend(as_socket(fd), &b, 1, &sent, 0, NULL, NULL) == SOCKET_ERROR) {
        int e = WSAGetLastError();
        if (e == WSAEWOULDBLOCK) return JABS_IOS_UNAVAILABLE;
        if (e == WSAECONNRESET) net_raise_io("Connection reset by peer");
        else raise_io_last_error((DWORD)e, "WSASend failed");
        return JABS_IOS_THROWN;
    }
    if (sent > INT32_MAX) jabs_not_compiled("WSASend sent more than asked (jabs_net_write, jabs_net.c)");
    return (int32_t)sent;
#else
    ssize_t n = write(fd, buffer, (size_t)length);
    if (n < 0 && (errno == ECONNRESET || errno == EPIPE)) {
        net_raise_io("Connection reset by peer");
        return JABS_IOS_THROWN;
    }
    if (n >= 0) return (int32_t)n;
    if (errno == EAGAIN || errno == EWOULDBLOCK) return JABS_IOS_UNAVAILABLE;
    if (errno == EINTR) return JABS_IOS_INTERRUPTED;
    raise_errno(-1, errno, "Write failed");
    return JABS_IOS_THROWN;
#endif
}

#ifdef _WIN32
/* SocketDispatcher.close0 (Windows): closesocket. */
void jabs_net_close_socket(int32_t fd) {
    net_ready();
    if (closesocket(as_socket(fd)) == SOCKET_ERROR) raise_io_last_error((DWORD)WSAGetLastError(), "Socket close failed");
}

/* IOUtil.drain (Windows, a socket): read until none is left; whether anything was. */
int32_t jabs_net_drain(int32_t fd) {
    net_ready();
    char buffer[16];
    int32_t any = 0;
    for (;;) {
        int n = recv(as_socket(fd), buffer, (int)sizeof buffer, 0);
        if (n == SOCKET_ERROR) {
            int e = WSAGetLastError();
            if (e == WSAEWOULDBLOCK) return any;
            raise_io_last_error((DWORD)e, "recv failed");
            return 0;
        }
        if (n == 0) return any;
        any = 1;
        if ((size_t)n < sizeof buffer) return any;
    }
}

/* IOUtil.write1 (Windows, a socket): one byte; 1, or IOS_UNAVAILABLE when the socket is full. */
int32_t jabs_net_write1(int32_t fd, int8_t byte) {
    net_ready();
    char b = (char)byte;
    int n = send(as_socket(fd), &b, 1, 0);
    if (n == SOCKET_ERROR) {
        int e = WSAGetLastError();
        if (e == WSAEWOULDBLOCK) return JABS_IOS_UNAVAILABLE;
        raise_io_last_error((DWORD)e, "send failed");
        return JABS_IOS_THROWN;
    }
    return n;
}
#else
/* ---- Unix descriptors: UnixDispatcher, NativeThread, IOUtil's pipe helpers ---- */

/* The descriptor every preClose dup2s over a socket: a half-closed socket pair, so a thread
 * blocked on the socket wakes with end of stream (UnixDispatcher.init). */
static _Atomic int pre_close_fd = -1;

void jabs_net_dispatcher_init(void) {
    int pair[2];
    if (socketpair(PF_UNIX, SOCK_STREAM, 0, pair) < 0) { raise_errno(-1, errno, "socketpair failed"); return; }
    int expected = -1;
    if (!atomic_compare_exchange_strong(&pre_close_fd, &expected, pair[0])) close(pair[0]);
    close(pair[1]);
}

void jabs_net_pre_close(jabs_object *fdo) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return;
    int pre = atomic_load(&pre_close_fd);
    if (pre >= 0 && dup2(pre, fd) < 0) raise_errno(-1, errno, "dup2 failed");
}

void jabs_net_close_fd(int32_t fd) {
    if (fd != -1 && close(fd) < 0 && errno != EINTR) raise_errno(-1, errno, "Close failed");
}

/* NativeThread: the signal a blocked I/O call is woken with (SIGRTMAX - 2, as the JDK's). */
static void null_handler(int signal) { (void)signal; }

void jabs_net_native_thread_init(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = null_handler;
    sa.sa_flags = 0;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGRTMAX - 2, &sa, NULL) < 0) raise_errno(-1, errno, "sigaction");
}

int64_t jabs_net_native_thread_current(void) {
    pthread_t self = pthread_self();
    int64_t id = 0;
    if (sizeof self > sizeof id) jabs_not_compiled("a pthread_t wider than a long (jabs_net_native_thread_current, jabs_net.c)");
    memcpy(&id, &self, sizeof self);
    return id;
}

void jabs_net_native_thread_signal(int64_t thread) {
    pthread_t target;
    memcpy(&target, &thread, sizeof target);
    int r = pthread_kill(target, SIGRTMAX - 2);
    if (r != 0) raise_errno(-1, r, "Thread signal failed");
}

/* IOUtil.makePipe: read end in the high 32 bits, write end in the low, both non-blocking
 * unless asked to block. */
int64_t jabs_net_make_pipe(int32_t blocking) {
    int fds[2];
    if (pipe(fds) < 0) { raise_errno(-1, errno, "Pipe failed"); return 0; }
    if (!blocking) {
        for (int i = 0; i < 2; i++) {
            int flags = fcntl(fds[i], F_GETFL);
            if (flags < 0 || fcntl(fds[i], F_SETFL, flags | O_NONBLOCK) < 0) {
                int e = errno;
                close(fds[0]);
                close(fds[1]);
                raise_errno(-1, e, "Configure blocking failed");
                return 0;
            }
        }
    }
    return ((int64_t)fds[0] << 32) | (int64_t)(uint32_t)fds[1];
}

int32_t jabs_net_drain(int32_t fd) {
    char buffer[16];
    int32_t any = 0;
    for (;;) {
        ssize_t n = read(fd, buffer, sizeof buffer);
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) return any;
            if (errno == EINTR) continue;
            raise_errno(-1, errno, "Drain");
            return 0;
        }
        if (n == 0) return any;
        any = 1;
        if ((size_t)n < sizeof buffer) return any;
    }
}

int32_t jabs_net_write1(int32_t fd, int8_t byte) {
    char b = (char)byte;
    ssize_t n = write(fd, &b, 1);
    if (n == 1) return 1;
    if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return JABS_IOS_UNAVAILABLE;
    if (n < 0 && errno == EINTR) return JABS_IOS_INTERRUPTED;
    raise_errno(-1, errno, "Write failed");
    return JABS_IOS_THROWN;
}

/* IOUtil.iovMax and fdLimit: sysconf(_SC_IOV_MAX) (16 when unknown) and the descriptor limit. */
int32_t jabs_net_iov_max(void) {
    long n = sysconf(_SC_IOV_MAX);
    if (n < 0) return 16;
    return n > INT32_MAX ? INT32_MAX : (int32_t)n;
}

int32_t jabs_net_fd_limit(void) {
    struct rlimit limit;
    if (getrlimit(RLIMIT_NOFILE, &limit) < 0) { raise_errno(-1, errno, "getrlimit failed"); return -1; }
    if (limit.rlim_cur == RLIM_INFINITY || limit.rlim_cur > (rlim_t)INT32_MAX) return INT32_MAX;
    return (int32_t)limit.rlim_cur;
}

/* IOUtil.initIDs (Linux): a write to a closed socket must fail with EPIPE, not kill the
 * process; HotSpot ignores SIGPIPE, so JABS does too (unless the program set a handler). */
void jabs_net_ignore_sigpipe(void) {
    struct sigaction old;
    if (sigaction(SIGPIPE, NULL, &old) < 0) jabs_not_compiled("sigaction(SIGPIPE) failed (jabs_net_ignore_sigpipe, jabs_net.c)");
    if (old.sa_handler != SIG_DFL) return;
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = SIG_IGN;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGPIPE, &sa, NULL) < 0) jabs_not_compiled("sigaction(SIGPIPE, SIG_IGN) failed (jabs_net_ignore_sigpipe, jabs_net.c)");
}
#endif

/* ---- Small natives: answers the JDK's natives give on this OS ---- */

int32_t jabs_net_fd_val(jabs_object *fdo) { return fd_of(fdo); }

void jabs_net_set_fd_val(jabs_object *fdo, int32_t fd) {
    if (!fdo) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }
    *jabs_descriptor_fd_slot(fdo) = fd;
}

/* Net.isReusePortAvailable0: whether SO_REUSEPORT can be set (never on Windows). */
static int32_t reuse_port_available(void) {
#if defined(_WIN32) || !defined(SO_REUSEPORT)
    return 0;
#else
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) return 0;
    int on = 1;
    int ok = setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &on, sizeof on) == 0;
    close(fd);
    return ok;
#endif
}

/* The values behind JABS_NET_VALUE_* (src/cgen/net.rs), as the reference JVMs report them
 * (JDK 25 on Windows and Linux; tests/networking.rs compares them on every run). */
int64_t jabs_net_value(int32_t which) {
    switch (which) {
    case JABS_NET_VALUE_POLLIN: return POLLIN;
    case JABS_NET_VALUE_POLLOUT: return POLLOUT;
    case JABS_NET_VALUE_POLLERR: return POLLERR;
    case JABS_NET_VALUE_POLLHUP: return POLLHUP;
    case JABS_NET_VALUE_POLLNVAL: return POLLNVAL;
    /* A finished connect is reported as writable on both. */
    case JABS_NET_VALUE_POLLCONN: return POLLOUT;
    case JABS_NET_VALUE_REUSE_PORT: return reuse_port_available();
#ifdef _WIN32
    case JABS_NET_VALUE_EXCLUSIVE_BIND: return 1;
    case JABS_NET_VALUE_SHUTDOWN_WRITE_BEFORE_CLOSE: return 1;
    case JABS_NET_VALUE_IPV6_OPTIONS_WITH_IPV4_LOCAL: return 0;
    case JABS_NET_VALUE_IOV_MAX: return 16;
    case JABS_NET_VALUE_WRITEV_MAX: return INT64_MAX;
#else
    case JABS_NET_VALUE_EXCLUSIVE_BIND: return -1;
    case JABS_NET_VALUE_SHUTDOWN_WRITE_BEFORE_CLOSE: return 0;
    case JABS_NET_VALUE_IPV6_OPTIONS_WITH_IPV4_LOCAL: return 1;
    case JABS_NET_VALUE_IOV_MAX: return jabs_net_iov_max();
    case JABS_NET_VALUE_WRITEV_MAX: return INT32_MAX;
#endif
    case JABS_NET_VALUE_SET_BOTH_IPV4_IPV6_OPTIONS: return 1;
    case JABS_NET_VALUE_IPV6_JOINS_IPV4_GROUP: return 1;
    case JABS_NET_VALUE_JOIN6_WITH_IPV4_GROUP: return 0;
    default: jabs_not_compiled("jabs_net_value with an unknown value (src/cgen/net.rs, jabs_net.c)");
    }
}

/* Natives of the other OS's JDK: never reached there, but every program declares them. */
#ifdef _WIN32
void jabs_net_dispatcher_init(void) { jabs_not_compiled("UnixDispatcher.init on Windows (jabs_net.c)"); }
void jabs_net_pre_close(jabs_object *fdo) { (void)fdo; jabs_not_compiled("UnixDispatcher.preClose0 on Windows (jabs_net.c)"); }
void jabs_net_close_fd(int32_t fd) { (void)fd; jabs_not_compiled("UnixDispatcher.close0 on Windows (jabs_net.c)"); }
void jabs_net_native_thread_init(void) { jabs_not_compiled("NativeThread.init on Windows (jabs_net.c)"); }
int64_t jabs_net_native_thread_current(void) { jabs_not_compiled("NativeThread.current0 on Windows (jabs_net.c)"); }
void jabs_net_native_thread_signal(int64_t thread) { (void)thread; jabs_not_compiled("NativeThread.signal0 on Windows (jabs_net.c)"); }
int64_t jabs_net_make_pipe(int32_t blocking) { (void)blocking; jabs_not_compiled("IOUtil.makePipe on Windows (the JDK's Windows IOUtil has no pipe; jabs_net.c)"); }
int32_t jabs_net_fd_limit(void) { jabs_not_compiled("IOUtil.fdLimit on Windows (jabs_net.c)"); }
void jabs_net_ioutil_init(void) { /* nothing to prepare: Windows sends no SIGPIPE */ }
#else
void jabs_net_close_socket(int32_t fd) { (void)fd; jabs_not_compiled("SocketDispatcher.close0(int) on Linux (jabs_net.c)"); }
void jabs_net_ioutil_init(void) { jabs_net_ignore_sigpipe(); }
#endif

/* ---- Selectors: WEPoll (Windows), EPoll and EventFD (Linux) ---- */

#ifdef _WIN32
/* The JDK's Windows selector runs on wepoll, an epoll emulation it builds in; JABS emulates
 * the same epoll contract over WSAPoll. An epoll set is a table of (socket, events, data)
 * under a lock; a wait polls a snapshot of it (level-triggered, as wepoll is without
 * EPOLLONESHOT) and reports up to maxevents ready sockets as wepoll's epoll_event, whose layout
 * Java reads at eventSize/eventsOffset/dataOffset. Known difference: WSAPoll before Windows 10
 * 2004 did not report a refused non-blocking connect; wepoll (AFD) does. */
#define JABS_EPOLLIN 0x001
#define JABS_EPOLLPRI 0x002
#define JABS_EPOLLOUT 0x004
#define JABS_EPOLLERR 0x008
#define JABS_EPOLLHUP 0x010
#define JABS_EPOLLONESHOT 0x80000000u
#define JABS_EPOLL_CTL_ADD 1
#define JABS_EPOLL_CTL_MOD 2
#define JABS_EPOLL_CTL_DEL 3
/* The errno values wepoll's epoll_ctl returns (Windows CRT numbering). */
#define JABS_ENOENT 2
#define JABS_EEXIST 17
#define JABS_EINVAL 22
#define JABS_ENOMEM 12
/* Most sockets one set holds: far above what a selector registers, a table that big is a bug. */
#define JABS_EPOLL_CAP 65536
/* Marks a live set, so a stale or foreign handle stops instead of corrupting memory. */
#define JABS_EPOLL_MAGIC 0x4a414253504f4c4cULL

/* wepoll's struct epoll_event: events, then a union of 8 bytes (16 bytes on x64). */
typedef struct {
    uint32_t events;
    uint64_t data;
} jabs_epoll_event;

typedef struct {
    SOCKET socket;
    uint32_t events;
    uint64_t data;
} epoll_entry;

typedef struct {
    uint64_t magic;
    SRWLOCK lock;
    epoll_entry *entries;
    size_t count, capacity;
} epoll_set;

static epoll_set *epoll_of(int64_t handle) {
    epoll_set *set = (epoll_set *)(intptr_t)handle;
    if (!set || set->magic != JABS_EPOLL_MAGIC) jabs_not_compiled("a WEPoll handle that is not a live epoll set (jabs_net.c)");
    return set;
}

int32_t jabs_net_epoll_event_size(void) { return (int32_t)sizeof(jabs_epoll_event); }
int32_t jabs_net_epoll_events_offset(void) { return (int32_t)offsetof(jabs_epoll_event, events); }
int32_t jabs_net_epoll_data_offset(void) { return (int32_t)offsetof(jabs_epoll_event, data); }

int64_t jabs_net_epoll_create(void) {
    net_ready();
    epoll_set *set = (epoll_set *)calloc(1, sizeof *set);
    if (!set) { net_raise_io("epoll_create1 failed"); return 0; }
    set->magic = JABS_EPOLL_MAGIC;
    InitializeSRWLock(&set->lock);
    return (int64_t)(intptr_t)set;
}

/* epoll_ctl: 0, or the errno wepoll would give (EEXIST, ENOENT, EINVAL, ENOMEM). */
int32_t jabs_net_epoll_ctl(int64_t handle, int32_t op, int64_t socket_value, int32_t events) {
    epoll_set *set = epoll_of(handle);
    SOCKET s = (SOCKET)(intptr_t)socket_value;
    int32_t result = 0;
    AcquireSRWLockExclusive(&set->lock);
    size_t at = 0;
    while (at < set->count && set->entries[at].socket != s) at++;
    int found = at < set->count;
    if (op == JABS_EPOLL_CTL_ADD) {
        if (found) result = JABS_EEXIST;
        else if (set->count >= JABS_EPOLL_CAP) result = JABS_ENOMEM;
        else {
            if (set->count == set->capacity) {
                size_t grown = set->capacity ? set->capacity * 2 : 16;
                epoll_entry *bigger = (epoll_entry *)realloc(set->entries, grown * sizeof *bigger);
                if (!bigger) result = JABS_ENOMEM;
                else { set->entries = bigger; set->capacity = grown; }
            }
            if (result == 0) {
                set->entries[set->count].socket = s;
                set->entries[set->count].events = (uint32_t)events;
                /* wepoll's JDK caller keeps the socket itself in the data. */
                set->entries[set->count].data = (uint64_t)socket_value;
                set->count++;
            }
        }
    } else if (op == JABS_EPOLL_CTL_MOD) {
        if (!found) result = JABS_ENOENT;
        else set->entries[at].events = (uint32_t)events;
    } else if (op == JABS_EPOLL_CTL_DEL) {
        if (!found) result = JABS_ENOENT;
        else set->entries[at] = set->entries[--set->count];
    } else {
        result = JABS_EINVAL;
    }
    ReleaseSRWLockExclusive(&set->lock);
    return result;
}

/* WSAPoll's report for one socket as epoll events, limited to what was asked plus the errors
 * and hangups epoll always reports. A closed connection is readable (its end of stream). */
static uint32_t epoll_events_of(short revents, uint32_t wanted) {
    uint32_t got = 0;
    if (revents & (POLLRDNORM | POLLRDBAND)) got |= JABS_EPOLLIN;
    if (revents & POLLWRNORM) got |= JABS_EPOLLOUT;
    if (revents & POLLERR) got |= JABS_EPOLLERR;
    if (revents & POLLHUP) got |= JABS_EPOLLHUP | JABS_EPOLLIN;
    if (revents & POLLNVAL) got |= JABS_EPOLLERR | JABS_EPOLLHUP;
    return got & (wanted | JABS_EPOLLERR | JABS_EPOLLHUP);
}

/* epoll_wait: ready sockets into the epoll_event array at address, at most maxevents; 0 on
 * timeout; timeout -1 waits indefinitely. */
int32_t jabs_net_epoll_wait(int64_t handle, int64_t address, int32_t maxevents, int32_t timeout) {
    epoll_set *set = epoll_of(handle);
    if (maxevents <= 0 || !address) jabs_not_compiled("WEPoll.wait without room for events (jabs_net_epoll_wait, jabs_net.c)");
    jabs_epoll_event *out = (jabs_epoll_event *)(intptr_t)address;
    AcquireSRWLockShared(&set->lock);
    size_t n = set->count;
    WSAPOLLFD *fds = n ? (WSAPOLLFD *)calloc(n, sizeof *fds) : NULL;
    uint32_t *wanted = n ? (uint32_t *)calloc(n, sizeof *wanted) : NULL;
    uint64_t *data = n ? (uint64_t *)calloc(n, sizeof *data) : NULL;
    if (n && (!fds || !wanted || !data)) {
        ReleaseSRWLockShared(&set->lock);
        free(fds); free(wanted); free(data);
        net_raise_io("epoll_wait failed");
        return JABS_IOS_THROWN;
    }
    size_t polled = 0;
    for (size_t i = 0; i < n; i++) {
        epoll_entry *e = &set->entries[i];
        /* Disarmed (EPOLLONESHOT fired, or no events): epoll reports nothing for it. */
        if ((e->events & ~JABS_EPOLLONESHOT) == 0) continue;
        fds[polled].fd = e->socket;
        fds[polled].events = (short)(((e->events & JABS_EPOLLIN) ? POLLRDNORM : 0) | ((e->events & JABS_EPOLLPRI) ? POLLRDBAND : 0) | ((e->events & JABS_EPOLLOUT) ? POLLWRNORM : 0));
        wanted[polled] = e->events;
        data[polled] = e->data;
        polled++;
    }
    ReleaseSRWLockShared(&set->lock);
    if (polled == 0) {
        /* Nothing to wait on: wepoll would sleep out the timeout. */
        free(fds); free(wanted); free(data);
        if (timeout != 0) Sleep(timeout < 0 ? INFINITE : (DWORD)timeout);
        return 0;
    }
    if (polled > ULONG_MAX) jabs_not_compiled("more sockets than WSAPoll takes (jabs_net_epoll_wait, jabs_net.c)");
    int r = WSAPoll(fds, (ULONG)polled, timeout < 0 ? -1 : timeout);
    if (r == SOCKET_ERROR) {
        int e = WSAGetLastError();
        free(fds); free(wanted); free(data);
        if (e == WSAEINTR) return JABS_IOS_INTERRUPTED;
        raise_io_last_error((DWORD)e, "epoll_wait failed");
        return JABS_IOS_THROWN;
    }
    int32_t reported = 0;
    for (size_t i = 0; i < polled && reported < maxevents; i++) {
        uint32_t got = epoll_events_of(fds[i].revents, wanted[i]);
        if (!got) continue;
        out[reported].events = got;
        out[reported].data = data[i];
        reported++;
        if (wanted[i] & JABS_EPOLLONESHOT) {
            AcquireSRWLockExclusive(&set->lock);
            for (size_t k = 0; k < set->count; k++) {
                if (set->entries[k].socket == fds[i].fd) { set->entries[k].events = JABS_EPOLLONESHOT; break; }
            }
            ReleaseSRWLockExclusive(&set->lock);
        }
    }
    free(fds); free(wanted); free(data);
    return reported;
}

void jabs_net_epoll_close(int64_t handle) {
    epoll_set *set = epoll_of(handle);
    set->magic = 0;
    free(set->entries);
    free(set);
}

/* Linux's natives: never reached on Windows. */
int32_t jabs_net_linux_epoll_create(void) { jabs_not_compiled("EPoll.create on Windows (jabs_net.c)"); }
int32_t jabs_net_linux_epoll_ctl(int32_t epfd, int32_t op, int32_t fd, int32_t events) { (void)epfd; (void)op; (void)fd; (void)events; jabs_not_compiled("EPoll.ctl on Windows (jabs_net.c)"); }
int32_t jabs_net_linux_epoll_wait(int32_t epfd, int64_t address, int32_t maxevents, int32_t timeout) { (void)epfd; (void)address; (void)maxevents; (void)timeout; jabs_not_compiled("EPoll.wait on Windows (jabs_net.c)"); }
int32_t jabs_net_eventfd(void) { jabs_not_compiled("EventFD.eventfd0 on Windows (jabs_net.c)"); }
int32_t jabs_net_eventfd_set(int32_t efd) { (void)efd; jabs_not_compiled("EventFD.set0 on Windows (jabs_net.c)"); }
#else
#include <sys/epoll.h>
#include <sys/eventfd.h>

int32_t jabs_net_epoll_event_size(void) { return (int32_t)sizeof(struct epoll_event); }
int32_t jabs_net_epoll_events_offset(void) { return (int32_t)offsetof(struct epoll_event, events); }
int32_t jabs_net_epoll_data_offset(void) { return (int32_t)offsetof(struct epoll_event, data); }

/* EPoll.create: epoll_create1(EPOLL_CLOEXEC), as the JDK. */
int32_t jabs_net_linux_epoll_create(void) {
    int epfd = epoll_create1(EPOLL_CLOEXEC);
    if (epfd < 0) raise_errno(-1, errno, "epoll_create1 failed");
    return epfd;
}

/* EPoll.ctl: 0, or the errno. */
int32_t jabs_net_linux_epoll_ctl(int32_t epfd, int32_t op, int32_t fd, int32_t events) {
    struct epoll_event event;
    memset(&event, 0, sizeof event);
    event.events = (uint32_t)events;
    event.data.fd = fd;
    return epoll_ctl(epfd, op, fd, &event) == 0 ? 0 : errno;
}

/* EPoll.wait: ready descriptors, IOS_INTERRUPTED on EINTR. */
int32_t jabs_net_linux_epoll_wait(int32_t epfd, int64_t address, int32_t maxevents, int32_t timeout) {
    int n = epoll_wait(epfd, (struct epoll_event *)(intptr_t)address, maxevents, timeout);
    if (n >= 0) return n;
    if (errno == EINTR) return JABS_IOS_INTERRUPTED;
    raise_errno(-1, errno, "epoll_wait failed");
    return JABS_IOS_THROWN;
}

/* EventFD.eventfd0: a blocking eventfd (the selector makes it non-blocking itself). */
int32_t jabs_net_eventfd(void) {
    int efd = eventfd(0, 0);
    if (efd < 0) raise_errno(-1, errno, "eventfd failed");
    return efd;
}

/* EventFD.set0: add 1 to the counter, waking an epoll_wait on it. */
int32_t jabs_net_eventfd_set(int32_t efd) {
    uint64_t one = 1;
    ssize_t n = write(efd, &one, sizeof one);
    if (n == (ssize_t)sizeof one) return 0;
    if (n < 0 && errno == EAGAIN) return 0;
    raise_errno(-1, errno, "eventfd write failed");
    return JABS_IOS_THROWN;
}

int64_t jabs_net_epoll_create(void) { jabs_not_compiled("WEPoll.create on Linux (jabs_net.c)"); }
int32_t jabs_net_epoll_ctl(int64_t handle, int32_t op, int64_t socket_value, int32_t events) { (void)handle; (void)op; (void)socket_value; (void)events; jabs_not_compiled("WEPoll.ctl on Linux (jabs_net.c)"); }
int32_t jabs_net_epoll_wait(int64_t handle, int64_t address, int32_t maxevents, int32_t timeout) { (void)handle; (void)address; (void)maxevents; (void)timeout; jabs_not_compiled("WEPoll.wait on Linux (jabs_net.c)"); }
void jabs_net_epoll_close(int64_t handle) { (void)handle; jabs_not_compiled("WEPoll.close on Linux (jabs_net.c)"); }
#endif

/* ---- Unix domain sockets (sun.nio.ch.UnixDomainSockets): the Windows selector's wakeup pipe ---- */

#ifdef _WIN32
#include <afunix.h>
#else
#include <sys/un.h>
#endif

/* A path's bytes as a sockaddr_un; the JDK's unixSocketAddressToSockaddr. An empty path binds
 * to an address the OS picks. Returns 0, or -1 with an exception raised. */
static int uds_address(jabs_object *path, struct sockaddr_un *sa, int *length) {
    if (!path) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return -1; }
    jabs_array *bytes = (jabs_array *)path;
    memset(sa, 0, sizeof *sa);
    sa->sun_family = AF_UNIX;
    if ((size_t)bytes->length >= sizeof sa->sun_path) {
        jabs_net_raise(JABS_NET_SOCKET, "Unix domain path too long");
        return -1;
    }
    memcpy(sa->sun_path, jabs_array_data(bytes), (size_t)bytes->length);
    *length = (int)(offsetof(struct sockaddr_un, sun_path) + (size_t)bytes->length + 1);
    return 0;
}

/* sockaddrToUnixAddressBytes: the path as a byte[] (empty for an unnamed socket). */
static jabs_object *uds_path_bytes(const struct sockaddr_un *sa, int length) {
    size_t n = 0;
    if (sa->sun_family == AF_UNIX && length > (int)offsetof(struct sockaddr_un, sun_path)) {
        size_t room = (size_t)length - offsetof(struct sockaddr_un, sun_path);
        if (room > sizeof sa->sun_path) room = sizeof sa->sun_path;
        while (n < room && sa->sun_path[n] != 0) n++;
    }
    jabs_object *bytes = jabs_new_array((int32_t)n, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
    if (bytes && n) memcpy(jabs_array_data((jabs_array *)bytes), sa->sun_path, n);
    return bytes;
}

#ifdef _WIN32
/* UnixDomainSockets.init: whether this Windows has AF_UNIX (Windows 10 1803 and later). */
int32_t jabs_net_uds_init(void) {
    net_ready();
    SOCKET s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s == INVALID_SOCKET) return 0;
    closesocket(s);
    return 1;
}

int32_t jabs_net_uds_socket(void) {
    net_ready();
    SOCKET s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s == INVALID_SOCKET) return socket_error(WSAGetLastError());
    int32_t value = socket_value(s, "a socket handle above int (jabs_net_uds_socket, jabs_net.c)");
    if (!SetHandleInformation((HANDLE)s, HANDLE_FLAG_INHERIT, 0)) jabs_not_compiled("SetHandleInformation failed on a new socket (jabs_net_uds_socket, jabs_net.c)");
    return value;
}

void jabs_net_uds_bind(jabs_object *fdo, jabs_object *path) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return;
    struct sockaddr_un sa;
    int length = 0;
    if (uds_address(path, &sa, &length) != 0) return;
    if (bind(as_socket(fd), (struct sockaddr *)&sa, length) == SOCKET_ERROR) socket_error(WSAGetLastError());
}

int32_t jabs_net_uds_connect(jabs_object *fdo, jabs_object *path) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return JABS_IOS_THROWN;
    struct sockaddr_un sa;
    int length = 0;
    if (uds_address(path, &sa, &length) != 0) return JABS_IOS_THROWN;
    if (connect(as_socket(fd), (struct sockaddr *)&sa, length) == SOCKET_ERROR) {
        int e = WSAGetLastError();
        if (e == WSAEINPROGRESS || e == WSAEWOULDBLOCK) return JABS_IOS_UNAVAILABLE;
        return socket_error(e);
    }
    return 1;
}

int32_t jabs_net_uds_accept(jabs_object *fdo, jabs_object *newfdo, jabs_object *addresses) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return JABS_IOS_THROWN;
    if (!newfdo || !addresses) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return JABS_IOS_THROWN; }
    if (((jabs_array *)addresses)->length < 1) { jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_INDEX); return JABS_IOS_THROWN; }
    struct sockaddr_un sa;
    memset(&sa, 0, sizeof sa);
    int length = (int)sizeof sa;
    SOCKET s = accept(as_socket(fd), (struct sockaddr *)&sa, &length);
    if (s == INVALID_SOCKET) {
        int e = WSAGetLastError();
        if (e == WSAEWOULDBLOCK) return JABS_IOS_UNAVAILABLE;
        return socket_error(e);
    }
    int32_t newfd = socket_value(s, "an accepted socket handle above int (jabs_net_uds_accept, jabs_net.c)");
    if (!SetHandleInformation((HANDLE)s, HANDLE_FLAG_INHERIT, 0)) jabs_not_compiled("SetHandleInformation failed on an accepted socket (jabs_net_uds_accept, jabs_net.c)");
    *jabs_descriptor_fd_slot(newfdo) = newfd;
    jabs_object *peer = uds_path_bytes(&sa, length);
    if (!peer) return JABS_IOS_THROWN;
    ((jabs_object **)jabs_array_data((jabs_array *)addresses))[0] = peer;
    return 1;
}

jabs_object *jabs_net_uds_local_address(jabs_object *fdo) {
    int32_t fd = fd_of(fdo);
    if (jabs_pending) return NULL;
    struct sockaddr_un sa;
    memset(&sa, 0, sizeof sa);
    int length = (int)sizeof sa;
    if (getsockname(as_socket(fd), (struct sockaddr *)&sa, &length) == SOCKET_ERROR) { socket_error(WSAGetLastError()); return NULL; }
    return uds_path_bytes(&sa, length);
}
#else
/* Linux: AF_UNIX always exists; only the Windows selector uses these natives, so they stop. */
int32_t jabs_net_uds_init(void) { jabs_not_compiled("UnixDomainSockets.init on Linux (jabs_net.c)"); }
int32_t jabs_net_uds_socket(void) { jabs_not_compiled("UnixDomainSockets.socket0 on Linux (jabs_net.c)"); }
void jabs_net_uds_bind(jabs_object *fdo, jabs_object *path) { (void)fdo; (void)path; (void)uds_address; jabs_not_compiled("UnixDomainSockets.bind0 on Linux (jabs_net.c)"); }
int32_t jabs_net_uds_connect(jabs_object *fdo, jabs_object *path) { (void)fdo; (void)path; jabs_not_compiled("UnixDomainSockets.connect0 on Linux (jabs_net.c)"); }
int32_t jabs_net_uds_accept(jabs_object *fdo, jabs_object *newfdo, jabs_object *addresses) { (void)fdo; (void)newfdo; (void)addresses; (void)uds_path_bytes; jabs_not_compiled("UnixDomainSockets.accept0 on Linux (jabs_net.c)"); }
jabs_object *jabs_net_uds_local_address(jabs_object *fdo) { (void)fdo; jabs_not_compiled("UnixDomainSockets.localAddress0 on Linux (jabs_net.c)"); }
#endif

/* ---- sun.security.provider.NativeSeedGenerator (Windows): the OS's random bytes ---- */

#ifdef _WIN32
/* The JDK asks CryptGenRandom; rand_s is the C runtime's door to the same system generator
 * (RtlGenRandom), without linking advapi32. Whether every byte was filled. */
int32_t jabs_net_generate_seed(jabs_object *bytes) {
    if (!bytes) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }
    jabs_array *a = (jabs_array *)bytes;
    uint8_t *out = (uint8_t *)jabs_array_data(a);
    for (int32_t i = 0; i < a->length; i += 4) {
        unsigned int r = 0;
        if (rand_s(&r) != 0) return 0;
        for (int32_t k = 0; k < 4 && i + k < a->length; k++) out[i + k] = (uint8_t)(r >> (8 * k));
    }
    return 1;
}
#else
int32_t jabs_net_generate_seed(jabs_object *bytes) { (void)bytes; jabs_not_compiled("NativeSeedGenerator.nativeGenerateSeed on Linux (the JDK reads /dev/random in Java there; jabs_net.c)"); }
#endif

/* ---- java.net.NetworkInterface ---- */

/* One address of an interface, as NetworkInterface reports it: family (AF_INET or AF_INET6),
 * bytes in network order, IPv6 scope id, and the on-link prefix length. */
typedef struct {
    int family;
    uint8_t bytes[16];
    uint32_t scope;
    int32_t prefix;
} ni_address;

/* Most addresses JABS keeps for one interface; more is a broken table. */
#define JABS_NI_ADDRESS_CAP 1024

#ifdef _WIN32
#include <iphlpapi.h>
#include <netioapi.h>

/* GetAdaptersAddresses for every family, without DNS servers, multicast and anycast addresses
 * (what NetworkInterface reports); malloc'd, NULL with an exception raised. */
#define JABS_NI_ADAPTER_TRIES 8
static IP_ADAPTER_ADDRESSES *ni_adapters(void) {
    ULONG flags = GAA_FLAG_SKIP_DNS_SERVER | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_ANYCAST;
    ULONG size = 16384;
    for (int attempt = 0; attempt < JABS_NI_ADAPTER_TRIES; attempt++) {
        IP_ADAPTER_ADDRESSES *list = (IP_ADAPTER_ADDRESSES *)malloc(size);
        if (!list) { jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }
        ULONG r = GetAdaptersAddresses(AF_UNSPEC, flags, NULL, list, &size);
        if (r == ERROR_SUCCESS) return list;
        free(list);
        if (r == ERROR_NO_DATA) {
            /* No adapters at all: an empty list. */
            list = (IP_ADAPTER_ADDRESSES *)calloc(1, sizeof *list);
            if (!list) { jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }
            list->Length = 0;
            return list;
        }
        if (r != ERROR_BUFFER_OVERFLOW) { jabs_net_raise(JABS_NET_SOCKET, "GetAdaptersAddresses failed"); return NULL; }
    }
    jabs_net_raise(JABS_NET_SOCKET, "GetAdaptersAddresses kept growing");
    return NULL;
}

/* Whether an address is usable: its duplicate-address detection finished (preferred, or
 * deprecated but still valid), as the JDK's getAddrsFromAdapter requires. A disconnected
 * adapter's tentative 169.254.x.x address is not reported. */
static int ni_usable(const IP_ADAPTER_UNICAST_ADDRESS *u) {
    return u->DadState == IpDadStatePreferred || u->DadState == IpDadStateDeprecated;
}

/* The usable unicast addresses of the adapter with this interface index, in its order. */
static int32_t ni_addresses_of(IP_ADAPTER_ADDRESSES *adapters, NET_IFINDEX index, ni_address *out) {
    int32_t n = 0;
    for (IP_ADAPTER_ADDRESSES *a = adapters; a && a->Length; a = a->Next) {
        if (a->IfIndex != index && a->Ipv6IfIndex != index) continue;
        for (IP_ADAPTER_UNICAST_ADDRESS *u = a->FirstUnicastAddress; u; u = u->Next) {
            const struct sockaddr *sa = u->Address.lpSockaddr;
            if (!sa || (sa->sa_family != AF_INET && sa->sa_family != AF_INET6) || !ni_usable(u)) continue;
            if (n >= JABS_NI_ADDRESS_CAP) jabs_not_compiled("an interface with more than JABS_NI_ADDRESS_CAP addresses (jabs_net.c)");
            memset(&out[n], 0, sizeof out[n]);
            out[n].family = sa->sa_family;
            if (sa->sa_family == AF_INET) memcpy(out[n].bytes, &((const struct sockaddr_in *)sa)->sin_addr, 4);
            else {
                memcpy(out[n].bytes, &((const struct sockaddr_in6 *)sa)->sin6_addr, 16);
                out[n].scope = ((const struct sockaddr_in6 *)sa)->sin6_scope_id;
            }
            out[n].prefix = u->OnLinkPrefixLength;
            n++;
        }
        break;
    }
    return n;
}

/* One NetworkInterface: name from the LUID (ethernet_32769, loopback_0, ...), the row's
 * description, its index, its addresses (IPv4 ones with a broadcast address), no children. */
static jabs_object *ni_object(const MIB_IF_ROW2 *row, IP_ADAPTER_ADDRESSES *adapters, ni_address *scratch) {
    char name[NDIS_IF_MAX_STRING_SIZE + 1];
    if (ConvertInterfaceLuidToNameA(&row->InterfaceLuid, name, sizeof name) != NO_ERROR) jabs_not_compiled("ConvertInterfaceLuidToNameA failed (ni_object, jabs_net.c)");
    name[NDIS_IF_MAX_STRING_SIZE] = 0;
    jabs_object *name_string = jabs_string_from_utf8(name);
    if (!name_string) return NULL;
    size_t length = wcsnlen(row->Description, IF_MAX_STRING_SIZE + 1);
    jabs_object *display = jabs_string_from_utf16((const uint16_t *)row->Description, length);
    if (!display) return NULL;
    int32_t count = ni_addresses_of(adapters, row->InterfaceIndex, scratch);
    jabs_object *addresses = jabs_net_new_inet_array(count);
    if (!addresses) return NULL;
    jabs_object *bindings = jabs_net_new_interface_address_array(count);
    if (!bindings) return NULL;
    if (row->InterfaceIndex > INT32_MAX) jabs_not_compiled("an interface index above int (ni_object, jabs_net.c)");
    jabs_object *ni = jabs_net_new_interface(name_string, display, (int32_t)row->InterfaceIndex, addresses, bindings);
    if (!ni) return NULL;
    for (int32_t i = 0; i < count; i++) {
        jabs_object *a, *broadcast = NULL;
        const ni_address *x = &scratch[i];
        if (x->family == AF_INET) {
            uint32_t v = ((uint32_t)x->bytes[0] << 24) | ((uint32_t)x->bytes[1] << 16) | ((uint32_t)x->bytes[2] << 8) | x->bytes[3];
            a = jabs_net_new_inet4((int32_t)v, NULL);
            if (!a) return NULL;
            if (x->prefix < 0 || x->prefix > 32) jabs_not_compiled("an IPv4 prefix length outside 0..32 (ni_object, jabs_net.c)");
            uint32_t mask = x->prefix == 0 ? 0 : 0xffffffffu << (32 - x->prefix);
            broadcast = jabs_net_new_inet4((int32_t)(v | ~mask), NULL);
            if (!broadcast) return NULL;
        } else {
            if (x->scope > INT32_MAX) jabs_not_compiled("an IPv6 scope id above int (ni_object, jabs_net.c)");
            a = jabs_net_new_inet6(x->bytes, (int32_t)x->scope, NULL);
            if (!a) return NULL;
            /* A scoped address names its interface (Inet6Address.scope_ifname). */
            if (x->scope != 0) jabs_net_set_scope_interface(a, ni);
        }
        jabs_object *binding = jabs_net_new_interface_address(a, broadcast, (int16_t)x->prefix);
        if (!binding) return NULL;
        ((jabs_object **)jabs_array_data((jabs_array *)addresses))[i] = a;
        ((jabs_object **)jabs_array_data((jabs_array *)bindings))[i] = binding;
    }
    return ni;
}

/* NetworkInterface.getAll: every row of GetIfTable2, in its order. */
jabs_object *jabs_net_interfaces(void) {
    net_ready();
    MIB_IF_TABLE2 *table = NULL;
    if (GetIfTable2(&table) != NO_ERROR || !table) { jabs_net_raise(JABS_NET_SOCKET, "GetIfTable2 failed"); return NULL; }
    IP_ADAPTER_ADDRESSES *adapters = ni_adapters();
    ni_address *scratch = (ni_address *)malloc(sizeof(ni_address) * JABS_NI_ADDRESS_CAP);
    jabs_object *result = NULL;
    if (adapters && scratch) {
        if (table->NumEntries > INT32_MAX) jabs_not_compiled("more interfaces than an array holds (jabs_net_interfaces, jabs_net.c)");
        result = jabs_net_new_interface_array((int32_t)table->NumEntries);
        for (ULONG i = 0; result && i < table->NumEntries; i++) {
            jabs_object *ni = ni_object(&table->Table[i], adapters, scratch);
            if (!ni) { result = NULL; break; }
            ((jabs_object **)jabs_array_data((jabs_array *)result))[i] = ni;
        }
    } else if (!scratch) {
        jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
    }
    free(scratch);
    free(adapters);
    FreeMibTable(table);
    return result;
}

/* The MIB_IF_ROW2 of an interface index; 0 when there is none. */
static int ni_row(int32_t index, MIB_IF_ROW2 *row) {
    memset(row, 0, sizeof *row);
    if (index < 0) return 0;
    row->InterfaceIndex = (NET_IFINDEX)index;
    return GetIfEntry2(row) == NO_ERROR;
}

/* NetworkInterface.isLoopback0: whether the interface is the software loopback. */
int32_t jabs_net_interface_is_loopback(int32_t index) {
    net_ready();
    MIB_IF_ROW2 row;
    return ni_row(index, &row) && row.Type == IF_TYPE_SOFTWARE_LOOPBACK;
}

/* NetworkInterface.getMacAddr0: the physical address, or null when it has none. */
jabs_object *jabs_net_interface_mac(int32_t index) {
    net_ready();
    MIB_IF_ROW2 row;
    if (!ni_row(index, &row) || row.PhysicalAddressLength == 0) return NULL;
    if (row.PhysicalAddressLength > IF_MAX_PHYS_ADDRESS_LENGTH) jabs_not_compiled("a physical address longer than IF_MAX_PHYS_ADDRESS_LENGTH (jabs_net.c)");
    jabs_object *bytes = jabs_new_array((int32_t)row.PhysicalAddressLength, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
    if (bytes) memcpy(jabs_array_data((jabs_array *)bytes), row.PhysicalAddress, row.PhysicalAddressLength);
    return bytes;
}

/* NetworkInterface.boundInetAddress0: whether any interface has this address. */
int32_t jabs_net_address_is_bound(jabs_object *address) {
    uint8_t bytes[16];
    int32_t scope = 0;
    int32_t family = jabs_net_inet_read(address, bytes, &scope);
    if (family == 0) return 0;
    net_ready();
    IP_ADAPTER_ADDRESSES *adapters = ni_adapters();
    if (!adapters) return 0;
    int wanted = family == 1 ? AF_INET : AF_INET6;
    size_t length = family == 1 ? 4 : 16;
    int32_t bound = 0;
    for (IP_ADAPTER_ADDRESSES *a = adapters; a && a->Length && !bound; a = a->Next) {
        for (IP_ADAPTER_UNICAST_ADDRESS *u = a->FirstUnicastAddress; u && !bound; u = u->Next) {
            const struct sockaddr *sa = u->Address.lpSockaddr;
            if (!sa || sa->sa_family != wanted || !ni_usable(u)) continue;
            const void *b = wanted == AF_INET ? (const void *)&((const struct sockaddr_in *)sa)->sin_addr : (const void *)&((const struct sockaddr_in6 *)sa)->sin6_addr;
            bound = memcmp(b, bytes, length) == 0;
        }
    }
    free(adapters);
    return bound;
}
#else
#include <ifaddrs.h>

/* NetworkInterface.boundInetAddress0: whether an interface has the address (getifaddrs,
 * which lists the IPv4 and IPv6 addresses the JDK reads with SIOCGIFCONF and
 * /proc/net/if_inet6). IPv6 is asked only while IPv6 is available, as the JDK does. */
int32_t jabs_net_address_is_bound(jabs_object *address) {
    uint8_t bytes[16];
    int32_t scope = 0;
    int32_t family = jabs_net_inet_read(address, bytes, &scope);
    if (family == 0) return 0;
    if (family == 2 && !jabs_net_ipv6_available()) return 0;
    struct ifaddrs *list = NULL;
    if (getifaddrs(&list) != 0) { raise_errno(JABS_NET_SOCKET, errno, "getifaddrs failed"); return 0; }
    int wanted = family == 1 ? AF_INET : AF_INET6;
    size_t length = family == 1 ? 4 : 16;
    int32_t bound = 0;
    for (struct ifaddrs *i = list; i && !bound; i = i->ifa_next) {
        if (!i->ifa_addr || i->ifa_addr->sa_family != wanted) continue;
        const void *b = wanted == AF_INET ? (const void *)&((const struct sockaddr_in *)i->ifa_addr)->sin_addr : (const void *)&((const struct sockaddr_in6 *)i->ifa_addr)->sin6_addr;
        bound = memcmp(b, bytes, length) == 0;
    }
    freeifaddrs(list);
    return bound;
}

/* The rest of NetworkInterface stops on Linux: the JDK's interface list there (SIOCGIFCONF,
 * /proc/net/if_inet6, its own ordering) is not implemented. */
jabs_object *jabs_net_interfaces(void) { jabs_not_compiled("NetworkInterface.getAll on Linux (jabs_net.c)"); }
int32_t jabs_net_interface_is_loopback(int32_t index) { (void)index; jabs_not_compiled("NetworkInterface.isLoopback0 on Linux (jabs_net.c)"); }
jabs_object *jabs_net_interface_mac(int32_t index) { (void)index; jabs_not_compiled("NetworkInterface.getMacAddr0 on Linux (jabs_net.c)"); }
#endif

/* ---- jdk.net extended socket options: which the OS has ---- */

/* socketOptionSupported in the JDK's WindowsSocketOptions.c / LinuxSocketOptions.c: a TCP
 * socket's getsockopt of the option fails with "no such option" only when it is missing. */
#ifdef _WIN32
/* ws2ipdef.h values (Windows 10 1709 and later), for headers without them. */
#ifndef TCP_KEEPIDLE
#define TCP_KEEPIDLE 3
#endif
#ifndef TCP_KEEPCNT
#define TCP_KEEPCNT 16
#endif
#ifndef TCP_KEEPINTVL
#define TCP_KEEPINTVL 17
#endif
static int32_t option_supported(int level, int option) {
    net_ready();
    SOCKET s = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (s == INVALID_SOCKET) return 0;
    int value = 0, length = sizeof value;
    int r = getsockopt(s, level, option, (char *)&value, &length);
    int supported = !(r != 0 && WSAGetLastError() == WSAENOPROTOOPT);
    closesocket(s);
    return supported;
}

int32_t jabs_net_extended_option_supported(int32_t which) {
    if (which != JABS_NET_EXTENDED_KEEPALIVE) jabs_not_compiled("a Linux-only extended socket option asked on Windows (jabs_net.c)");
    return option_supported(IPPROTO_TCP, TCP_KEEPIDLE) && option_supported(IPPROTO_TCP, TCP_KEEPCNT) && option_supported(IPPROTO_TCP, TCP_KEEPINTVL);
}
#else
/* Linux's netinet/tcp.h and asm/socket.h values, hidden from strict POSIX headers. */
#ifndef TCP_KEEPIDLE
#define TCP_KEEPIDLE 4
#endif
#ifndef TCP_KEEPINTVL
#define TCP_KEEPINTVL 5
#endif
#ifndef TCP_KEEPCNT
#define TCP_KEEPCNT 6
#endif
#ifndef TCP_QUICKACK
#define TCP_QUICKACK 12
#endif
#ifndef SO_INCOMING_NAPI_ID
#define SO_INCOMING_NAPI_ID 56
#endif
static int32_t option_supported(int level, int option) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    if (s < 0) return 0;
    int value = 0;
    socklen_t length = sizeof value;
    int r = getsockopt(s, level, option, &value, &length);
    int supported = !(r != 0 && errno == ENOPROTOOPT);
    close(s);
    return supported;
}

int32_t jabs_net_extended_option_supported(int32_t which) {
    switch (which) {
    case JABS_NET_EXTENDED_KEEPALIVE: return option_supported(IPPROTO_TCP, TCP_KEEPIDLE) && option_supported(IPPROTO_TCP, TCP_KEEPCNT) && option_supported(IPPROTO_TCP, TCP_KEEPINTVL);
    case JABS_NET_EXTENDED_QUICKACK: return option_supported(IPPROTO_TCP, TCP_QUICKACK);
    case JABS_NET_EXTENDED_NAPI_ID: return option_supported(SOL_SOCKET, SO_INCOMING_NAPI_ID);
    default: jabs_not_compiled("an unknown extended socket option (jabs_net.c)");
    }
}
#endif
