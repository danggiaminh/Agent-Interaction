// C++20 with the standard library, threads and exceptions.
#include <algorithm>
#include <format>
#include <iostream>
#include <numeric>
#include <stdexcept>
#include <thread>
#include <vector>

int main() {
	std::vector<int> v(10);
	std::iota(v.begin(), v.end(), 1);
	int sum = 0;
	std::thread t([&] { sum = std::accumulate(v.begin(), v.end(), 0); });
	t.join();
	try {
		throw std::runtime_error("caught");
	} catch (const std::exception &e) {
		std::cout << std::format("cpp-ok {} {}\n", sum, e.what());
	}
	return std::ranges::is_sorted(v) ? 0 : 1;
}
