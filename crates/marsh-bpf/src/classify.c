/* Host build of the kernel classifier, so tests can compare it with marsh-peer-class. */
#include "marsh.h"

int marsh_host_builtin_class(const struct marsh_endpoint *e)
{
    return marsh_builtin_class(e);
}

void marsh_host_normalize_ip(struct marsh_endpoint *e)
{
    marsh_normalize_ip(e);
}

/* The kernel's name matcher on a whole name, split into head and tail as the programs read it. */
int marsh_host_name_matches(const struct marsh_name_rule *rule, const __u8 *name, __u32 len)
{
    __u8 head[NAME_GLOB] = {0}, tail[NAME_GLOB] = {0};
    for (__u32 i = 0; i < NAME_GLOB && i < len; i++)
        head[i] = name[i];
    if (len > NAME_GLOB)
        for (__u32 i = 0; i < NAME_GLOB; i++)
            tail[i] = name[len - NAME_GLOB + i];
    return marsh_name_matches(rule, head, tail, len);
}
