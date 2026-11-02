#ifndef SAFE_MEDIA_RSS_BOUND_H
#define SAFE_MEDIA_RSS_BOUND_H
#include <linux/types.h>
#include <linux/ioctl.h>
struct safe_media_rss_bound {
    __u64 abi, initial_bytes, positive_pages, upper_bytes;
    __u64 events, missed, errors, page_size;
};
#define SAFE_MEDIA_RSS_END _IOR('J', 0x71, struct safe_media_rss_bound)
#endif
