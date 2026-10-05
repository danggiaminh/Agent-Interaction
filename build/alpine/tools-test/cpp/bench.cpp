// Google Benchmark micro-benchmark.
#include <benchmark/benchmark.h>

#include <numeric>
#include <vector>

static void BM_Accumulate(benchmark::State &state) {
	std::vector<int> v(1024, 1);
	for (auto _ : state) {
		benchmark::DoNotOptimize(std::accumulate(v.begin(), v.end(), 0));
	}
}
BENCHMARK(BM_Accumulate);
