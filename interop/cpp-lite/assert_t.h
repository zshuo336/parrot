// 最小 assert 集（test_vectors.cpp / test_interop.cpp 共用）。
#ifndef PL_ASSERT_T_H
#define PL_ASSERT_T_H

#include <cstdio>

static int g_fail = 0;

#define CHECK(cond, msg)                                                       \
  do {                                                                         \
    if (!(cond)) {                                                             \
      printf("FAIL %s:%d %s\n", __FILE__, __LINE__, msg);                      \
      g_fail++;                                                                \
    }                                                                          \
  } while (0)

#endif
