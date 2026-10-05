int deref(int *p) {
	if (p == 0) {
		return *p;
	}
	return 1;
}

int main(void) {
	char a[4];
	a[4] = 0;
	return deref(0) + a[0];
}
