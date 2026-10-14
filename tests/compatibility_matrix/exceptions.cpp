#include <atomic>
#include <cstdio>
#include <stdexcept>
#include <thread>
#include <vector>

static std::atomic<int> destroyed(0);
struct Guard { ~Guard() { ++destroyed; } };
static thread_local int tls = 0;
__attribute__((noinline)) static void leaf(int n) {
    Guard guard;
    if (n & 1) throw std::runtime_error("maclator-EH");
    throw n;
}
__attribute__((noinline)) static bool trial(int n) {
    Guard guard;
    try { leaf(n); }
    catch (const std::runtime_error &e) { return (n & 1) && e.what()[0] == 'm'; }
    catch (int v) { return !(n & 1) && v == n; }
    return false;
}
int main() {
    std::atomic<int> failures(0);
    std::vector<std::thread> threads;
    for (int t = 0; t < 4; ++t) threads.emplace_back([&] {
        for (int i = 0; i < 100; ++i) if (!trial(i)) ++failures;
        if (++tls != 1) ++failures;
    });
    for (auto &thread : threads) thread.join();
    bool ok = failures == 0 && destroyed == 800 && tls == 0;
    std::printf("CXX_EH_TLS %s destroyed=%d failures=%d\n", ok ? "PASS" : "FAIL",
                destroyed.load(), failures.load());
    return ok ? 0 : 1;
}
