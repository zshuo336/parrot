int main() {
    volatile int* value = new int(7);
    delete value;
    return *value;
}
