#ifndef TRIERGON_RSS_BOUND_H
#define TRIERGON_RSS_BOUND_H
#include <linux/types.h>
#include <linux/ioctl.h>
struct triergon_rss_bound {
    __u64 abi, initial_bytes, positive_pages, upper_bytes;
    __u64 events, missed, errors, page_size;
};
#define TRIERGON_RSS_END _IOR('J', 0x71, struct triergon_rss_bound)
#endif
