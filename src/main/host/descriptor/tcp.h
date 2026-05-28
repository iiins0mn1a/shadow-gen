/*
 * The Shadow Simulator
 * Copyright (c) 2010-2011, Rob Jansen
 * See LICENSE for licensing information
 */

#ifndef SHD_TCP_H_
#define SHD_TCP_H_

#include <glib.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/un.h>

#include "main/bindings/c/bindings-opaque.h"
#include "main/core/definitions.h"
#include "main/network/legacypacket.h"

#define TCP_MIN_CWND 10

typedef struct _TCP TCP;
struct TCPCong_;

/* these were redefined in shd-tcp-retransmit-tally.h
 * if they change here, they must also change there!! (-RSW)
 */
typedef enum TCPProcessFlags TCPProcessFlags;
enum TCPProcessFlags {
    TCP_PF_NONE = 0,
    TCP_PF_PROCESSED = 1 << 0,
    TCP_PF_DATA_RECEIVED = 1 << 1,
    TCP_PF_DATA_ACKED = 1 << 2,
    TCP_PF_DATA_SACKED = 1 << 3,
    TCP_PF_DATA_LOST = 1 << 4,
    TCP_PF_RWND_UPDATED = 1 << 5,
};

typedef enum _TCPCongestionType TCPCongestionType;
enum _TCPCongestionType {
    TCP_CC_UNKNOWN, TCP_CC_AIMD, TCP_CC_RENO, TCP_CC_CUBIC,
};

typedef enum _LegacyTcpCongestionState LegacyTcpCongestionState;
enum _LegacyTcpCongestionState {
    TCP_CONG_STATE_UNKNOWN = 0,
    TCP_CONG_STATE_SLOW_START = 1,
    TCP_CONG_STATE_CONG_AVOID = 2,
    TCP_CONG_STATE_FAST_RECOVERY = 3,
};

TCP* tcp_new(const Host* host, guint receiveBufferSize, guint sendBufferSize);

void tcp_setRustSocket(TCP* tcp, InetSocketWeak* rustSocket);

// clang-format off
/* Returns a positive number to indicate that we have not yet sent a SYN
 * packet, i.e., connect() has not been called.
 *
 * Returns 0 to signal that a previous connect() attempt succeeded. A 0 return
 * code is only returned once, after which it is assumed that the successful
 * connect() has been signaled to the user.
 *
 * Otherwise returns a negative code:
 * -ECONNRESET: an established connection failed unexpectedly
 * -ENOTCONN: the connection was established, but now both reading and writing
 *            are done
 * -EISCONN: the connection is established and we already returned 0 once to
 *           indicate a successful 3-way handshake
 * -ECONNREFUSED: the 3-way handshake failed
 * -EALREADY: connect() was called and we are waiting for the 3-way handshake
 */
gint tcp_getConnectionError(TCP* tcp);
// clang-format on

typedef struct _LegacyTcpRestoreState LegacyTcpRestoreState;
struct _LegacyTcpRestoreState {
    guint state;
    guint flags;
    guint error;
    gboolean is_server;
    guint server_pending_max;
    guint server_pending_count;
    pid_t server_process_for_children;
    in_addr_t server_last_peer_ip;
    in_port_t server_last_peer_port;
    in_addr_t server_last_ip;
    guint32 recv_start;
    guint32 recv_next;
    guint32 recv_window;
    guint32 recv_end;
    guint32 recv_last_window;
    guint32 recv_last_ack;
    guint32 recv_last_seq;
    guint32 send_unacked;
    guint32 send_next;
    guint32 send_window;
    guint32 send_end;
    guint32 send_last_ack;
    guint32 send_last_window;
    guint32 send_highest_seq;
};

typedef struct _LegacyTcpRangeSnapshot LegacyTcpRangeSnapshot;
struct _LegacyTcpRangeSnapshot {
    guint32 begin;
    guint32 end;
};

void tcp_getInfo(TCP* tcp, struct tcp_info *tcpinfo);
void tcp_enterServerMode(TCP* tcp, const Host* host, pid_t process, gint backlog);
void tcp_updateServerBacklog(TCP* tcp, gint backlog);
void tcp_getRestoreState(TCP* tcp, LegacyTcpRestoreState* out);
void tcp_restoreListenerState(TCP* tcp, const Host* host, const LegacyTcpRestoreState* state);
void tcp_restoreEstablishedState(TCP* tcp, const Host* host, const LegacyTcpRestoreState* state);
gsize tcp_getUnorderedInputPacketCount(TCP* tcp);
Packet* tcp_getUnorderedInputPacketAt(TCP* tcp, gsize index);
gsize tcp_getThrottledOutputPacketCount(TCP* tcp);
Packet* tcp_getThrottledOutputPacketAt(TCP* tcp, gsize index);
gsize tcp_getRetransmitQueuePacketCount(TCP* tcp);
Packet* tcp_getRetransmitQueuePacketAt(TCP* tcp, gsize index);
gint tcp_getRetransmitTimeout(TCP* tcp);
guint tcp_getRetransmitBackoffCount(TCP* tcp);
CSimulationTime tcp_getRetransmitDesiredTimerExpiration(TCP* tcp);
gsize tcp_getRetransmitScheduledExpirationCount(TCP* tcp);
CSimulationTime tcp_getRetransmitScheduledExpirationAt(TCP* tcp, gsize index);
gboolean tcp_getDelayedAckIsScheduled(TCP* tcp);
guint tcp_getDelayedAckCounter(TCP* tcp);
guint tcp_getNumQuickACKsSent(TCP* tcp);
gboolean tcp_getWindowUpdatePending(TCP* tcp);
gint tcp_getTimingRttSmoothed(TCP* tcp);
gint tcp_getTimingRttVariance(TCP* tcp);
guint32 tcp_getCongestionWindow(TCP* tcp);
guint32 tcp_getCongestionSsthresh(TCP* tcp);
gsize tcp_getCongestionDuplicateAckCount(TCP* tcp);
guint32 tcp_getCongestionAvoidNacked(TCP* tcp);
LegacyTcpCongestionState tcp_getCongestionState(TCP* tcp);
gint64 tcp_getRetransmitTallyLastAck(TCP* tcp);
gsize tcp_getRetransmitTallyNumDupAcks(TCP* tcp);
gsize tcp_getRetransmitTallyMarkedLostCount(TCP* tcp);
LegacyTcpRangeSnapshot tcp_getRetransmitTallyMarkedLostRange(TCP* tcp, gsize index);
gsize tcp_getRetransmitTallySackedCount(TCP* tcp);
LegacyTcpRangeSnapshot tcp_getRetransmitTallySackedRange(TCP* tcp, gsize index);
gsize tcp_getRetransmitTallyRetransmittedCount(TCP* tcp);
LegacyTcpRangeSnapshot tcp_getRetransmitTallyRetransmittedRange(TCP* tcp, gsize index);
Packet* tcp_getPartialUserDataPacket(TCP* tcp);
guint tcp_getPartialOffset(TCP* tcp);
void tcp_restoreInputBufferPacket(TCP* tcp, const Host* host, Packet* packet);
void tcp_restoreOutputBufferPacket(TCP* tcp, const Host* host, Packet* packet);
void tcp_restoreThrottledOutputPacket(TCP* tcp, Packet* packet);
void tcp_restoreRetransmitQueuePacket(TCP* tcp, Packet* packet);
void tcp_restoreRetransmitState(TCP* tcp, gint timeout, CSimulationTime desired_expiration,
                                guint backoff_count, gint rtt_smoothed, gint rtt_variance,
                                gboolean delayed_ack_is_scheduled, guint delayed_ack_counter,
                                guint num_quick_acks_sent, gboolean window_update_pending);
void tcp_restoreCongestionState(TCP* tcp, guint32 cwnd, guint32 ssthresh,
                                gsize duplicate_ack_n, guint32 cong_avoid_nacked,
                                LegacyTcpCongestionState state);
void tcp_restoreRetransmitScheduledExpiration(TCP* tcp, CSimulationTime expiration);
void tcp_restoreRetransmitTally(TCP* tcp, gint64 last_ack, gsize num_dup_acks,
                                const LegacyTcpRangeSnapshot* marked_lost,
                                gsize marked_lost_len,
                                const LegacyTcpRangeSnapshot* sacked,
                                gsize sacked_len,
                                const LegacyTcpRangeSnapshot* retransmitted,
                                gsize retransmitted_len);
void tcp_restoreUnorderedInputPacket(TCP* tcp, Packet* packet);
void tcp_restorePartialUserDataPacket(TCP* tcp, Packet* packet, guint offset);
void tcp_refreshReadableInputState(TCP* tcp);
void tcp_runCloseTimerExpiredTask(TCP* tcp, const Host* host);
void tcp_runRetransmitTimerExpiredTask(TCP* tcp, const Host* host);
void tcp_sendACKTask(TCP* tcp, const Host* host);
void tcp_sendWindowUpdateTask(TCP* tcp, const Host* host);
/* Address and port must be in network byte order. */
gint tcp_acceptServerPeer(TCP* tcp, const Host* host, in_addr_t* ip, in_port_t* port,
                          gint* acceptedHandle);

struct TCPCong_ *tcp_cong(TCP *tcp);

void tcp_clearAllChildrenIfServer(TCP* tcp);

gsize tcp_getOutputBufferLength(TCP* tcp);
gsize tcp_getInputBufferLength(TCP* tcp);
gsize tcp_getNotSentBytes(TCP* tcp);

void tcp_disableSendBufferAutotuning(TCP* tcp);
void tcp_disableReceiveBufferAutotuning(TCP* tcp);

gboolean tcp_isValidListener(TCP* tcp);
gboolean tcp_isListeningAllowed(TCP* tcp);

gssize tcp_sendUserData(TCP* tcp, const Host* host, UntypedForeignPtr buffer, gsize nBytes,
                        in_addr_t ip, in_port_t port, const MemoryManager* mem);
gssize tcp_receiveUserData(TCP* tcp, const Host* host, UntypedForeignPtr buffer, gsize nBytes,
                           in_addr_t* ip, in_port_t* port, MemoryManager* mem);

gint tcp_shutdown(TCP* tcp, const Host* host, gint how);

void tcp_networkInterfaceIsAboutToSendPacket(TCP* tcp, const Host* host, Packet* packet);

TCPCongestionType tcpCongestion_getType(const gchar* type);

#endif /* SHD_TCP_H_ */
