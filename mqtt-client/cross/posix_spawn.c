/* posix_spawn 辅助函数补全桩（RV1106 / uclibc 1.0.31）
 *
 * 背景：Rust 官方无 armv7-*-uclibceabihf target，本项目使用
 * `armv7-unknown-linux-gnueabihf` + Rockchip uclibc 工具链链接（见 .cargo/config.toml）。
 * 该 target 下 Rust std 的 `std::process::Command` 代码路径会引用完整的 posix_spawn
 * 接口族；而 uclibc 的 libc 只导出：
 *     posix_spawn, posix_spawnp,
 *     posix_spawn_file_actions_{addclose,adddup2,addopen}
 * 其余（posix_spawn_file_actions_{init,destroy} 与 posix_spawnattr_*）在 uclibc 的
 * <spawn.h> 中**只是 static inline**，对 C 调用方有效，但 Rust 通过 extern 符号引用，
 * 链接时得不到定义 → "undefined reference to posix_spawn*"。
 *
 * 因此本文件**故意不 include <spawn.h>**（否则那些 static inline 会与这里的外部
 * 定义冲突，报 redefinition），而是按 uclibc spawn.h 的布局自行声明等价结构体，
 * 再导出外部符号。结构体字段与 uclibc 保持一致，故与 libc 中原生的
 * posix_spawn / adddup2 / addclose 协同正常（真正的 spawn 与 fd 重定向仍由 uclibc
 * 原生实现完成，本桩只负责属性与 file_actions 容器的初始化/销毁）。
 *
 * 构建：
 *   arm-rockchip830-linux-uclibcgnueabihf-gcc -O2 -c posix_spawn.c -o posix_spawn.o
 *   arm-rockchip830-linux-uclibcgnueabihf-ar  rcs libposix_spawn.a posix_spawn.o
 */

#include <signal.h>   /* sigset_t */
#include <sched.h>    /* struct sched_param */
#include <sys/types.h>/* pid_t */
#include <stdlib.h>
#include <string.h>
#include <errno.h>

/* 仅作指针使用，无需完整定义（uclibc 内部类型） */
struct __spawn_action;

/* 与 uclibc <spawn.h> 中 posix_spawnattr_t 布局一致 */
typedef struct
{
    short int __flags;
    pid_t __pgrp;
    sigset_t __sd;
    sigset_t __ss;
    struct sched_param __sp;
    int __policy;
    int __pad[16];
} posix_spawnattr_t;

/* 与 uclibc <spawn.h> 中 posix_spawn_file_actions_t 布局一致 */
typedef struct
{
    int __allocated;
    int __used;
    struct __spawn_action *__actions;
    int __pad[16];
} posix_spawn_file_actions_t;

int posix_spawn_file_actions_init(posix_spawn_file_actions_t *fa)
{
    if (fa == NULL) {
        return EINVAL;
    }
    memset(fa, 0, sizeof(*fa));
    return 0;
}

int posix_spawn_file_actions_destroy(posix_spawn_file_actions_t *fa)
{
    if (fa == NULL) {
        return EINVAL;
    }
    /* __actions 由 uclibc 的 addclose/adddup2/addopen 通过 malloc/realloc 分配 */
    if (fa->__actions != NULL) {
        free(fa->__actions);
        fa->__actions = NULL;
    }
    fa->__allocated = 0;
    fa->__used = 0;
    return 0;
}

int posix_spawnattr_init(posix_spawnattr_t *attr)
{
    if (attr == NULL) {
        return EINVAL;
    }
    memset(attr, 0, sizeof(*attr));
    return 0;
}

int posix_spawnattr_destroy(posix_spawnattr_t *attr)
{
    if (attr == NULL) {
        return EINVAL;
    }
    memset(attr, 0, sizeof(*attr));
    return 0;
}

int posix_spawnattr_setflags(posix_spawnattr_t *attr, short int flags)
{
    if (attr == NULL) {
        return EINVAL;
    }
    attr->__flags = flags;
    return 0;
}

int posix_spawnattr_setsigmask(posix_spawnattr_t *attr, const sigset_t *sigmask)
{
    if (attr == NULL || sigmask == NULL) {
        return EINVAL;
    }
    memcpy(&attr->__ss, sigmask, sizeof(sigset_t));
    return 0;
}

int posix_spawnattr_setsigdefault(posix_spawnattr_t *attr, const sigset_t *sigdefault)
{
    if (attr == NULL || sigdefault == NULL) {
        return EINVAL;
    }
    memcpy(&attr->__sd, sigdefault, sizeof(sigset_t));
    return 0;
}

int posix_spawnattr_setpgroup(posix_spawnattr_t *attr, pid_t pgroup)
{
    if (attr == NULL) {
        return EINVAL;
    }
    attr->__pgrp = pgroup;
    return 0;
}
