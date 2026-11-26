//! java.net and java.nio natives: the name resolver (InetAddress and its Impl classes), NIO
//! sockets (sun.nio.ch.Net, IOUtil, SocketDispatcher), the selectors (WEPoll on Windows, EPoll
//! on Linux) and the descriptors they work on. Each body calls the handwritten glue in
//! runtime/jabs_net.c, which mirrors what the JDK's own natives (libnet and libnio) do on the
//! same OS: the same OS calls, the same result order, the same exception classes and messages.
//! The constants the JDK's natives answer with were read on the reference JVMs (JDK 25.0.4 on
//! Windows, 25.0.5 on Linux) and are compared on every test run (tests/networking.rs).
//!
//! What C needs from Java objects (InetAddress holders, InetSocketAddress, exception classes) is
//! generated here by [`glue`], so jabs_net.c stays independent of any one JDK's field layout.
//! A native left out stops the program with a diagnostic when it is called.

use super::*;

const INET_ADDRESS: &str = "java/net/InetAddress";
const INET4_ADDRESS: &str = "java/net/Inet4Address";
const INET6_ADDRESS: &str = "java/net/Inet6Address";
const HOLDER: &str = "java/net/InetAddress$InetAddressHolder";
const HOLDER6: &str = "java/net/Inet6Address$Inet6AddressHolder";
const SOCKET_ADDRESS: &str = "java/net/InetSocketAddress";
const SOCKET_ADDRESS_INIT: &str = "(Ljava/net/InetAddress;I)V";
/// Whether UnknownHostException messages name the host: the JDK's resolver natives ask
/// jdk.internal.util.Exceptions.enhancedNonSocketExceptions(), which is
/// `jdk.includeInExceptions` having hostInfo or hostInfoExclSocket once the VM has booted. JABS
/// runs only System.initPhase1, so VM.isBooted() stays false and Exceptions never reads the
/// property; the glue asks SecurityProperties for those two values itself, which is the answer
/// every program running after boot (all of them, on the JVM) gets.
const SECURITY_PROPERTIES: &str = "sun/security/util/SecurityProperties";
const INCLUDED: (&str, &str) = ("includedInExceptions", "(Ljava/lang/String;)Z");
/// `Boolean.getBoolean("java.net.preferIPv4Stack")`, which the JDK's libnet reads once when it
/// loads: true makes IPv6 unavailable.
const BOOLEAN: &str = "java/lang/Boolean";
const GET_BOOLEAN: (&str, &str) = ("getBoolean", "(Ljava/lang/String;)Z");

/// Exceptions the net glue raises with a message, by the kind jabs_net.c passes
/// (JABS_NET_* in runtime/jabs_rt.h, in this order). Each is built with its (String)
/// constructor, so it carries a stack trace as one thrown by a JDK native does.
const RAISED: [(&str, &str); 7] = [
    ("java/net/UnknownHostException", "JABS_NET_UNKNOWN_HOST"),
    ("java/net/SocketException", "JABS_NET_SOCKET"),
    ("java/net/BindException", "JABS_NET_BIND"),
    ("java/net/ConnectException", "JABS_NET_CONNECT"),
    ("java/net/NoRouteToHostException", "JABS_NET_NO_ROUTE"),
    ("java/net/ProtocolException", "JABS_NET_PROTOCOL"),
    (
        "sun/net/ConnectionResetException",
        "JABS_NET_CONNECTION_RESET",
    ),
];
const STRING_INIT: &str = "(Ljava/lang/String;)V";

/// Whether a native belongs to the classes this module implements.
fn is_net_class(class: &str) -> bool {
    matches!(
        class,
        "java/net/InetAddress"
            | "java/net/Inet4Address"
            | "java/net/Inet6Address"
            | "java/net/Inet4AddressImpl"
            | "java/net/Inet6AddressImpl"
            | "java/net/NetworkInterface"
            | "sun/security/provider/NativeSeedGenerator"
            | "jdk/net/WindowsSocketOptions"
            | "jdk/net/LinuxSocketOptions"
    ) || class.starts_with("sun/nio/ch/")
}

/// Java methods a net native calls from C (no Java instruction names them), so reachable()
/// keeps them: (class, name, descriptor). Constructors also make their class instantiated.
pub(super) fn called_by(key: &MethodKey) -> Vec<(&'static str, &'static str, &'static str)> {
    if !is_net_class(&key.0) {
        return Vec::new();
    }
    let mut calls = vec![
        (INET4_ADDRESS, "<init>", "()V"),
        (INET6_ADDRESS, "<init>", "()V"),
        (SOCKET_ADDRESS, "<init>", SOCKET_ADDRESS_INIT),
        (SECURITY_PROPERTIES, INCLUDED.0, INCLUDED.1),
        (BOOLEAN, GET_BOOLEAN.0, GET_BOOLEAN.1),
        (NETWORK_INTERFACE, "<init>", "()V"),
        (INTERFACE_ADDRESS, "<init>", "()V"),
    ];
    for (class, _) in RAISED {
        calls.push((class, "<init>", STRING_INIT));
    }
    calls
}

/// The body of a net native, if JABS implements it: (C body, needs the class registry).
pub(super) fn intrinsic(key: &MethodKey, classes_available: bool) -> Option<(String, bool)> {
    if !is_net_class(&key.0) {
        return None;
    }
    let body: &str = match (key.0.as_str(), key.1.as_str(), key.2.as_str()) {
        // The JDK's init natives cache field and method ids; JABS's are compiled in.
        ("java/net/InetAddress", "init", "()V")
        | ("java/net/Inet4Address", "init", "()V")
        | ("java/net/Inet6Address", "init", "()V")
        | ("java/net/NetworkInterface", "init", "()V") => {
            "/* field offsets are known at compile time */"
        }
        ("java/net/InetAddress", "isIPv4Available", "()Z") => "return jabs_net_ipv4_available();",
        ("java/net/InetAddress", "isIPv6Supported", "()Z") => "return jabs_net_ipv6_available();",
        ("java/net/Inet4AddressImpl", "getLocalHostName", "()Ljava/lang/String;")
        | ("java/net/Inet6AddressImpl", "getLocalHostName", "()Ljava/lang/String;") => {
            "(void)self; return jabs_net_local_host_name();"
        }
        // Inet4AddressImpl asks for IPv4 only, in the resolver's order (characteristics 1:
        // IPV4 without a FIRST flag), as the JDK's Inet4AddressImpl native does.
        (
            "java/net/Inet4AddressImpl",
            "lookupAllHostAddr",
            "(Ljava/lang/String;)[Ljava/net/InetAddress;",
        ) => "(void)self; return jabs_net_lookup(a0, 1);",
        (
            "java/net/Inet6AddressImpl",
            "lookupAllHostAddr",
            "(Ljava/lang/String;I)[Ljava/net/InetAddress;",
        ) => "(void)self; return jabs_net_lookup(a0, a1);",
        ("java/net/Inet4AddressImpl", "getHostByAddr", "([B)Ljava/lang/String;")
        | ("java/net/Inet6AddressImpl", "getHostByAddr", "([B)Ljava/lang/String;") => {
            "(void)self; return jabs_net_host_by_addr(a0);"
        }
        _ => socket_body(key)?,
    };
    classes_available.then(|| (body.to_string(), true))
}

const NET: &str = "sun/nio/ch/Net";
const IOUTIL: &str = "sun/nio/ch/IOUtil";

/// NIO's socket natives: sun.nio.ch.Net, IOUtil and SocketDispatcher, and Linux's
/// UnixDispatcher and NativeThread (runtime/jabs_net.c). Option levels and numbers, poll bits
/// and SHUT_* values come from Java as the OS's own (the JDK generates them per platform).
fn socket_body(key: &MethodKey) -> Option<&'static str> {
    Some(match (key.0.as_str(), key.1.as_str(), key.2.as_str()) {
        (NET, "initIDs", "()V") => "/* field offsets are known at compile time */",
        (IOUTIL, "initIDs", "()V") => "jabs_net_ioutil_init();",
        (NET, "isIPv6Available0", "()Z") => "return jabs_net_ipv6_available();",
        (NET, "isReusePortAvailable0", "()Z") => {
            "return jabs_net_value(JABS_NET_VALUE_REUSE_PORT) != 0;"
        }
        (NET, "isExclusiveBindAvailable", "()I") => {
            "return (int32_t)jabs_net_value(JABS_NET_VALUE_EXCLUSIVE_BIND);"
        }
        (NET, "shouldShutdownWriteBeforeClose0", "()Z") => {
            "return jabs_net_value(JABS_NET_VALUE_SHUTDOWN_WRITE_BEFORE_CLOSE) != 0;"
        }
        (NET, "shouldSetBothIPv4AndIPv6Options0", "()Z") => {
            "return jabs_net_value(JABS_NET_VALUE_SET_BOTH_IPV4_IPV6_OPTIONS) != 0;"
        }
        (NET, "canIPv6SocketJoinIPv4Group0", "()Z") => {
            "return jabs_net_value(JABS_NET_VALUE_IPV6_JOINS_IPV4_GROUP) != 0;"
        }
        (NET, "canJoin6WithIPv4Group0", "()Z") => {
            "return jabs_net_value(JABS_NET_VALUE_JOIN6_WITH_IPV4_GROUP) != 0;"
        }
        (NET, "canUseIPv6OptionsWithIPv4LocalAddress0", "()Z") => {
            "return jabs_net_value(JABS_NET_VALUE_IPV6_OPTIONS_WITH_IPV4_LOCAL) != 0;"
        }
        (NET, "pollinValue", "()S") => "return (int16_t)jabs_net_value(JABS_NET_VALUE_POLLIN);",
        (NET, "polloutValue", "()S") => "return (int16_t)jabs_net_value(JABS_NET_VALUE_POLLOUT);",
        (NET, "pollerrValue", "()S") => "return (int16_t)jabs_net_value(JABS_NET_VALUE_POLLERR);",
        (NET, "pollhupValue", "()S") => "return (int16_t)jabs_net_value(JABS_NET_VALUE_POLLHUP);",
        (NET, "pollnvalValue", "()S") => "return (int16_t)jabs_net_value(JABS_NET_VALUE_POLLNVAL);",
        (NET, "pollconnValue", "()S") => "return (int16_t)jabs_net_value(JABS_NET_VALUE_POLLCONN);",
        (NET, "socket0", "(ZZZZ)I") => "return jabs_net_socket(a0, a1, a2, a3);",
        (NET, "bind0", "(Ljava/io/FileDescriptor;ZZLjava/net/InetAddress;I)V") => {
            "jabs_net_bind(a0, a1, a2, a3, a4);"
        }
        (NET, "listen", "(Ljava/io/FileDescriptor;I)V") => "jabs_net_listen(a0, a1);",
        (NET, "connect0", "(ZLjava/io/FileDescriptor;Ljava/net/InetAddress;I)I") => {
            "return jabs_net_connect(a0, a1, a2, a3);"
        }
        (
            NET,
            "accept",
            "(Ljava/io/FileDescriptor;Ljava/io/FileDescriptor;[Ljava/net/InetSocketAddress;)I",
        ) => "return jabs_net_accept(a0, a1, a2);",
        (NET, "shutdown", "(Ljava/io/FileDescriptor;I)V") => "jabs_net_shutdown(a0, a1);",
        (NET, "localPort", "(Ljava/io/FileDescriptor;)I") => "return jabs_net_port(a0, 0);",
        (NET, "remotePort", "(Ljava/io/FileDescriptor;)I") => "return jabs_net_port(a0, 1);",
        (NET, "localInetAddress", "(Ljava/io/FileDescriptor;)Ljava/net/InetAddress;") => {
            "return jabs_net_address(a0, 0);"
        }
        (NET, "remoteInetAddress", "(Ljava/io/FileDescriptor;)Ljava/net/InetAddress;") => {
            "return jabs_net_address(a0, 1);"
        }
        (NET, "getIntOption0", "(Ljava/io/FileDescriptor;ZII)I") => {
            "return jabs_net_get_option(a0, a1, a2, a3);"
        }
        (NET, "setIntOption0", "(Ljava/io/FileDescriptor;ZIIIZ)V") => {
            "jabs_net_set_option(a0, a1, a2, a3, a4, a5);"
        }
        (NET, "poll", "(Ljava/io/FileDescriptor;IJ)I") => "return jabs_net_poll(a0, a1, a2);",
        (NET, "pollConnect", "(Ljava/io/FileDescriptor;J)Z") => {
            "return jabs_net_poll_connect(a0, a1);"
        }
        (NET, "available", "(Ljava/io/FileDescriptor;)I") => "return jabs_net_available(a0);",
        (IOUTIL, "fdVal", "(Ljava/io/FileDescriptor;)I") => "return jabs_net_fd_val(a0);",
        (IOUTIL, "setfdVal", "(Ljava/io/FileDescriptor;I)V") => "jabs_net_set_fd_val(a0, a1);",
        (IOUTIL, "configureBlocking", "(Ljava/io/FileDescriptor;Z)V") => {
            "jabs_net_configure_blocking(a0, a1);"
        }
        (IOUTIL, "iovMax", "()I") => "return (int32_t)jabs_net_value(JABS_NET_VALUE_IOV_MAX);",
        (IOUTIL, "writevMax", "()J") => "return jabs_net_value(JABS_NET_VALUE_WRITEV_MAX);",
        (IOUTIL, "fdLimit", "()I") => "return jabs_net_fd_limit();",
        (IOUTIL, "drain", "(I)Z") => "return jabs_net_drain(a0);",
        (IOUTIL, "write1", "(IB)I") => "return jabs_net_write1(a0, a1);",
        (IOUTIL, "makePipe", "(Z)J") => "return jabs_net_make_pipe(a0);",
        ("sun/nio/ch/SocketDispatcher", "read0", "(Ljava/io/FileDescriptor;JI)I") => {
            "return jabs_net_read(a0, a1, a2);"
        }
        ("sun/nio/ch/SocketDispatcher", "write0", "(Ljava/io/FileDescriptor;JI)I") => {
            "return jabs_net_write(a0, a1, a2);"
        }
        ("sun/nio/ch/SocketDispatcher", "close0", "(I)V") => "jabs_net_close_socket(a0);",
        ("sun/nio/ch/UnixDispatcher", "init", "()V") => "jabs_net_dispatcher_init();",
        ("sun/nio/ch/UnixDispatcher", "preClose0", "(Ljava/io/FileDescriptor;)V") => {
            "jabs_net_pre_close(a0);"
        }
        ("sun/nio/ch/UnixDispatcher", "close0", "(Ljava/io/FileDescriptor;)V") => {
            "int32_t fd = jabs_net_fd_val(a0); if (!jabs_pending) jabs_net_close_fd(fd);"
        }
        ("sun/nio/ch/NativeThread", "init", "()V") => "jabs_net_native_thread_init();",
        ("sun/nio/ch/NativeThread", "current0", "()J") => {
            "return jabs_net_native_thread_current();"
        }
        ("sun/nio/ch/NativeThread", "signal0", "(J)V") => "jabs_net_native_thread_signal(a0);",
        // Whether a signal sent to a thread not yet blocked stays pending (false on Linux, as
        // the reference JVM reports; true only on macOS).
        ("sun/nio/ch/NativeThread", "supportPendingSignals0", "()Z") => "return 0;",
        _ => return selector_body(key),
    })
}

const WEPOLL: &str = "sun/nio/ch/WEPoll";
const EPOLL: &str = "sun/nio/ch/EPoll";
const UNIX_DOMAIN: &str = "sun/nio/ch/UnixDomainSockets";
const NETWORK_INTERFACE: &str = "java/net/NetworkInterface";
const INTERFACE_ADDRESS: &str = "java/net/InterfaceAddress";

/// Selectors (WEPoll on Windows, EPoll and EventFD on Linux), the Unix domain sockets of the
/// Windows selector's wakeup pipe, the seed its SecureRandom takes, and NetworkInterface, whose
/// list SecureRandom's seed generator hashes on Windows (runtime/jabs_net.c).
fn selector_body(key: &MethodKey) -> Option<&'static str> {
    Some(match (key.0.as_str(), key.1.as_str(), key.2.as_str()) {
        (WEPOLL, "eventSize", "()I") | (EPOLL, "eventSize", "()I") => {
            "return jabs_net_epoll_event_size();"
        }
        (WEPOLL, "eventsOffset", "()I") | (EPOLL, "eventsOffset", "()I") => {
            "return jabs_net_epoll_events_offset();"
        }
        (WEPOLL, "dataOffset", "()I") | (EPOLL, "dataOffset", "()I") => {
            "return jabs_net_epoll_data_offset();"
        }
        (WEPOLL, "create", "()J") => "return jabs_net_epoll_create();",
        (WEPOLL, "ctl", "(JIJI)I") => "return jabs_net_epoll_ctl(a0, a1, a2, a3);",
        (WEPOLL, "wait", "(JJII)I") => "return jabs_net_epoll_wait(a0, a1, a2, a3);",
        (WEPOLL, "close", "(J)V") => "jabs_net_epoll_close(a0);",
        (EPOLL, "create", "()I") => "return jabs_net_linux_epoll_create();",
        (EPOLL, "ctl", "(IIII)I") => "return jabs_net_linux_epoll_ctl(a0, a1, a2, a3);",
        (EPOLL, "wait", "(IJII)I") => "return jabs_net_linux_epoll_wait(a0, a1, a2, a3);",
        ("sun/nio/ch/EventFD", "eventfd0", "()I") => "return jabs_net_eventfd();",
        ("sun/nio/ch/EventFD", "set0", "(I)I") => "return jabs_net_eventfd_set(a0);",
        (UNIX_DOMAIN, "init", "()Z") => "return jabs_net_uds_init();",
        (UNIX_DOMAIN, "socket0", "()I") => "return jabs_net_uds_socket();",
        (UNIX_DOMAIN, "bind0", "(Ljava/io/FileDescriptor;[B)V") => "jabs_net_uds_bind(a0, a1);",
        (UNIX_DOMAIN, "connect0", "(Ljava/io/FileDescriptor;[B)I") => {
            "return jabs_net_uds_connect(a0, a1);"
        }
        (
            UNIX_DOMAIN,
            "accept0",
            "(Ljava/io/FileDescriptor;Ljava/io/FileDescriptor;[Ljava/lang/Object;)I",
        ) => "return jabs_net_uds_accept(a0, a1, a2);",
        (UNIX_DOMAIN, "localAddress0", "(Ljava/io/FileDescriptor;)[B") => {
            "return jabs_net_uds_local_address(a0);"
        }
        ("sun/security/provider/NativeSeedGenerator", "nativeGenerateSeed", "([B)Z") => {
            "return jabs_net_generate_seed(a0);"
        }
        (NETWORK_INTERFACE, "getAll", "()[Ljava/net/NetworkInterface;") => {
            "return jabs_net_interfaces();"
        }
        (NETWORK_INTERFACE, "isLoopback0", "(Ljava/lang/String;I)Z") => {
            "(void)a0; return jabs_net_interface_is_loopback(a1);"
        }
        (NETWORK_INTERFACE, "getMacAddr0", "([BLjava/lang/String;I)[B") => {
            "(void)a0; (void)a1; return jabs_net_interface_mac(a2);"
        }
        (NETWORK_INTERFACE, "boundInetAddress0", "(Ljava/net/InetAddress;)Z") => {
            "return jabs_net_address_is_bound(a0);"
        }
        // jdk.net.ExtendedSocketOptions asks which TCP options the OS has when it loads.
        ("jdk/net/WindowsSocketOptions", "keepAliveOptionsSupported0", "()Z")
        | ("jdk/net/LinuxSocketOptions", "keepAliveOptionsSupported0", "()Z") => {
            "return jabs_net_extended_option_supported(JABS_NET_EXTENDED_KEEPALIVE);"
        }
        ("jdk/net/LinuxSocketOptions", "quickAckSupported0", "()Z") => {
            "return jabs_net_extended_option_supported(JABS_NET_EXTENDED_QUICKACK);"
        }
        ("jdk/net/LinuxSocketOptions", "incomingNapiIdSupported0", "()Z") => {
            "return jabs_net_extended_option_supported(JABS_NET_EXTENDED_NAPI_ID);"
        }
        _ => return None,
    })
}

/// C glue the net natives call (declared in runtime/jabs_rt.h). Each function whose classes,
/// fields or constructors are missing from the program stops when called, naming what is
/// missing, instead of failing to compile.
pub(super) fn glue(
    unit: &Unit,
    methods: &BTreeSet<MethodKey>,
    records: &BTreeSet<String>,
    inits: &BTreeSet<String>,
) -> String {
    let has_field = |class: &str, field: &str, descriptor: &str| {
        records.contains(class)
            && unit.get(class).is_some_and(|c| {
                c.fields.iter().any(|f| {
                    f.name == field && f.descriptor == descriptor && f.access & ACC_STATIC == 0
                })
            })
    };
    let has_method = |class: &str, name: &str, descriptor: &str| {
        unit.get(class).is_some()
            && records.contains(class)
            && inits.contains(class)
            && methods.contains(&(class.to_string(), name.to_string(), descriptor.to_string()))
    };
    let at = |class: &str, object: &str, field: &str| {
        format!(
            "(({} *){object})->{}",
            object_struct(class),
            instance_field(field)
        )
    };
    let mut out = String::from("\n/* ---- java.net glue (src/cgen/net.rs) ---- */\n");
    let holder_ok = has_field(INET_ADDRESS, "holder", &format!("L{HOLDER};"))
        && has_field(HOLDER, "address", "I")
        && has_field(HOLDER, "family", "I")
        && has_field(HOLDER, "hostName", "Ljava/lang/String;")
        && has_field(HOLDER, "originalHostName", "Ljava/lang/String;");
    let holder6_ok = has_field(INET6_ADDRESS, "holder6", &format!("L{HOLDER6};"))
        && has_field(HOLDER6, "ipaddress", "[B")
        && has_field(HOLDER6, "scope_id", "I")
        && has_field(HOLDER6, "scope_id_set", "Z");
    // Holder fields are two levels deep: the InetAddress's holder, then the holder's field.
    let holder_field = |object: &str, field: &str| {
        format!(
            "(({} *){})->{}",
            object_struct(HOLDER),
            at(INET_ADDRESS, object, "holder"),
            instance_field(field)
        )
    };
    let holder6_field = |object: &str, field: &str| {
        format!(
            "(({} *){})->{}",
            object_struct(HOLDER6),
            at(INET6_ADDRESS, object, "holder6"),
            instance_field(field)
        )
    };

    // InetAddress construction, as the JDK's natives do it: the no-arg constructor, then the
    // holder's address and both host names (setInetAddress_addr, setInetAddress_hostName).
    let new4 = "jabs_object *jabs_net_new_inet4(int32_t address, jabs_object *host)";
    if holder_ok && has_method(INET4_ADDRESS, "<init>", "()V") {
        let _ = writeln!(
            out,
            "{new4} {{\n    if ({init}() != 0) return NULL;\n    jabs_object *a = jabs_new(&{record});\n    if (!a) return NULL;\n    {ctor}(a);\n    if (jabs_pending) return NULL;\n    if (!{h}) jabs_not_compiled(\"Inet4Address() left no holder (jabs_net_new_inet4, src/cgen/net.rs)\");\n    {addr} = address;\n    {name} = host;\n    {orig} = host;\n    return a;\n}}",
            init = init_symbol(INET4_ADDRESS),
            record = class_record(INET4_ADDRESS),
            ctor = symbol(INET4_ADDRESS, "<init>", "()V"),
            h = at(INET_ADDRESS, "a", "holder"),
            addr = holder_field("a", "address"),
            name = holder_field("a", "hostName"),
            orig = holder_field("a", "originalHostName"),
        );
    } else {
        let _ = writeln!(
            out,
            "{new4} {{ (void)address; (void)host; jabs_not_compiled(\"Inet4Address construction (its class, constructor or holder fields are not in the program; src/cgen/net.rs)\"); }}"
        );
    }
    let new6 = "jabs_object *jabs_net_new_inet6(const uint8_t *bytes, int32_t scope_id, jabs_object *host)";
    if holder_ok && holder6_ok && has_method(INET6_ADDRESS, "<init>", "()V") {
        let _ = writeln!(
            out,
            "{new6} {{\n    if ({init}() != 0) return NULL;\n    jabs_object *a = jabs_new(&{record});\n    if (!a) return NULL;\n    {ctor}(a);\n    if (jabs_pending) return NULL;\n    if (!{h} || !{h6}) jabs_not_compiled(\"Inet6Address() left no holder (jabs_net_new_inet6, src/cgen/net.rs)\");\n    jabs_array *ip = (jabs_array *){ip};\n    if (!ip || ip->length != 16) jabs_not_compiled(\"Inet6Address() without a 16-byte ipaddress (jabs_net_new_inet6, src/cgen/net.rs)\");\n    memcpy(jabs_array_data(ip), bytes, 16);\n    /* As setInet6Address_scopeid: zero is the default and stays unset. */\n    if (scope_id != 0) {{ {scope} = scope_id; {scope_set} = 1; }}\n    {name} = host;\n    {orig} = host;\n    return a;\n}}",
            init = init_symbol(INET6_ADDRESS),
            record = class_record(INET6_ADDRESS),
            ctor = symbol(INET6_ADDRESS, "<init>", "()V"),
            h = at(INET_ADDRESS, "a", "holder"),
            h6 = at(INET6_ADDRESS, "a", "holder6"),
            ip = holder6_field("a", "ipaddress"),
            scope = holder6_field("a", "scope_id"),
            scope_set = holder6_field("a", "scope_id_set"),
            name = holder_field("a", "hostName"),
            orig = holder_field("a", "originalHostName"),
        );
    } else {
        let _ = writeln!(
            out,
            "{new6} {{ (void)bytes; (void)scope_id; (void)host; jabs_not_compiled(\"Inet6Address construction (its class, constructor or holder fields are not in the program; src/cgen/net.rs)\"); }}"
        );
    }
    let new_array = "jabs_object *jabs_net_new_inet_array(int32_t length)";
    if records.contains(INET_ADDRESS) {
        let _ = writeln!(
            out,
            "{new_array} {{ return jabs_new_array(length, sizeof(jabs_object *), JABS_COMPONENT_CLASS, &{}, \"[L{INET_ADDRESS};\", NULL); }}",
            class_record(INET_ADDRESS)
        );
    } else {
        let _ = writeln!(
            out,
            "{new_array} {{ (void)length; jabs_not_compiled(\"InetAddress[] (InetAddress is not in the program; src/cgen/net.rs)\"); }}"
        );
    }
    // Reading an InetAddress back (NET_InetAddressToSockaddr): its family, and its address.
    let read = "int32_t jabs_net_inet_read(jabs_object *a, uint8_t *bytes, int32_t *scope_id)";
    if holder_ok && holder6_ok && records.contains(INET4_ADDRESS) {
        let _ = writeln!(
            out,
            "/* An InetAddress's family (1 IPv4, 2 IPv6) with its address in network order in bytes\n * (4 or 16 of them) and an IPv6 scope id (0 when unset); 0 with NullPointerException for null. */\n{read} {{\n    if (!a) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }}\n    if (!{h}) jabs_not_compiled(\"an InetAddress without a holder (jabs_net_inet_read, src/cgen/net.rs)\");\n    *scope_id = 0;\n    int32_t family = {family};\n    if (family == 1) {{ uint32_t v = (uint32_t){addr}; bytes[0] = (uint8_t)(v >> 24); bytes[1] = (uint8_t)(v >> 16); bytes[2] = (uint8_t)(v >> 8); bytes[3] = (uint8_t)v; return 1; }}\n    if (family != 2 || !jabs_is_instance(a, &{record6})) jabs_not_compiled(\"an InetAddress of neither family (jabs_net_inet_read, src/cgen/net.rs)\");\n    if (!{h6}) jabs_not_compiled(\"an Inet6Address without holder6 (jabs_net_inet_read, src/cgen/net.rs)\");\n    jabs_array *ip = (jabs_array *){ip};\n    if (!ip || ip->length != 16) jabs_not_compiled(\"an Inet6Address without a 16-byte ipaddress (jabs_net_inet_read, src/cgen/net.rs)\");\n    memcpy(bytes, jabs_array_data(ip), 16);\n    if ({scope_set}) *scope_id = {scope};\n    return 2;\n}}",
            h = at(INET_ADDRESS, "a", "holder"),
            family = holder_field("a", "family"),
            addr = holder_field("a", "address"),
            record6 = class_record(INET6_ADDRESS),
            h6 = at(INET6_ADDRESS, "a", "holder6"),
            ip = holder6_field("a", "ipaddress"),
            scope = holder6_field("a", "scope_id"),
            scope_set = holder6_field("a", "scope_id_set"),
        );
    } else {
        let _ = writeln!(
            out,
            "{read} {{ (void)a; (void)bytes; (void)scope_id; jabs_not_compiled(\"reading an InetAddress (its holder fields are not in the program; src/cgen/net.rs)\"); }}"
        );
    }
    let new_isa = "jabs_object *jabs_net_new_socket_address(jabs_object *address, int32_t port)";
    if has_method(SOCKET_ADDRESS, "<init>", SOCKET_ADDRESS_INIT) {
        let _ = writeln!(
            out,
            "{new_isa} {{\n    if ({init}() != 0) return NULL;\n    jabs_object *s = jabs_new(&{record});\n    if (!s) return NULL;\n    {ctor}(s, address, port);\n    return jabs_pending ? NULL : s;\n}}",
            init = init_symbol(SOCKET_ADDRESS),
            record = class_record(SOCKET_ADDRESS),
            ctor = symbol(SOCKET_ADDRESS, "<init>", SOCKET_ADDRESS_INIT),
        );
    } else {
        let _ = writeln!(
            out,
            "{new_isa} {{ (void)address; (void)port; jabs_not_compiled(\"InetSocketAddress(InetAddress, int) is not in the program (src/cgen/net.rs)\"); }}"
        );
    }
    let enhanced = "int32_t jabs_net_enhanced_host_info(void)";
    if unit.get(SECURITY_PROPERTIES).is_some()
        && inits.contains(SECURITY_PROPERTIES)
        && methods.contains(&(
            SECURITY_PROPERTIES.to_string(),
            INCLUDED.0.to_string(),
            INCLUDED.1.to_string(),
        ))
    {
        let _ = writeln!(
            out,
            "/* jdk.includeInExceptions has hostInfo or hostInfoExclSocket (see INCLUDED in\n * src/cgen/net.rs): 1, 0, or -1 with an exception pending. */\n{enhanced} {{\n    if ({init}() != 0) return -1;\n    static const char *const categories[2] = {{ \"hostInfo\", \"hostInfoExclSocket\" }};\n    for (int i = 0; i < 2; i++) {{\n        jabs_object *category = jabs_string_from_utf8(categories[i]);\n        if (!category) return -1;\n        int32_t on = {call}(category);\n        if (jabs_pending) return -1;\n        if (on) return 1;\n    }}\n    return 0;\n}}",
            init = init_symbol(SECURITY_PROPERTIES),
            call = symbol(SECURITY_PROPERTIES, INCLUDED.0, INCLUDED.1),
        );
    } else {
        let _ = writeln!(
            out,
            "{enhanced} {{ jabs_not_compiled(\"sun.security.util.SecurityProperties.includedInExceptions is not in the program (src/cgen/net.rs)\"); }}"
        );
    }
    out.push_str(&interface_glue(unit, methods, records, inits, holder6_ok));
    let prefer = "int32_t jabs_net_prefer_ipv4_stack(void)";
    if unit.get(BOOLEAN).is_some()
        && inits.contains(BOOLEAN)
        && methods.contains(&(
            BOOLEAN.to_string(),
            GET_BOOLEAN.0.to_string(),
            GET_BOOLEAN.1.to_string(),
        ))
    {
        let _ = writeln!(
            out,
            "/* Boolean.getBoolean(\"java.net.preferIPv4Stack\"): 1, 0, or -1 with an exception pending. */
{prefer} {{
    if ({init}() != 0) return -1;
    jabs_object *key = jabs_string_from_utf8(\"java.net.preferIPv4Stack\");
    if (!key) return -1;
    int32_t on = {call}(key);
    return jabs_pending ? -1 : on != 0;
}}",
            init = init_symbol(BOOLEAN),
            call = symbol(BOOLEAN, GET_BOOLEAN.0, GET_BOOLEAN.1),
        );
    } else {
        let _ = writeln!(
            out,
            "{prefer} {{ jabs_not_compiled(\"Boolean.getBoolean is not in the program (jabs_net_prefer_ipv4_stack, src/cgen/net.rs)\"); }}"
        );
    }
    let _ = writeln!(
        out,
        "/* Raise one of the JABS_NET_* exceptions with a UTF-8 message (NULL for none). */\nvoid jabs_net_raise(int32_t kind, const char *utf8) {{\n    jabs_object *message = NULL;\n    if (utf8) {{ message = jabs_string_from_utf8(utf8); if (!message) return; }}\n    switch (kind) {{"
    );
    for (class, kind) in RAISED {
        if has_method(class, "<init>", STRING_INIT) {
            let _ = writeln!(
                out,
                "    case {kind}: {{\n        if ({init}() != 0) return;\n        jabs_object *e = jabs_new(&{record});\n        if (!e) return;\n        {ctor}(e, message);\n        if (!jabs_pending) jabs_pending = e;\n        return;\n    }}",
                init = init_symbol(class),
                record = class_record(class),
                ctor = symbol(class, "<init>", STRING_INIT),
            );
        } else {
            let _ = writeln!(
                out,
                "    case {kind}: jabs_not_compiled(\"{class}(String) is not in the program (jabs_net_raise, src/cgen/net.rs)\");"
            );
        }
    }
    out.push_str("    default: jabs_not_compiled(\"jabs_net_raise with an unknown kind (src/cgen/net.rs)\");\n    }\n}\n");
    out
}

/// NetworkInterface and InterfaceAddress built as the JDK's natives build them: the no-arg
/// constructor, then the fields (createNetworkInterface), and children an empty array.
fn interface_glue(
    unit: &Unit,
    methods: &BTreeSet<MethodKey>,
    records: &BTreeSet<String>,
    inits: &BTreeSet<String>,
    holder6_ok: bool,
) -> String {
    let field = |class: &str, name: &str, descriptor: &str| {
        records.contains(class)
            && unit.get(class).is_some_and(|c| {
                c.fields.iter().any(|f| {
                    f.name == name && f.descriptor == descriptor && f.access & ACC_STATIC == 0
                })
            })
    };
    let constructible = |class: &str| {
        unit.get(class).is_some()
            && records.contains(class)
            && inits.contains(class)
            && methods.contains(&(class.to_string(), "<init>".to_string(), "()V".to_string()))
    };
    let at = |class: &str, object: &str, name: &str| {
        format!(
            "(({} *){object})->{}",
            object_struct(class),
            instance_field(name)
        )
    };
    let array = |class: &str, signature: &str| {
        if records.contains(class) {
            format!(
                "{signature} {{ return jabs_new_array(length, sizeof(jabs_object *), JABS_COMPONENT_CLASS, &{}, \"[L{class};\", NULL); }}\n",
                class_record(class)
            )
        } else {
            format!(
                "{signature} {{ (void)length; jabs_not_compiled(\"{class}[] ({class} is not in the program; src/cgen/net.rs)\"); }}\n"
            )
        }
    };
    let mut out = String::new();
    out.push_str(&array(
        NETWORK_INTERFACE,
        "jabs_object *jabs_net_new_interface_array(int32_t length)",
    ));
    out.push_str(&array(
        INTERFACE_ADDRESS,
        "jabs_object *jabs_net_new_interface_address_array(int32_t length)",
    ));
    let new_ni = "jabs_object *jabs_net_new_interface(jabs_object *name, jabs_object *display_name, int32_t index, jabs_object *addresses, jabs_object *bindings)";
    let ni_fields = [
        ("name", "Ljava/lang/String;"),
        ("displayName", "Ljava/lang/String;"),
        ("index", "I"),
        ("addrs", "[Ljava/net/InetAddress;"),
        ("bindings", "[Ljava/net/InterfaceAddress;"),
        ("childs", "[Ljava/net/NetworkInterface;"),
    ];
    if constructible(NETWORK_INTERFACE)
        && ni_fields
            .iter()
            .all(|(name, descriptor)| field(NETWORK_INTERFACE, name, descriptor))
    {
        let _ = writeln!(
            out,
            "{new_ni} {{\n    if ({init}() != 0) return NULL;\n    jabs_object *n = jabs_new(&{record});\n    if (!n) return NULL;\n    {ctor}(n);\n    if (jabs_pending) return NULL;\n    jabs_object *children = jabs_net_new_interface_array(0);\n    if (!children) return NULL;\n    {name} = name; {display} = display_name; {index} = index; {addrs} = addresses; {bindings} = bindings; {childs} = children;\n    return n;\n}}",
            init = init_symbol(NETWORK_INTERFACE),
            record = class_record(NETWORK_INTERFACE),
            ctor = symbol(NETWORK_INTERFACE, "<init>", "()V"),
            name = at(NETWORK_INTERFACE, "n", "name"),
            display = at(NETWORK_INTERFACE, "n", "displayName"),
            index = at(NETWORK_INTERFACE, "n", "index"),
            addrs = at(NETWORK_INTERFACE, "n", "addrs"),
            bindings = at(NETWORK_INTERFACE, "n", "bindings"),
            childs = at(NETWORK_INTERFACE, "n", "childs"),
        );
    } else {
        let _ = writeln!(
            out,
            "{new_ni} {{ (void)name; (void)display_name; (void)index; (void)addresses; (void)bindings; jabs_not_compiled(\"NetworkInterface construction (its class, constructor or fields are not in the program; src/cgen/net.rs)\"); }}"
        );
    }
    let new_ia = "jabs_object *jabs_net_new_interface_address(jabs_object *address, jabs_object *broadcast, int16_t prefix)";
    if constructible(INTERFACE_ADDRESS)
        && field(INTERFACE_ADDRESS, "address", "Ljava/net/InetAddress;")
        && field(INTERFACE_ADDRESS, "broadcast", "Ljava/net/Inet4Address;")
        && field(INTERFACE_ADDRESS, "maskLength", "S")
    {
        let _ = writeln!(
            out,
            "{new_ia} {{\n    if ({init}() != 0) return NULL;\n    jabs_object *b = jabs_new(&{record});\n    if (!b) return NULL;\n    {ctor}(b);\n    if (jabs_pending) return NULL;\n    {address} = address; {broadcast} = broadcast; {mask} = prefix;\n    return b;\n}}",
            init = init_symbol(INTERFACE_ADDRESS),
            record = class_record(INTERFACE_ADDRESS),
            ctor = symbol(INTERFACE_ADDRESS, "<init>", "()V"),
            address = at(INTERFACE_ADDRESS, "b", "address"),
            broadcast = at(INTERFACE_ADDRESS, "b", "broadcast"),
            mask = at(INTERFACE_ADDRESS, "b", "maskLength"),
        );
    } else {
        let _ = writeln!(
            out,
            "{new_ia} {{ (void)address; (void)broadcast; (void)prefix; jabs_not_compiled(\"InterfaceAddress construction (its class, constructor or fields are not in the program; src/cgen/net.rs)\"); }}"
        );
    }
    let scope = "void jabs_net_set_scope_interface(jabs_object *inet6, jabs_object *owner)";
    if holder6_ok
        && field(HOLDER6, "scope_ifname", "Ljava/net/NetworkInterface;")
        && field(HOLDER6, "scope_ifname_set", "Z")
    {
        let holder = at(INET6_ADDRESS, "inet6", "holder6");
        let _ = writeln!(
            out,
            "/* setInet6Address_scopeifname: the interface a scoped address belongs to. */\n{scope} {{ (({h} *){holder})->{name} = owner; (({h} *){holder})->{set} = 1; }}",
            h = object_struct(HOLDER6),
            name = instance_field("scope_ifname"),
            set = instance_field("scope_ifname_set"),
        );
    } else {
        let _ = writeln!(
            out,
            "{scope} {{ (void)inet6; (void)owner; jabs_not_compiled(\"Inet6Address holder6.scope_ifname is not in the program (src/cgen/net.rs)\"); }}"
        );
    }
    out
}
