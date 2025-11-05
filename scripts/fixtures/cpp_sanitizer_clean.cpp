#include <vector>

int main() {
    std::vector<int> values(16, 3);
    int total = 0;
    for (int value : values) total += value;
    return total == 48 ? 0 : 1;
}
