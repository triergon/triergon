// SPDX-License-Identifier: GPL-2.0-only
/* External measurement experiment only. Never a production Rust dependency.
 * Exact Ubuntu7.0 headers/ABI; no kernel implementation copied.
 * Bound: initial summed RSS + every positive counter update; no releases deducted.
 * OPEN precedes main/input loading. Only one no-THP, single-thread MM is admitted.
 * All probes must stay armed, source coverage must be reviewed and nmissed zero.
 */
#include <linux/module.h>
#include <linux/miscdevice.h>
#include <linux/fs.h>
#include <linux/mm.h>
#include <linux/sched/mm.h>
#include <linux/sched/signal.h>
#include <linux/kprobes.h>
#include <linux/ptrace.h>
#include <linux/stop_machine.h>
#include <linux/slab.h>
#include <linux/mutex.h>
#include <linux/overflow.h>
#include <linux/uaccess.h>

#include "rss_bound.h"

struct session {
    struct mm_struct *mm;
    pid_t owner;
    u64 initial, missed[5];
    atomic64_t positive, events;
    atomic_t errors;
    bool running;
    struct safe_media_rss_bound result;
};
static DEFINE_MUTEX(session_lock);
static bool session_busy;
static struct session *watch;
static struct kprobe probes[5];
static struct kprobe *probe_ptrs[5];
static unsigned int probe_count;
static struct kprobe *iterator_probe;
static struct file *control_file;
static atomic_t global_control_written = ATOMIC_INIT(0);

/* Hold the actual global-control file and hook ALL of its filesystem write
 * entry points, rather than relying on a private/inlined setter symbol. Inode
 * identity also covers already-open descriptors and alternate path spellings. */
static int global_control_write(struct kprobe *probe, struct pt_regs *regs)
{
    void *argument = (void *)regs_get_kernel_argument(regs, 0);
    struct file *file = probe == iterator_probe ?
        ((struct kiocb *)argument)->ki_filp : argument;
    if (file_inode(file) == file_inode(control_file))
        atomic_set(&global_control_written, 1);
    return 0;
}

static int count_positive(struct kprobe *probe, struct pt_regs *regs)
{
    struct session *s = READ_ONCE(watch);
    struct percpu_counter *counter;
    s64 amount, total;
    if (!s) return 0;
    counter = (void *)regs_get_kernel_argument(regs, 0);
    if (counter != &s->mm->rss_stat[MM_FILEPAGES] &&
        counter != &s->mm->rss_stat[MM_ANONPAGES] &&
        counter != &s->mm->rss_stat[MM_SHMEMPAGES]) return 0;
    /* External MM updates invalidate admission, even when they release pages. */
    if (current->pid != s->owner || current->mm != s->mm)
        atomic_or(1, &s->errors);
    amount = (s64)regs_get_kernel_argument(regs, probe == &probes[2] ? 2 : 1);
    atomic64_inc(&s->events);
    if (amount > 0) {
        total = atomic64_add_return(amount, &s->positive);
        if (total < 0 || total < amount) atomic_or(2, &s->errors);
    }
    return 0;
}
static int start_stopped(void *data)
{
    struct session *s = data;
    unsigned long pages = get_mm_rss_sum(s->mm);
    unsigned int n;
    if (atomic_read(&global_control_written)) return -EACCES;
    if (check_mul_overflow((u64)pages, (u64)PAGE_SIZE, &s->initial)) return -EOVERFLOW;
    for (n = 0; n < probe_count; ++n) {
        if (n >= 3 && probes[n].nmissed) return -EACCES;
        s->missed[n] = probes[n].nmissed;
    }
    atomic64_set(&s->positive, 0);
    atomic64_set(&s->events, 0);
    atomic_set(&s->errors, 0);
    s->running = true;
    WRITE_ONCE(watch, s);
    return 0;
}
static int end_stopped(void *data)
{
    struct session *s = data;
    u64 positive, bytes, upper, missed = 0;
    unsigned int n;
    WRITE_ONCE(watch, NULL);
    s->running = false;
    positive = (u64)atomic64_read(&s->positive);
    for (n = 0; n < probe_count; ++n) {
        if (probes[n].nmissed < s->missed[n] || !probes[n].addr ||
            kprobe_disabled(&probes[n])) atomic_or(4, &s->errors);
        missed += probes[n].nmissed - s->missed[n];
    }
    for (n = 3; n < probe_count; ++n)
        if (probes[n].nmissed) atomic_or(4, &s->errors);
    if (missed) atomic_or(4, &s->errors);
    if (atomic_read(&global_control_written)) atomic_or(8, &s->errors);
    if (check_mul_overflow(positive, (u64)PAGE_SIZE, &bytes) ||
        check_add_overflow(s->initial, bytes, &upper)) {
        atomic_or(2, &s->errors); upper = 0;
    }
    s->result = (struct safe_media_rss_bound) {
        .abi = 1, .initial_bytes = s->initial, .positive_pages = positive,
        .upper_bytes = upper, .events = (u64)atomic64_read(&s->events),
        .missed = missed, .errors = atomic_read(&s->errors), .page_size = PAGE_SIZE
    };
    return 0;
}
static int bound_open(struct inode *inode, struct file *file)
{
    struct session *s;
    int result;
    (void)inode;
    mutex_lock(&session_lock);
    if (session_busy) { mutex_unlock(&session_lock); return -EBUSY; }
    session_busy = true;
    mutex_unlock(&session_lock);
    if (!current->mm || get_nr_threads(current) != 1 ||
        !mm_flags_test(MMF_DISABLE_THP_COMPLETELY, current->mm)) {
        mutex_lock(&session_lock); session_busy = false; mutex_unlock(&session_lock); return -EPERM;
    }
    s = kzalloc(sizeof(*s), GFP_KERNEL);
    if (!s) { mutex_lock(&session_lock); session_busy = false; mutex_unlock(&session_lock); return -ENOMEM; }
    s->mm = get_task_mm(current);
    if (!s->mm) { kfree(s); mutex_lock(&session_lock); session_busy = false; mutex_unlock(&session_lock); return -ESRCH; }
    s->owner = current->pid;
    result = stop_machine(start_stopped, s, NULL);
    if (result) { mmput(s->mm); kfree(s); mutex_lock(&session_lock); session_busy = false; mutex_unlock(&session_lock); return result; }
    file->private_data = s;
    return 0;
}
static long bound_ioctl(struct file *file, unsigned int command, unsigned long value)
{
    struct session *s = file->private_data;
    int result;
    if (command != SAFE_MEDIA_RSS_END) return -ENOTTY;
    if (!s || current->pid != s->owner || current->mm != s->mm) return -EPERM;
    if (s->running) {
        result = stop_machine(end_stopped, s, NULL);
        if (result) return result;
    }
    return copy_to_user((void __user *)value, &s->result, sizeof(s->result)) ? -EFAULT : 0;
}
static int bound_release(struct inode *inode, struct file *file)
{
    struct session *s = file->private_data;
    (void)inode;
    if (s) {
        if (s->running) stop_machine(end_stopped, s, NULL);
        mmput(s->mm); kfree(s);
    }
    mutex_lock(&session_lock);
    session_busy = false;
    mutex_unlock(&session_lock);
    return 0;
}
static const struct file_operations bound_ops = {
    .owner = THIS_MODULE, .open = bound_open, .release = bound_release,
    .unlocked_ioctl = bound_ioctl
};
static struct miscdevice bound_device = {
    .minor = MISC_DYNAMIC_MINOR, .name = "safe_media_rss_bound",
    .fops = &bound_ops, .mode = 0600
};
static int __init bound_init(void)
{
    unsigned int n;
    int result;
    probes[0].symbol_name = "percpu_counter_add_batch";
    probes[1].symbol_name = "percpu_counter_set";
    probes[2].symbol_name = "__percpu_counter_limited_add";
    probes[0].pre_handler = probes[1].pre_handler = probes[2].pre_handler = count_positive;
    control_file = filp_open("/sys/kernel/debug/kprobes/enabled", O_RDONLY, 0);
    if (IS_ERR(control_file)) return PTR_ERR(control_file);
    probe_count = 3;
    if (control_file->f_op->write) {
        probes[probe_count].addr = (kprobe_opcode_t *)control_file->f_op->write;
        probes[probe_count++].pre_handler = global_control_write;
    }
    if (control_file->f_op->write_iter) {
        iterator_probe = &probes[probe_count];
        probes[probe_count].addr = (kprobe_opcode_t *)control_file->f_op->write_iter;
        probes[probe_count++].pre_handler = global_control_write;
    }
    if (probe_count == 3) { filp_close(control_file, NULL); return -EINVAL; }
    for (n = 0; n < probe_count; ++n) probe_ptrs[n] = &probes[n];
    result = register_kprobes(probe_ptrs, probe_count);
    if (result) { filp_close(control_file, NULL); return result; }
    result = misc_register(&bound_device);
    if (!result) return 0;
    unregister_kprobes(probe_ptrs, probe_count);
    filp_close(control_file, NULL);
    return result;
}
static void __exit bound_exit(void)
{
    misc_deregister(&bound_device);
    unregister_kprobes(probe_ptrs, probe_count);
    filp_close(control_file, NULL);
}

module_init(bound_init);
module_exit(bound_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Isolated safe-media RSS positive-delta bound experiment");
MODULE_VERSION("2");
