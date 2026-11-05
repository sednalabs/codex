#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <linux/capability.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    int socket_type;
    socklen_t size = sizeof(socket_type);
    uid_t ru, eu, su;
    gid_t rg, eg, sg;
    if (getresuid(&ru, &eu, &su) || getresgid(&rg, &eg, &sg) ||
        ru != 992 || eu != 992 || su != 992 || rg != 981 || eg != 981 ||
        sg != 981 || getgroups(0, NULL) != 0 ||
        prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 1) {
        return 10;
    }
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct caps[2] = {{0}};
    if (syscall(SYS_capget, &header, caps) != 0) return 13;
    for (int i = 0; i < 2; ++i) {
        if (caps[i].effective || caps[i].permitted || caps[i].inheritable) return 13;
    }
    for (int cap = 0; cap < 64; ++cap) {
        errno = 0;
        int result = prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, cap, 0, 0);
        if (result == 1 || (result < 0 && errno != EINVAL)) return 14;
    }
    for (int fd = 3; fd <= 4; ++fd) {
        size = sizeof(socket_type);
        if (getsockopt(fd, SOL_SOCKET, SO_TYPE, &socket_type, &size) != 0 ||
            socket_type != SOCK_SEQPACKET) {
            return 11;
        }
    }
    for (int fd = 5; fd < 1024; ++fd) {
        errno = 0;
        if (fcntl(fd, F_GETFD) >= 0 || errno != EBADF) {
            return 12;
        }
    }
    puts("verified_fd_spawn_ok");
    return 0;
}
