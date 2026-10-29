#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    int socket_type;
    socklen_t size = sizeof(socket_type);
    if (getuid() != 65534 || geteuid() != 65534 || getgid() != 65534 ||
        getegid() != 65534 || getgroups(0, NULL) != 0 ||
        prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 1) {
        return 10;
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
