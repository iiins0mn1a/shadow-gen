/*
 * The Shadow Simulator
 * Copyright (c) 2010-2011, Rob Jansen
 * See LICENSE for licensing information
 */

#include <glib.h>
#include <netinet/in.h>
#include <sys/un.h>

#include "main/core/definitions.h"
#include "main/core/worker.h"
#include "main/host/descriptor/descriptor.h"
#include "main/host/descriptor/socket.h"
#include "main/host/descriptor/tcp.h"
#include "main/network/legacypacket.h"
#include "main/utility/utility.h"

gsize _legacysocket_getOutputBufferSpaceIncludingTCP(LegacySocket* socket);

static gboolean _legacysocket_restore_output_trace_enabled(const Host* host, LegacySocket* socket) {
    const char* raw = getenv("SHADOW_RESTORE_OUTPUT_TRACE");
    if (raw == NULL || raw[0] == '\0' || strcmp(raw, "0") == 0) {
        return FALSE;
    }

    const char* host_name = host_getName(host);
    if (host_name == NULL) {
        host_name = "";
    }

    in_addr_t local_ip = 0;
    in_port_t local_port = 0;
    in_addr_t peer_ip = 0;
    in_port_t peer_port = 0;
    legacysocket_getSocketName(socket, &local_ip, &local_port);
    legacysocket_getPeerName(socket, &peer_ip, &peer_port);

    guint16 local_port_host = ntohs(local_port);
    guint16 peer_port_host = ntohs(peer_port);
    bool matched = false;
    bool host_ok = false;
    bool port_ok = false;

    gchar** parts = g_strsplit(raw, ",", -1);
    for (gsize i = 0; parts[i] != NULL; i++) {
        gchar* token = g_strstrip(parts[i]);
        if (token[0] == '\0') {
            continue;
        }
        if (strcmp(token, "1") == 0 || strcmp(token, "all") == 0) {
            matched = true;
            host_ok = true;
            port_ok = true;
            continue;
        }
        if (g_str_has_prefix(token, "host=")) {
            matched = true;
            if (strcmp(token + strlen("host="), host_name) == 0) {
                host_ok = true;
            }
            continue;
        }
        if (g_str_has_prefix(token, "port=")) {
            matched = true;
            guint64 port = g_ascii_strtoull(token + strlen("port="), NULL, 10);
            if (port > 0 && port <= G_MAXUINT16 &&
                (((guint16)port == local_port_host) || ((guint16)port == peer_port_host))) {
                port_ok = true;
            }
            continue;
        }
    }
    g_strfreev(parts);

    if (!matched) {
        return FALSE;
    }
    if (strstr(raw, "host=") != NULL && !host_ok) {
        return FALSE;
    }
    if (strstr(raw, "port=") != NULL && !port_ok) {
        return FALSE;
    }
    return TRUE;
}

static void _legacysocket_restore_output_trace(const Host* host, LegacySocket* socket,
                                               const char* action, Packet* packet_or_null,
                                               const char* buffer_kind) {
    if (!_legacysocket_restore_output_trace_enabled(host, socket)) {
        return;
    }

    in_addr_t local_ip = 0;
    in_port_t local_port = 0;
    in_addr_t peer_ip = 0;
    in_port_t peer_port = 0;
    legacysocket_getSocketName(socket, &local_ip, &local_port);
    legacysocket_getPeerName(socket, &peer_ip, &peer_port);

    char local_ip_buf[INET_ADDRSTRLEN] = {0};
    char peer_ip_buf[INET_ADDRSTRLEN] = {0};
    const char* local_ip_str = inet_ntop(AF_INET, &local_ip, local_ip_buf, sizeof(local_ip_buf));
    const char* peer_ip_str = inet_ntop(AF_INET, &peer_ip, peer_ip_buf, sizeof(peer_ip_buf));
    if (local_ip_str == NULL) {
        local_ip_str = "?";
    }
    if (peer_ip_str == NULL) {
        peer_ip_str = "?";
    }

    guint seq = 0;
    guint ack = 0;
    guint flags = 0;
    guint64 priority = 0;
    guint payload = 0;
    if (packet_or_null != NULL) {
        PacketTCPHeader header = packet_getTCPHeader(packet_or_null);
        seq = header.sequence;
        ack = header.acknowledgment;
        flags = header.flags;
        priority = packet_getPriority(packet_or_null);
        payload = packet_getPayloadSize(packet_or_null);
    }

    info("restore-output-trace host=%s sim_time_ns=%" G_GUINT64_FORMAT
         " action=%s local=%s:%u peer=%s:%u socket_ptr=%p buffer_kind=%s"
         " output_len=%" G_GSIZE_FORMAT " output_ctl_count=%" G_GSIZE_FORMAT
         " output_count=%" G_GSIZE_FORMAT
         " priority=%" G_GUINT64_FORMAT " seq=%u ack=%u flags=0x%x payload=%u",
         host_getName(host), worker_getCurrentSimulationTime(), action, local_ip_str,
         ntohs(local_port), peer_ip_str, ntohs(peer_port), socket,
         buffer_kind ? buffer_kind : "-", socket->outputBufferLength,
         (gsize)g_queue_get_length(socket->outputControlBuffer),
         (gsize)g_queue_get_length(socket->outputBuffer), priority, seq, ack, flags, payload);
}

static LegacySocket* _legacysocket_fromLegacyFile(LegacyFile* descriptor) {
    utility_debugAssert(legacyfile_getType(descriptor) == DT_TCPSOCKET);
    return (LegacySocket*)descriptor;
}

static void _legacysocket_cleanup(LegacyFile* descriptor) {
    LegacySocket* socket = _legacysocket_fromLegacyFile(descriptor);
    MAGIC_ASSERT(socket);
    MAGIC_ASSERT(socket->vtable);

    if (socket->vtable->cleanup) {
        socket->vtable->cleanup(descriptor);
    }
}

static void _legacysocket_free(LegacyFile* descriptor) {
    LegacySocket* socket = _legacysocket_fromLegacyFile(descriptor);
    MAGIC_ASSERT(socket);
    MAGIC_ASSERT(socket->vtable);


    if(socket->peerString) {
        g_free(socket->peerString);
    }
    if(socket->boundString) {
        g_free(socket->boundString);
    }
    if(socket->unixPath) {
        g_free(socket->unixPath);
    }

    while(g_queue_get_length(socket->inputBuffer) > 0) {
        packet_unref(g_queue_pop_head(socket->inputBuffer));
    }
    g_queue_free(socket->inputBuffer);

    while(g_queue_get_length(socket->outputBuffer) > 0) {
        packet_unref(g_queue_pop_head(socket->outputBuffer));
    }
    g_queue_free(socket->outputBuffer);

    while(g_queue_get_length(socket->outputControlBuffer) > 0) {
        packet_unref(g_queue_pop_head(socket->outputControlBuffer));
    }
    g_queue_free(socket->outputControlBuffer);

    // TODO: assertion errors will occur if the subclass uses the socket
    // during the free call. This could be fixed by making all descriptor types
    // a direct child of the descriptor class.
    MAGIC_CLEAR(socket);
    socket->vtable->free((LegacyFile*)socket);
}

static void _legacysocket_close(LegacyFile* descriptor, const Host* host) {
    LegacySocket* socket = _legacysocket_fromLegacyFile(descriptor);
    MAGIC_ASSERT(socket);
    MAGIC_ASSERT(socket->vtable);
    socket->vtable->close((LegacyFile*)socket, host);
}

gssize legacysocket_sendUserData(LegacySocket* socket, const Thread* thread,
                                 UntypedForeignPtr buffer, gsize nBytes, in_addr_t ip,
                                 in_port_t port) {
    MAGIC_ASSERT(socket);
    MAGIC_ASSERT(socket->vtable);
    return socket->vtable->send(socket, thread, buffer, nBytes, ip, port);
}

gssize legacysocket_receiveUserData(LegacySocket* socket, const Thread* thread,
                                    UntypedForeignPtr buffer, gsize nBytes, in_addr_t* ip,
                                    in_port_t* port) {
    MAGIC_ASSERT(socket);
    MAGIC_ASSERT(socket->vtable);
    return socket->vtable->receive(socket, thread, buffer, nBytes, ip, port);
}

LegacyFileFunctionTable socket_functions = {
    _legacysocket_close, _legacysocket_cleanup, _legacysocket_free, MAGIC_VALUE};

void legacysocket_init(LegacySocket* socket, const Host* host, SocketFunctionTable* vtable,
                       LegacyFileType type, guint receiveBufferSize, guint sendBufferSize) {
    utility_debugAssert(socket && vtable);

    legacyfile_init(&(socket->super), type, &socket_functions);

    MAGIC_INIT(socket);
    MAGIC_INIT(vtable);

    socket->vtable = vtable;

    utility_debugAssert(type == DT_TCPSOCKET);
    socket->protocol = type == PTCP;
    socket->inputBuffer = g_queue_new();
    socket->inputBufferSize = receiveBufferSize;
    socket->outputBuffer = g_queue_new();
    socket->outputControlBuffer = g_queue_new();
    socket->outputBufferSize = sendBufferSize;
}

ProtocolType legacysocket_getProtocol(LegacySocket* socket) {
    MAGIC_ASSERT(socket);
    return socket->protocol;
}

/* interface functions, implemented by subtypes */

gboolean legacysocket_isFamilySupported(LegacySocket* socket, sa_family_t family) {
    MAGIC_ASSERT(socket);
    MAGIC_ASSERT(socket->vtable);
    return socket->vtable->isFamilySupported(socket, family);
}

gint legacysocket_connectToPeer(LegacySocket* socket, const Host* host, in_addr_t ip,
                                in_port_t port, sa_family_t family) {
    MAGIC_ASSERT(socket);
    MAGIC_ASSERT(socket->vtable);
    return socket->vtable->connectToPeer(socket, host, ip, port, family);
}

// This function takes an owned packet from the Rust caller, and drops the packet upon return.
void legacysocket_pushInPacket(LegacySocket* socket, const Host* host, Packet* packet) {
    MAGIC_ASSERT(socket);
    MAGIC_ASSERT(socket->vtable);
    packet_addDeliveryStatus(packet, PDS_RCV_SOCKET_PROCESSED);
    // The TCP code will only borrow our ref to `Packet` in the process function, but we still own
    // it. If the TCP layer wants to hold another ref to the packet, it should call `packet_ref()`.
    socket->vtable->process(socket, host, packet);
    packet_unref(packet);
}

void legacysocket_dropPacket(LegacySocket* socket, const Host* host, Packet* packet) {
    MAGIC_ASSERT(socket);
    MAGIC_ASSERT(socket->vtable);
    socket->vtable->dropPacket(socket, host, packet);
}

/* functions implemented by socket */

// Returns an owned packet to the Rust caller, or NULL if no packet is available.
Packet* legacysocket_pullOutPacket(LegacySocket* socket, const Host* host) {
    Packet* packet = legacysocket_removeFromOutputBuffer(socket, host);

    /* if packet was NULL, then the buffer shouldn't have changed so we can skip the check */
    if (packet != NULL) {
        /* we are writable if we now have space */
        gsize space = _legacysocket_getOutputBufferSpaceIncludingTCP(socket);
        gboolean is_active = legacyfile_getStatus((LegacyFile*)socket) & FileState_ACTIVE;
        if (space > 0 && is_active) {
            legacyfile_adjustStatus((LegacyFile*)socket, FileState_WRITABLE, TRUE, 0);
        }
    }

    return packet;
}

// Returns an owned packet to the Rust caller, or NULL if no packet is available.
Packet* legacysocket_peekNextOutPacket(const LegacySocket* socket) {
    MAGIC_ASSERT(socket);
    Packet* packet = NULL;
    if(!g_queue_is_empty(socket->outputControlBuffer)) {
        packet = g_queue_peek_head(socket->outputControlBuffer);
    } else {
        packet = g_queue_peek_head(socket->outputBuffer);
    }
    // Increment the ref count since we return an owned packet to the Rust caller.
    if (packet != NULL) {
        packet_ref(packet);
    }
    return packet;
}

Packet* legacysocket_peekNextInPacket(const LegacySocket* socket) {
    MAGIC_ASSERT(socket);
    return g_queue_peek_head(socket->inputBuffer);
}

gsize legacysocket_getInputBufferPacketCount(LegacySocket* socket) {
    MAGIC_ASSERT(socket);
    return g_queue_get_length(socket->inputBuffer);
}

Packet* legacysocket_getInputBufferPacketAt(LegacySocket* socket, gsize index) {
    MAGIC_ASSERT(socket);
    Packet* packet = g_queue_peek_nth(socket->inputBuffer, index);
    if (packet != NULL) {
        packet_ref(packet);
    }
    return packet;
}

gsize legacysocket_getOutputBufferPacketCount(LegacySocket* socket, gboolean control) {
    MAGIC_ASSERT(socket);
    GQueue* queue = control ? socket->outputControlBuffer : socket->outputBuffer;
    return g_queue_get_length(queue);
}

Packet* legacysocket_getOutputBufferPacketAt(LegacySocket* socket, gboolean control, gsize index) {
    MAGIC_ASSERT(socket);
    GQueue* queue = control ? socket->outputControlBuffer : socket->outputBuffer;
    Packet* packet = g_queue_peek_nth(queue, index);
    if (packet != NULL) {
        packet_ref(packet);
    }
    return packet;
}

gboolean legacysocket_getPeerName(LegacySocket* socket, in_addr_t* ip, in_port_t* port) {
    MAGIC_ASSERT(socket);

    if(socket->peerIP == 0 || socket->peerPort == 0) {
        return FALSE;
    }

    if(ip) {
        *ip = socket->peerIP;
    }
    if(port) {
        *port = socket->peerPort;
    }

    return TRUE;
}

void legacysocket_setPeerName(LegacySocket* socket, in_addr_t ip, in_port_t port) {
    MAGIC_ASSERT(socket);

    socket->peerIP = ip;
    socket->peerPort = port;

    /* store the new ascii name of this peer */
    if(socket->peerString) {
        g_free(socket->peerString);
    }
    gchar* ipString = util_ipToNewString(ip);
    GString* stringBuffer = g_string_new(ipString);
    g_free(ipString);
    g_string_append_printf(stringBuffer, ":%u", ntohs(port));
    socket->peerString = g_string_free(stringBuffer, FALSE);
}

gboolean legacysocket_getSocketName(LegacySocket* socket, in_addr_t* ip, in_port_t* port) {
    MAGIC_ASSERT(socket);

    /* boundAddress could be 0 (INADDR_NONE), so just check port */
    if(!legacysocket_isBound(socket)) {
        return FALSE;
    }

    if(ip) {
        if(socket->boundAddress == htonl(INADDR_ANY) &&
                socket->peerIP && socket->peerIP == htonl(INADDR_LOOPBACK)) {
            *ip = htonl(INADDR_LOOPBACK);
        } else {
            *ip = socket->boundAddress;
        }
    }
    if(port) {
        *port = socket->boundPort;
    }

    return TRUE;
}

void legacysocket_setSocketName(LegacySocket* socket, in_addr_t ip, in_port_t port) {
    MAGIC_ASSERT(socket);

    socket->boundAddress = ip;
    socket->boundPort = port;

    /* store the new ascii name of this socket endpoint */
    if(socket->boundString) {
        g_free(socket->boundString);
    }

    gchar* ipString = util_ipToNewString(ip);
    GString* stringBuffer = g_string_new(ipString);
    g_free(ipString);
    g_string_append_printf(stringBuffer, ":%u (descriptor %p)", ntohs(port), &socket->super);
    socket->boundString = g_string_free(stringBuffer, FALSE);

    /* the socket is now bound */
    socket->flags |= SF_BOUND;
}

gboolean legacysocket_isBound(LegacySocket* socket) {
    MAGIC_ASSERT(socket);
    return (socket->flags & SF_BOUND) ? TRUE : FALSE;
}

gsize legacysocket_getInputBufferSpace(LegacySocket* socket) {
    MAGIC_ASSERT(socket);
    utility_debugAssert(socket->inputBufferSize >= socket->inputBufferLength);
    gsize bufferSize = legacysocket_getInputBufferSize(socket);
    if(bufferSize < socket->inputBufferLength) {
        return 0;
    } else {
        return bufferSize - socket->inputBufferLength;
    }
}

gsize legacysocket_getOutputBufferSpace(LegacySocket* socket) {
    MAGIC_ASSERT(socket);
    utility_debugAssert(socket->outputBufferSize >= socket->outputBufferLength);
    gsize bufferSize = legacysocket_getOutputBufferSize(socket);
    if(bufferSize < socket->outputBufferLength) {
        return 0;
    } else {
        return bufferSize - socket->outputBufferLength;
    }
}

gsize legacysocket_getInputBufferLength(LegacySocket* socket) {
    MAGIC_ASSERT(socket);
    return socket->inputBufferLength;
}

gsize legacysocket_getOutputBufferLength(LegacySocket* socket) {
    MAGIC_ASSERT(socket);
    return socket->outputBufferLength;
}

gsize legacysocket_getInputBufferSize(LegacySocket* socket) {
    MAGIC_ASSERT(socket);
    return socket->inputBufferSizePending > 0 ? socket->inputBufferSizePending : socket->inputBufferSize;
}

gsize legacysocket_getOutputBufferSize(LegacySocket* socket) {
    MAGIC_ASSERT(socket);
    return socket->outputBufferSizePending > 0 ? socket->outputBufferSizePending : socket->outputBufferSize;
}

void legacysocket_setInputBufferSize(LegacySocket* socket, gsize newSize) {
    MAGIC_ASSERT(socket);
    if(newSize >= socket->inputBufferLength) {
        socket->inputBufferSize = newSize;
        socket->inputBufferSizePending = 0;
    } else {
        /* ensure positive size, reduce size as buffer drains */
        socket->inputBufferSize = socket->inputBufferLength;
        socket->inputBufferSizePending = newSize;
    }
}

void legacysocket_setOutputBufferSize(LegacySocket* socket, gsize newSize) {
    MAGIC_ASSERT(socket);
    if(newSize >= socket->outputBufferLength) {
        socket->outputBufferSize = newSize;
        socket->outputBufferSizePending = 0;
    } else {
        /* ensure positive size, reduce size as buffer drains */
        socket->outputBufferSize = socket->outputBufferLength;
        socket->outputBufferSizePending = newSize;
    }
}

gboolean legacysocket_addToInputBuffer(LegacySocket* socket, const Host* host, Packet* packet) {
    MAGIC_ASSERT(socket);

    /* check if the packet fits */
    gsize length = packet_getPayloadSize(packet);
    if(length > legacysocket_getInputBufferSpace(socket)) {
        return FALSE;
    }

    /* add to our queue */
    g_queue_push_tail(socket->inputBuffer, packet);
    packet_ref(packet);
    socket->inputBufferLength += length;
    packet_addDeliveryStatus(packet, PDS_RCV_SOCKET_BUFFERED);

    return TRUE;
}

Packet* legacysocket_removeFromInputBuffer(LegacySocket* socket, const Host* host) {
    MAGIC_ASSERT(socket);

    /* see if we have any packets */
    Packet* packet = g_queue_pop_head(socket->inputBuffer);
    if(packet) {
        /* just removed a packet */
        gsize length = packet_getPayloadSize(packet);
        socket->inputBufferLength -= length;

        /* check if we need to reduce the buffer size */
        if(socket->inputBufferSizePending > 0) {
            legacysocket_setInputBufferSize(socket, socket->inputBufferSizePending);
        }
    }

    return packet;
}

gsize _legacysocket_getOutputBufferSpaceIncludingTCP(LegacySocket* socket) {
    /* get the space in the socket layer */
    gsize space = legacysocket_getOutputBufferSpace(socket);

    /* internal TCP buffers count against our space */
    gsize tcpLength = socket->protocol == PTCP ? tcp_getOutputBufferLength((TCP*)socket) : 0;

    /* subtract tcpLength without underflowing space */
    space = (tcpLength < space) ? (space - tcpLength) : 0;

    return space;
}

// takes ownership of `inetSocket` (will free/drop)
gboolean legacysocket_addToOutputBuffer(LegacySocket* socket, InetSocket* inetSocket,
                                        const Host* host, Packet* packet) {
    MAGIC_ASSERT(socket);

    /* check if the packet fits */
    gsize length = packet_getPayloadSize(packet);
    if(length > legacysocket_getOutputBufferSpace(socket)) {
        return FALSE;
    }

    /* add to our queue */
    if(packet_getPriority(packet) == 0) {
        /* control packets get sent first */
        g_queue_push_tail(socket->outputControlBuffer, packet);
        _legacysocket_restore_output_trace(host, socket, "add_output", packet, "control");
    } else {
        g_queue_push_tail(socket->outputBuffer, packet);
        _legacysocket_restore_output_trace(host, socket, "add_output", packet, "data");
    }

    socket->outputBufferLength += length;
    packet_addDeliveryStatus(packet, PDS_SND_SOCKET_BUFFERED);

    /* tell the interface to include us when sending out to the network */
    in_addr_t ip = packet_getSourceIP(packet);
    socket_wants_to_send_with_global_cb_queue(host, inetSocket, ip);

    return TRUE;
}

Packet* legacysocket_removeFromOutputBuffer(LegacySocket* socket, const Host* host) {
    MAGIC_ASSERT(socket);

    /* see if we have any packets */
    Packet* packet = !g_queue_is_empty(socket->outputControlBuffer) ?
            g_queue_pop_head(socket->outputControlBuffer) : g_queue_pop_head(socket->outputBuffer);

    if(packet) {
        _legacysocket_restore_output_trace(
            host,
            socket,
            "remove_output",
            packet,
            packet_getPriority(packet) == 0 ? "control" : "data");
        /* just removed a packet */
        gsize length = packet_getPayloadSize(packet);
        socket->outputBufferLength -= length;

        /* check if we need to reduce the buffer size */
        if(socket->outputBufferSizePending > 0) {
            legacysocket_setOutputBufferSize(socket, socket->outputBufferSizePending);
        }
    }

    return packet;
}

gboolean legacysocket_isUnix(LegacySocket* socket) {
    return (socket->flags & SF_UNIX) ? TRUE : FALSE;
}

void legacysocket_setUnix(LegacySocket* socket, gboolean isUnixSocket) {
    MAGIC_ASSERT(socket);
    socket->flags = isUnixSocket ? (socket->flags | SF_UNIX) : (socket->flags & ~SF_UNIX);
}

void legacysocket_setUnixPath(LegacySocket* socket, const gchar* path, gboolean isBound) {
    MAGIC_ASSERT(socket);
    if(isBound) {
        socket->flags |= SF_UNIX_BOUND;
    }
    socket->unixPath = g_strdup(path);
}

gchar* legacysocket_getUnixPath(LegacySocket* socket) {
    MAGIC_ASSERT(socket);
    return socket->unixPath;
}
