#ifndef SHD_TCP_CONG_RENO_H_
#define SHD_TCP_CONG_RENO_H_

#include "main/host/descriptor/tcp.h"
#include "main/host/descriptor/tcp_cong.h"

// the name linux gives for this congestion control algorithm
extern const char* TCP_CONG_RENO_NAME;

void tcp_cong_reno_init(TCP *tcp);
guint32 tcp_cong_reno_get_ssthresh(TCP *tcp);
gsize tcp_cong_reno_get_duplicate_ack_count(TCP *tcp);
guint32 tcp_cong_reno_get_cong_avoid_nacked(TCP *tcp);
LegacyTcpCongestionState tcp_cong_reno_get_state(TCP *tcp);
void tcp_cong_reno_restore_state(TCP *tcp, guint32 cwnd, guint32 ssthresh,
                                 gsize duplicate_ack_n, guint32 cong_avoid_nacked,
                                 LegacyTcpCongestionState state);

#endif // SHD_TCP_CONG_RENO_H_
