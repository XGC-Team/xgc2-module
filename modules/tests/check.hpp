#pragma once
// A minimal assertion harness for the module tests: failures are counted and printed, main returns non-zero.
#include <cstdio>
#include <cstdlib>
#include <string>
#include <type_traits>

namespace check {
inline std::string show(const std::string& v) { return "\"" + v + "\""; }
inline std::string show(double v) { return std::to_string(v); }
inline std::string show(const char* v) { return std::string("\"") + v + "\""; }
template <typename T>
std::string show(const T& v) { return std::to_string(static_cast<long long>(v)); }
inline int& failures() {
    static int count = 0;
    return count;
}
inline void fail(const char* file, int line, const std::string& what) {
    std::fprintf(stderr, "%s:%d: CHECK failed: %s\n", file, line, what.c_str());
    ++failures();
}
}  // namespace check

#define CHECK(cond) \
    do { if (!(cond)) check::fail(__FILE__, __LINE__, #cond); } while (0)
#define CHECK_EQ(a, b) \
    do { auto va = (a); auto vb = (b); if (!(va == vb)) check::fail(__FILE__, __LINE__, std::string(#a " == " #b " (") + check::show(va) + " vs " + check::show(vb) + ")"); } while (0)
#define CHECK_NEAR(a, b, tol) \
    do { double va = (a); double vb = (b); if (!(va >= vb - (tol) && va <= vb + (tol))) check::fail(__FILE__, __LINE__, std::string(#a " near " #b " (") + check::show(va) + " vs " + check::show(vb) + ")"); } while (0)
#define TEST_MAIN_END() \
    do { if (check::failures() != 0) { std::fprintf(stderr, "%d check(s) failed\n", check::failures()); return 1; } std::puts("ok"); return 0; } while (0)
