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
