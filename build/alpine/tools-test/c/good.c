int deref(const int *p) {
    if (p == 0) {
        return 0;
    }
    return *p;
}

int main(void) {
    const char a[4] = {0};
    return deref(0) + a[0];
}
