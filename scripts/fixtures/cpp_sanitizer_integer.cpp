#include <limits>

int main() {
    volatile int one = 1;
    return std::numeric_limits<int>::max() + one;
}
