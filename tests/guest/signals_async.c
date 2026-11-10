#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/ucontext.h>
#include <unistd.h>

#define CHECK(c) do { if (!(c)) { fprintf(stderr,"FAIL line %d: %s (errno=%d)\n",__LINE__,#c,errno); return 1; } } while(0)
static volatile sig_atomic_t count, errors, depth, maxdepth;
static int mode;
static uintptr_t altbase;
static size_t altsize;
static pthread_t mainthread;
static pthread_t handlerthread;
static void handler(int sig, siginfo_t *info, void *context) {
    ucontext_t *uc = context;
    sigset_t mask;
    count++;
    if (sig != SIGUSR1 && sig != SIGALRM) errors++;
    if (!info || info->si_signo != sig || !uc || uc->uc_mcsize != sizeof(*uc->uc_mcontext)) errors++;
    if (sigprocmask(SIG_SETMASK,NULL,&mask)) errors++;
    if (mode != 4 && !sigismember(&mask,sig)) errors++;
    if (mode == 4 && sigismember(&mask,sig)) errors++;
    if (pthread_self() != handlerthread) errors++;
    if (mode == 1 && !sigismember(&mask,SIGUSR2)) errors++;
    if (mode == 2) {
        char local;
        stack_t current, disable = { .ss_sp = (void *)altbase, .ss_size = altsize, .ss_flags = SS_DISABLE };
        if ((uintptr_t)&local < altbase || (uintptr_t)&local >= altbase+altsize) errors++;
        if (sigaltstack(NULL,&current) || !(current.ss_flags & SS_ONSTACK)) errors++;
        errno = 0;
        if (sigaltstack(&disable,NULL) != -1 || errno != EPERM) errors++;
    }
    if (mode == 3) {
        if (uc->uc_mcontext->__ss.__x[19] != 0x1234 || uc->uc_mcontext->__ss.__x[20] != 0x5678) errors++;
        uint64_t *v = (uint64_t *)&uc->uc_mcontext->__ns.__v[8];
        if (v[0] != 0x1122 || v[1] != 0x3344) errors++;
        uc->uc_mcontext->__ss.__x[19] = 0xaabb;
        uc->uc_mcontext->__ss.__x[20] = 0xccdd;
        v[0] = 0x5566; v[1] = 0x7788;
    }
    if (mode == 4) {
        depth++; if (depth > maxdepth) maxdepth = depth;
        if (count == 1 && pthread_kill(mainthread,SIGUSR1)) errors++;
        depth--;
    }
    if (mode == 5) {
        struct sigaction old;
        if (sigaction(SIGUSR1,NULL,&old) || old.sa_handler != SIG_DFL) errors++;
    }
}
static int install(int signal, int flags) {
    struct sigaction action = { .sa_sigaction = handler, .sa_flags = SA_SIGINFO | flags };
    sigemptyset(&action.sa_mask);
    if (mode == 1) sigaddset(&action.sa_mask,SIGUSR2);
    return sigaction(signal,&action,NULL);
}
extern int register_roundtrip(pid_t pid, uint64_t *output);
static int pipefd[2];
static atomic_int worker_ready, worker_go;
static void *target_worker(void *unused) {
    (void)unused;
    handlerthread = pthread_self();
    sigset_t block, old; sigemptyset(&block); sigaddset(&block,SIGUSR1);
    if (pthread_sigmask(SIG_BLOCK,&block,&old)) return (void *)1;
    atomic_store_explicit(&worker_ready,1,memory_order_release);
    while (!atomic_load_explicit(&worker_go,memory_order_acquire)) usleep(1000);
    if (pthread_sigmask(SIG_SETMASK,&old,NULL)) return (void *)2;
    return (void *)(uintptr_t)(count != 1 || errors != 0);
}
static void *sender(void *argument) {
    usleep(100000);
    int result = pthread_kill(mainthread,SIGUSR1);
    usleep(100000);
    if (argument) { char c='Q'; if (write(pipefd[1],&c,1)!=1) result=1; }
    return (void *)(uintptr_t)result;
}
int main(int argc, char **argv) {
    mainthread = pthread_self();
    handlerthread = mainthread;
    const char *name = argc > 1 ? argv[1] : "basic";
    if (!strcmp(name,"basic")) {
        mode=1; CHECK(!install(SIGUSR1,0)); CHECK(!pthread_kill(mainthread,SIGUSR1));
        CHECK(count==1 && errors==0);
        sigset_t mask; CHECK(!sigprocmask(SIG_SETMASK,NULL,&mask));
        CHECK(!sigismember(&mask,SIGUSR1) && !sigismember(&mask,SIGUSR2));
    } else if (!strcmp(name,"mask") || !strcmp(name,"suspend")) {
        CHECK(!install(SIGUSR1,0));
        sigset_t block, old; sigemptyset(&block); sigaddset(&block,SIGUSR1);
        CHECK(!pthread_sigmask(SIG_BLOCK,&block,&old)); CHECK(!pthread_kill(mainthread,SIGUSR1));
        CHECK(count==0); sigset_t pending; CHECK(!sigpending(&pending)); CHECK(sigismember(&pending,SIGUSR1));
        if (!strcmp(name,"suspend")) { errno=0; CHECK(sigsuspend(&old)==-1 && errno==EINTR); CHECK(count==1); }
        CHECK(!pthread_sigmask(SIG_SETMASK,&old,NULL)); CHECK(count==1 && errors==0);
    } else if (!strcmp(name,"altstack")) {
        mode=2; altsize=65536; altbase=(uintptr_t)malloc(altsize); CHECK(altbase);
        stack_t stack = { .ss_sp=(void *)altbase, .ss_size=altsize }, current;
        CHECK(!sigaltstack(&stack,NULL)); CHECK(!install(SIGUSR1,SA_ONSTACK));
        CHECK(!pthread_kill(mainthread,SIGUSR1)); CHECK(count==1 && errors==0);
        CHECK(!sigaltstack(NULL,&current) && !(current.ss_flags & SS_ONSTACK));
        stack.ss_flags=SS_DISABLE; CHECK(!sigaltstack(&stack,NULL)); free((void *)altbase);
    } else if (!strcmp(name,"registers")) {
        mode=3; CHECK(!install(SIGUSR1,0)); uint64_t output[4]={0};
        CHECK(!register_roundtrip(getpid(),output)); CHECK(count==1 && errors==0);
        CHECK(output[0]==0xaabb && output[1]==0xccdd && output[2]==0x5566 && output[3]==0x7788);
    } else if (!strcmp(name,"nested")) {
        mode=4; CHECK(!install(SIGUSR1,SA_NODEFER)); CHECK(!pthread_kill(mainthread,SIGUSR1));
        CHECK(count==2 && maxdepth==2 && depth==0 && errors==0);
    } else if (!strcmp(name,"reset")) {
        mode=5; CHECK(!install(SIGUSR1,SA_RESETHAND)); CHECK(!pthread_kill(mainthread,SIGUSR1)); CHECK(count==1 && errors==0);
    } else if (!strcmp(name,"read") || !strcmp(name,"restart")) {
        CHECK(!install(SIGUSR1,!strcmp(name,"restart") ? SA_RESTART : 0)); CHECK(!pipe(pipefd));
        pthread_t thread; CHECK(!pthread_create(&thread,NULL,sender,(void *)1));
        char c=0; errno=0; ssize_t n=read(pipefd[0],&c,1);
        if (!strcmp(name,"read")) { CHECK(n==-1 && errno==EINTR); n=read(pipefd[0],&c,1); }
        CHECK(n==1 && c=='Q' && count==1 && errors==0); void *result; CHECK(!pthread_join(thread,&result) && !result);
    } else if (!strcmp(name,"worker")) {
        CHECK(!install(SIGUSR1,0)); pthread_t target;
        CHECK(!pthread_create(&target,NULL,target_worker,NULL));
        while (!atomic_load_explicit(&worker_ready,memory_order_acquire)) usleep(1000);
        CHECK(!pthread_kill(target,SIGUSR1)); CHECK(count==0);
        atomic_store_explicit(&worker_go,1,memory_order_release);
        void *result; CHECK(!pthread_join(target,&result) && !result);
        CHECK(count==1 && errors==0);
    } else if (!strcmp(name,"alarm")) {
        CHECK(!install(SIGALRM,0)); struct itimerval timer={ .it_value={ .tv_usec=100000 } };
        CHECK(!setitimer(ITIMER_REAL,&timer,NULL));
        // No syscalls inside this hot loop: chained JIT must yield to dispatch.
        while (!count) __asm__ volatile("add x8, x8, #1" ::: "x8");
        CHECK(count==1 && errors==0);
    } else if (!strcmp(name,"invalid")) {
        errno=0; CHECK(syscall(SYS_sigaction,SIGUSR1,(void *)1,NULL)==-1 && errno==EFAULT);
        errno=0; CHECK(syscall(SYS_sigaltstack,(void *)1,NULL)==-1 && errno==EFAULT);
        errno=0; CHECK(syscall(SYS_sigreturn,(void *)1,30,0)==-1 && errno==EINVAL);
    } else return 2;
    printf("PASS: %s (count=%d errors=%d)\n",name,count,errors);
    return 0;
}
