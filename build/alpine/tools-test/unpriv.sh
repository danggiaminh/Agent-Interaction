#!/bin/sh
# Runs inside the tools image as root of an unprivileged user namespace (tools-test.sh -> tools-run.sh), offline.
# Exercises every tool that needs no host privilege and prints one KEY=VALUE line per fact; tools-test.sh judges
# them against inventory.tsv. Nothing here fails the script: a failed step is reported as its own value.
#   unpriv.sh [section...]     default: every section (useful for iterating: unpriv.sh debug perf)
cd /work || exit 1
. /work/common.sh

# ------------------------------------------------------------------------------------------------ identity
if section ident; then
	kv gcc_version "$(gcc -dumpfullversion)"
	kv gxx_version "$(g++ -dumpfullversion)"
	kv clang_version "$(clang --version | sed -n '1s/.*version \([0-9.]*\).*/\1/p')"
	kv clang_tidy_version "$(clang-tidy --version | sed -n 's/.*LLVM version \([0-9.]*\).*/\1/p')"
	kv clang_format_version "$(clang-format --version | sed -n 's/.*version \([0-9.]*\).*/\1/p')"
	kv cppcheck_version "$(cppcheck --version | sed 's/^Cppcheck //')"
	kv cmake_version "$(cmake --version | sed -n '1s/.* //p')"
	kv pkgconf_version "$(pkgconf --version)"
	kv gdb_version "$(gdb --version | sed -n '1s/.* //p')"
	kv strace_version "$(strace --version | sed -n '1s/.* //p')"
	kv ltrace_version "$(ltrace --version | sed -n '1s/^ltrace version //; 1s/\.$//p')"
	kv valgrind_version "$(valgrind --version | sed 's/^valgrind-//')"
	kv perf_version "$(perf --version | sed 's/^perf version //')"
	kv bpftool_version "$(bpftool version 2>&1 | sed -n '1s/^bpftool v//p')"
	kv hyperfine_version "$(hyperfine --version | sed 's/^hyperfine //')"
	kv sysbench_version "$(sysbench --version | sed 's/^sysbench //')"
	kv fio_version "$(fio --version | sed 's/^fio-//')"
	kv benchmark_version "$(pkgconf --modversion benchmark)"
	kv stress_ng_version "$(stress-ng --version | sed -n '1s/^stress-ng, version \([0-9.]*\).*/\1/p')"
	kv iperf3_version "$(iperf3 --version | sed -n '1s/^iperf \([0-9.]*\).*/\1/p')"
	kv iproute2_version "$(ip -V | sed 's/.*iproute2-//')"
	kv tcpdump_version "$(tcpdump --version 2>&1 | sed -n '1s/^tcpdump version //p')"
	kv lsof_version "$(lsof -v 2>&1 | sed -n 's/^ *revision: *//p')"
	kv e2fsprogs_version "$(mke2fs -V 2>&1 | sed -n '1s/^mke2fs \([0-9.]*\).*/\1/p')"
	kv xfsprogs_version "$(mkfs.xfs -V | sed 's/^mkfs.xfs version //')"
	kv btrfs_version "$(btrfs --version | sed -n '1s/^btrfs-progs v//p')"
	kv dosfstools_version "$(mkfs.fat --help 2>&1 | sed -n 's/^mkfs.fat \([0-9.]*\).*/\1/p')"
	kv mtools_version "$(mtools -V 2>&1 | sed -n '1s/^mtools (GNU mtools) //p')"
	kv squashfs_version "$(mksquashfs -version | sed -n '1s/.*version \([0-9.]*\).*/\1/p')"
	kv erofs_version "$(mkfs.erofs --version 2>&1 | sed -n '1s/^mkfs.erofs (erofs-utils) //p')"
	kv xorriso_version "$(xorriso -version 2>&1 | sed -n 's/^xorriso version *: *//p')"
	kv qemu_version "$(qemu-system-x86_64 --version | sed -n '1s/.*version \([0-9.]*\).*/\1/p')"
	kv qemu_img_version "$(qemu-img --version | sed -n '1s/.*version \([0-9.]*\).*/\1/p')"
	kv util_linux_version "$(mount --version | sed -n '1s/^mount from util-linux \([0-9.]*\).*/\1/p')"
	kv gptfdisk_version "$(gdisk -v 2>&1 </dev/null | sed -n 's/^GPT fdisk (gdisk) version //p')"
	kv sfdisk_version "$(sfdisk --version | sed -n '1s/^sfdisk from util-linux //p')"
	kv partx_version "$(partx --version | sed -n '1s/^partx from util-linux //p')"
	kv cargo_version "$(cargo --version | cut -d' ' -f2)"
	kv rustfmt_version "$(rustfmt --version | cut -d' ' -f2)"
	kv clippy_version "$(cargo clippy --version | cut -d' ' -f2)"
	kv nextest_version "$(cargo nextest --version | sed -n '1s/^cargo-nextest \([0-9.]*\).*/\1/p')"
	kv flamegraph_version "$(cargo flamegraph --version 2>&1 | sed -n '1s/^flamegraph-flamegraph //p')"
	kv nft_version "$(nft --version | sed -n '1s/^nftables v\([0-9.]*\).*/\1/p')"
	kv iptables_version "$(iptables --version | sed -n '1s/^iptables v\([0-9.]*\).*/\1/p')"
	kv libvirt_version "$(virsh --version)"
	kv libvirtd_version "$(libvirtd --version | sed -n '1s/.* //p')"
	kv docker_version "$(docker --version | sed -n '1s/^Docker version \([0-9.]*\).*/\1/p')"
	kv compose_version "$(docker compose version 2>&1 | sed -n 's/^Docker Compose version v\{0,1\}//p')"
	kv buildx_version "$(docker buildx version 2>&1 | sed -n '1s/^github.com\/docker\/buildx v\([0-9.]*\).*/\1/p')"
	kv dockerd_version "$(dockerd --version | sed -n '1s/^Docker version \([0-9.]*\).*/\1/p')"
	kv containerd_version "$(containerd --version | sed -n '1s/.* v\([0-9.]*\) .*/\1/p')"
	kv runc_version "$(runc --version | sed -n '1s/^runc version //p')"
fi

# ------------------------------------------------------------------------------------------------ C and C++
if section compile; then
	run gcc_build gcc -O2 -Wall -Wextra -Werror -o "$T/clean-gcc" c/clean.c
	found gcc_run '^42$' "$T/clean-gcc"
	run clang_build clang -O2 -Wall -Wextra -Werror -o "$T/clean-clang" c/clean.c
	found clang_run '^42$' "$T/clean-clang"
	found elf_file 'ELF 64-bit LSB.*x86-64.*interpreter /lib/ld-musl-x86_64.so.1' file "$T/clean-clang"
	run gxx_build g++ -std=c++20 -O2 -Wall -Wextra -Werror -pthread -o "$T/hello-gxx" cpp/hello.cpp
	found gxx_run '^cpp-ok 55 caught$' "$T/hello-gxx"
	run clangxx_build clang++ -std=c++20 -O2 -Wall -Wextra -Werror -pthread -o "$T/hello-clangxx" cpp/hello.cpp
	found clangxx_run '^cpp-ok 55 caught$' "$T/hello-clangxx"
	run linux_headers sh -c 'printf "#include <linux/futex.h>\n#include <linux/perf_event.h>\n#include <linux/bpf.h>\nint main(void) { return 0; }\n" | gcc -x c - -o /tmp/t/lh'
	run cmake_configure cmake -S cpp -B "$T/cpp" -G Ninja -DCMAKE_BUILD_TYPE=Release
	run cmake_build cmake --build "$T/cpp"
	found cmake_run '^cpp-ok 55 caught$' "$T/cpp/cpp-hello"
	found ctest_run '100% tests passed' ctest --test-dir "$T/cpp"
	found ninja_is_samurai 'samu' readlink -f /usr/bin/ninja
	found samu_noop 'nothing to do|^samu' samu -C "$T/cpp"
	found pkgconf_flags '(-lbenchmark|-I/usr/include)' pkgconf --cflags --libs benchmark
	found bench_run 'BM_Accumulate' "$T/cpp/cpp-bench" --benchmark_min_time=0.05s
fi

# ------------------------------------------------------------------------------------------------ format and lint
if section lint; then
	run clang_format_ok sh -c 'cd c && clang-format --dry-run -Werror good.c'
	found clang_format_detects 'clang-format-violations' sh -c 'cd c && clang-format --dry-run -Werror fmt-bad.c'
	found clang_tidy_detects 'warning: .*(null|Null)' clang-tidy --checks='-*,clang-analyzer-*' c/bad.c -- -std=c11
	run clang_tidy_clean clang-tidy --warnings-as-errors='*' --checks='-*,clang-analyzer-*,bugprone-*' c/good.c -- -std=c11
	found cppcheck_detects 'arrayIndexOutOfBounds' cppcheck --enable=warning --template='{id}: {message}' c/bad.c
	run cppcheck_clean cppcheck --error-exitcode=1 --enable=warning,style c/good.c
fi

# ------------------------------------------------------------------------------------------------ Rust
if section rust; then
	rm -rf "$T/rust"
	cp -a rust "$T/rust"
	cd "$T/rust" || exit 1
	run cargo_fmt cargo fmt --check
	found rustfmt_detects 'Diff in' rustfmt --check --edition 2021 bad.rs
	run cargo_clippy cargo clippy --offline --locked --all-targets -- -D warnings
	found clippy_detects 'error: ' clippy-driver --edition 2021 --crate-type lib -D warnings -o "$T/bad.rlib" bad.rs
	run clippy_clean clippy-driver --edition 2021 --crate-type lib -D warnings -o "$T/ok.rlib" clippy-ok.rs
	run cargo_test cargo test --offline --locked
	found nextest_run '2 tests run: 2 passed' cargo nextest run --offline --locked
	run cargo_build_release cargo build --release --offline --locked
	found burn_run '^burn [0-9]+$' target/release/burn 1000
	found burn_file 'ELF 64-bit LSB.*x86-64.*ld-musl-x86_64' file target/release/burn
	found cargo_bench 'bench fib: sum=' cargo bench --offline --locked
	cd /work || exit 1
fi

# ------------------------------------------------------------------------------------------------ debug and trace
if section debug; then
	gcc -g -O0 -o "$T/clean-g" c/clean.c
	found gdb_backtrace '^#0 +add \(' gdb -q -batch -ex 'break add' -ex run -ex bt "$T/clean-g"
	found gdb_args '^\$1 = 40' gdb -q -batch -ex 'break add' -ex run -ex 'print a' "$T/clean-g"
	if ensure_rust; then
		found rust_gdb_backtrace '^#0 +burn::burn' rust-gdb -q -batch -ex 'break burn::burn' -ex 'run 1000' -ex bt "$T/rust/target/release/burn"
	else
		kv rust_gdb_backtrace "none:cannot build the Rust fixture: $(tail -n 1 "$T/ensure_rust.log")"
	fi
	found strace_files 'hostname' strace -f -e trace=file cat /etc/hostname
	found strace_summary 'total' sh -c "strace -c -o $T/st-sum.txt cat /etc/hostname >/dev/null; cat $T/st-sum.txt"
	gcc -g -O0 -o "$T/clean-o0" c/clean.c
	gcc -g -O0 -no-pie -o "$T/clean-nopie" c/clean.c
	found ltrace_malloc 'malloc\(16\)' ltrace -e malloc@MAIN+free@MAIN "$T/clean-nopie"
	found ltrace_count 'malloc' ltrace -c "$T/clean-nopie"
	found ltrace_libwide 'malloc\(16\)' ltrace -e malloc "$T/clean-nopie"
	gcc -g -O0 -o "$T/leak" c/leak.c
	found valgrind_leak 'definitely lost: 16 bytes' valgrind --leak-check=full "$T/leak"
	found valgrind_clean 'All heap blocks were freed' valgrind --leak-check=full "$T/clean-o0"
	gcc -g -O0 -fsanitize=address -o "$T/uaf-gcc" c/uaf.c
	found asan_gcc 'heap-use-after-free' "$T/uaf-gcc"
	clang -g -O0 -fsanitize=address -o "$T/uaf-clang" c/uaf.c
	found asan_clang 'heap-use-after-free' "$T/uaf-clang"
	gcc -g -O0 -fsanitize=undefined -o "$T/ovf-gcc" c/ovf.c
	found ubsan_gcc 'signed integer overflow' "$T/ovf-gcc"
	clang -g -O0 -fsanitize=undefined -o "$T/ovf-clang" c/ovf.c
	found ubsan_clang 'signed integer overflow' "$T/ovf-clang"
fi

# ------------------------------------------------------------------------------------------------ profile
if section perf; then
	# perf finds the software PMUs (cpu-clock, task-clock) through sysfs; the session has none mounted.
	[ -d /sys/bus/event_source/devices/software ] || mount -t sysfs sysfs /sys 2>/dev/null
	if ! ensure_rust; then
		kv perf_record "none:cannot build the Rust fixture: $(tail -n 1 "$T/ensure_rust.log")"
	else
		B="$T/rust/target/release/burn"
		found perf_stat_sw 'task-clock' perf stat -e task-clock -- "$B" 200000
		found perf_record '[0-9]+ samples' perf record -e cpu-clock -F 999 -g -o "$T/perf.data" -- "$B" 3000000
		found perf_report 'fib' sh -c "perf report -i $T/perf.data --stdio --no-children 2>/dev/null | grep -v '^#' | grep -v '^$'"
		found perf_bench 'Total time:' perf bench sched pipe -l 20000
		found perf_hw_cycles 'cycles' perf stat -e cycles -- "$B" 1000
		cd "$T/rust" || exit 1
		run flamegraph cargo flamegraph --bin burn -o "$T/fg.svg" -- 3000000
		found flamegraph_svg 'fib' grep -m1 -o 'toolbox::fib' "$T/fg.svg"
		cd /work || exit 1
	fi
	found bpftool_ver 'bpftool v' bpftool version
fi

# ------------------------------------------------------------------------------------------------ benchmark
if section bench; then
	found hyperfine_json '"mean"' sh -c "hyperfine -N --warmup 1 --runs 5 --export-json $T/hf.json true >/dev/null && cat $T/hf.json"
	found sysbench_cpu 'events per second' sysbench cpu --threads=2 --time=1 run
	found sysbench_memory 'MiB transferred' sysbench memory --time=1 run
	mkdir -p "$T/sb"
	run sysbench_fileio_prepare sh -c "cd $T/sb && sysbench fileio --file-total-size=16M --file-num=4 prepare"
	found sysbench_fileio 'reads/s' sh -c "cd $T/sb && sysbench fileio --file-total-size=16M --file-num=4 --file-test-mode=rndrw --time=1 run"
	sh -c "cd $T/sb && sysbench fileio --file-total-size=16M --file-num=4 cleanup" >/dev/null 2>&1
	found fio_psync '"iops" *: *[1-9]' fio --name=probe --directory="$T" --size=8M --bs=4k --rw=randrw --ioengine=psync --direct=0 --runtime=1 --time_based --output-format=json
	found fio_libaio '"iops" *: *[1-9]' fio --name=probe --directory="$T" --size=8M --bs=4k --rw=randrw --ioengine=libaio --direct=0 --runtime=1 --time_based --output-format=json
	found fio_io_uring '"iops" *: *[1-9]|io_uring.*(not permitted|not supported|failed)|Operation not permitted' fio --name=probe --directory="$T" --size=8M --bs=4k --rw=randrw --ioengine=io_uring --direct=0 --runtime=1 --time_based --output-format=json
fi

# ------------------------------------------------------------------------------------------------ pressure
if section pressure; then
	ip link set lo up
	found stress_cpu 'successful run completed' stress-ng --cpu 2 --timeout 2s --metrics-brief
	found stress_vm 'successful run completed' stress-ng --vm 1 --vm-bytes 64M --timeout 2s --metrics-brief
	found stress_fork 'successful run completed' stress-ng --fork 2 --timeout 2s --metrics-brief
	found stress_pthread 'successful run completed' stress-ng --pthread 2 --timeout 2s --metrics-brief
	found stress_open 'successful run completed' stress-ng --open 2 --timeout 2s --metrics-brief
	found stress_sock 'successful run completed' stress-ng --sock 1 --sock-domain ipv4 --sock-port 15000 --timeout 2s --metrics-brief
	found stress_hdd 'successful run completed' stress-ng --hdd 1 --hdd-bytes 16M --temp-path "$T" --timeout 2s --metrics-brief
	gcc -O0 -o "$T/fdhog" c/fdhog.c
	found fd_exhaustion '^EMFILE opened=[0-9]+$' sh -c "ulimit -n 64 && $T/fdhog"
	gcc -O0 -o "$T/memhog" c/memhog.c
	found mem_exhaustion '^ENOMEM blocks=[0-9]+$' sh -c "ulimit -v 262144 && $T/memhog"
	# Network pressure on the loopback: capture while iperf3 runs, with the listener visible to ss and lsof.
	tcpdump -i lo -n -c 6 -w "$T/cap.pcap" >"$T/tcpdump.log" 2>&1 &
	i=0
	while [ $i -lt 50 ] && ! grep -q 'listening on lo' "$T/tcpdump.log"; do sleep 0.1; i=$((i + 1)); done
	iperf3 -s -p 5201 >"$T/iperf-server.log" 2>&1 &
	srv=$!
	i=0
	while [ $i -lt 50 ] && ! ss -ltn | grep -q ':5201'; do sleep 0.1; i=$((i + 1)); done
	found ss_listener ':5201' ss -ltn
	found lsof_listener 'iperf3.*LISTEN' lsof -nP -iTCP:5201 -sTCP:LISTEN
	found iperf3_loopback '"bits_per_second":[[:space:]]*[1-9]' iperf3 -c 127.0.0.1 -p 5201 -t 1 -J
	kill "$srv" 2>/dev/null
	wait
	found tcpdump_capture '127\.0\.0\.1\.[0-9]+ > 127\.0\.0\.1\.5201' tcpdump -nr "$T/cap.pcap"
	found ip_loopback 'LOOPBACK,UP' ip -o link show lo
	found ip_json '"ifname" *: *"lo"' ip -j addr show lo
	found lsof_self '/tmp/t' sh -c "exec 9>$T/lsof-probe; lsof -p \$\$"
fi

# ------------------------------------------------------------------------------------------------ filesystems and images
if section fs; then
	F="$T/fs"
	mkdir -p "$F/tree/etc" "$F/tree/data" "$F/tree/bin"
	printf 'hello image\n' >"$F/tree/etc/motd"
	seq 1 20000 >"$F/tree/data/numbers"
	ln -s ../etc/motd "$F/tree/data/link"
	cp /bin/busybox "$F/tree/bin/busybox"
	chmod 640 "$F/tree/etc/motd"
	find "$F/tree" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} + 2>/dev/null
	U=11111111-2222-3333-4444-555555555555

	# ext2/3/4
	for n in a b; do
		E2FSPROGS_FAKE_TIME="$SOURCE_DATE_EPOCH" mke2fs -q -t ext4 -d "$F/tree" -L toolsfs -U $U -E hash_seed=$U,root_owner=0:0 -b 4096 "$F/ext4-$n.img" 16M >"$T/mke2fs-$n.log" 2>&1
	done
	same ext4_deterministic "$F/ext4-a.img" "$F/ext4-b.img"
	run e2fsck e2fsck -fn "$F/ext4-a.img"
	found debugfs_ls 'numbers' debugfs -R 'ls -l /data' "$F/ext4-a.img"
	found debugfs_cat '^hello image$' debugfs -R 'cat /etc/motd' "$F/ext4-a.img"
	found tune2fs_label 'volume name: *toolsfs' tune2fs -l "$F/ext4-a.img"
	found dumpe2fs_uuid "Filesystem UUID: *$U" dumpe2fs -h "$F/ext4-a.img"
	found e2label_read '^toolsfs$' e2label "$F/ext4-a.img"
	found e2freefrag_run 'Total blocks' e2freefrag "$F/ext4-a.img"
	cp "$F/ext4-a.img" "$F/ext4-grow.img"
	truncate -s 48M "$F/ext4-grow.img"
	run resize2fs_grow resize2fs "$F/ext4-grow.img"
	found resize2fs_blocks 'Block count: *12288' dumpe2fs -h "$F/ext4-grow.img"
	run resize2fs_fsck e2fsck -fn "$F/ext4-grow.img"
	run mke2fs_ext2 mke2fs -q -t ext2 -L old "$F/ext2.img" 4M
	found e2image_run '' e2image -r "$F/ext4-a.img" "$F/ext4-meta.img"
	run e2image_fsck e2fsck -fn "$F/ext4-meta.img"

	# squashfs
	for n in a b; do
		mksquashfs "$F/tree" "$F/$n.sqsh" -quiet -noappend -all-root -processors 1 -comp zstd >"$T/mksquashfs-$n.log" 2>&1
	done
	same squashfs_deterministic "$F/a.sqsh" "$F/b.sqsh"
	found unsquashfs_list 'etc/motd' unsquashfs -ll "$F/a.sqsh"
	run unsquashfs_extract unsquashfs -q -d "$F/sq-out" "$F/a.sqsh"
	run squashfs_content diff -r "$F/tree" "$F/sq-out"

	# erofs
	for n in a b; do
		mkfs.erofs -T 0 -U $U -zlz4hc "$F/$n.erofs" "$F/tree" >"$T/mkfs-erofs-$n.log" 2>&1
	done
	same erofs_deterministic "$F/a.erofs" "$F/b.erofs"
	run fsck_erofs fsck.erofs "$F/a.erofs"
	found dump_erofs 'Filesystem UUID: *'"$U" dump.erofs -s "$F/a.erofs"
	run erofs_extract fsck.erofs --extract="$F/ero-out" "$F/a.erofs"
	run erofs_content diff -r "$F/tree/etc" "$F/ero-out/etc"

	# FAT
	truncate -s 16M "$F/fat.img"
	run mkfs_fat mkfs.fat -F 16 -n TOOLSFAT -i 12345678 "$F/fat.img"
	run fsck_fat fsck.fat -n "$F/fat.img"
	run mtools_mmd mmd -i "$F/fat.img" ::/DIR
	run mtools_mcopy mcopy -i "$F/fat.img" "$F/tree/etc/motd" ::/DIR/MOTD.TXT
	found mtools_mdir 'MOTD' mdir -i "$F/fat.img" ::/DIR
	found mtools_mtype '^hello image$' mtype -i "$F/fat.img" ::/DIR/MOTD.TXT
	run fsck_fat_after fsck.fat -n "$F/fat.img"

	# ISO 9660
	for n in a b; do
		xorriso -as mkisofs -quiet -r -V TOOLSISO -o "$F/$n.iso" "$F/tree" >"$T/xorriso-$n.log" 2>&1
	done
	same xorriso_deterministic "$F/a.iso" "$F/b.iso"
	found xorriso_find '/etc/motd' xorriso -indev "$F/a.iso" -find / -type f
	run osirrox_extract xorriso -osirrox on -indev "$F/a.iso" -extract /etc/motd "$F/iso-motd"
	found osirrox_content '^hello image$' cat "$F/iso-motd"

	# qemu-img: formats, conversion, checks, backing files, snapshots
	run qcow2_convert qemu-img convert -f raw -O qcow2 "$F/ext4-a.img" "$F/ext4.qcow2"
	found qcow2_info '"format": "qcow2"' qemu-img info --output=json "$F/ext4.qcow2"
	found qcow2_check 'No errors were found' qemu-img check "$F/ext4.qcow2"
	run qcow2_back qemu-img convert -f qcow2 -O raw "$F/ext4.qcow2" "$F/ext4-back.img"
	same qcow2_roundtrip "$F/ext4-a.img" "$F/ext4-back.img"
	run vmdk_convert qemu-img convert -f raw -O vmdk "$F/ext4-a.img" "$F/ext4.vmdk"
	run vmdk_back qemu-img convert -f vmdk -O raw "$F/ext4.vmdk" "$F/ext4-vmdk.img"
	same vmdk_roundtrip "$F/ext4-a.img" "$F/ext4-vmdk.img"
	run vpc_convert qemu-img convert -f raw -O vpc "$F/ext4-a.img" "$F/ext4.vhd"
	found qemu_compare 'Images are identical' qemu-img compare -f raw -F vpc "$F/ext4-a.img" "$F/ext4.vhd"
	run qcow2_overlay qemu-img create -q -f qcow2 -b "$F/ext4-a.img" -F raw "$F/overlay.qcow2"
	found qcow2_backing 'backing file: ' qemu-img info "$F/overlay.qcow2"
	run qcow2_snapshot qemu-img snapshot -c snap1 "$F/ext4.qcow2"
	found qcow2_snapshot_list 'snap1' qemu-img snapshot -l "$F/ext4.qcow2"
	found qemu_io 'wrote 4096' qemu-io -c 'write 0 4k' "$F/ext4.qcow2"
	found qemu_nbd_help 'Usage' qemu-nbd --help
	found qemu_storage_daemon 'Usage' qemu-storage-daemon --help

	# partition tables
	truncate -s 64M "$F/gpt.img"
	run sfdisk_gpt sh -c "printf 'label: gpt\nlabel-id: $U\nstart=2048, size=16384, type=L\nsize=+, type=U\n' | sfdisk -q $F/gpt.img"
	found sfdisk_dump 'type=0FC63DAF-8483-4772-8E79-3D69D8477DE4' sfdisk -d "$F/gpt.img"
	found sfdisk_verify 'No errors' sfdisk --verify "$F/gpt.img"
	found partx_show_image '^ *1 +2048 +18431' partx --show "$F/gpt.img"
	found gdisk_list 'Found valid GPT' gdisk -l "$F/gpt.img"
	found gdisk_partitions '^ +2 +[0-9]+ +[0-9]+ +.*EF00' gdisk -l "$F/gpt.img"
	truncate -s 64M "$F/gdisk.img"
	run gdisk_create sh -c "printf 'o\ny\nn\n1\n\n+8M\n8300\nn\n2\n\n\n8200\nw\ny\n' | gdisk $F/gdisk.img"
	found gdisk_created 'Linux swap' gdisk -l "$F/gdisk.img"
	truncate -s 64M "$F/dos.img"
	run sfdisk_dos sh -c "printf 'label: dos\nstart=2048, size=8192, type=83\ntype=82\n' | sfdisk -q $F/dos.img"
	found fdisk_list 'Disklabel type: dos' fdisk -l "$F/dos.img"
	found blkid_gpt 'PTTYPE="gpt"' blkid -p "$F/gpt.img"
	found blkid_dos 'PTTYPE="dos"' blkid -p "$F/dos.img"

	# signatures
	found blkid_ext4 'TYPE="ext4"' blkid -p "$F/ext4-a.img"
	found blkid_label 'LABEL="toolsfs"' blkid -p "$F/ext4-a.img"
	found blkid_vfat 'TYPE="vfat"' blkid -p "$F/fat.img"
	found blkid_squashfs 'TYPE="squashfs"' blkid -p "$F/a.sqsh"
	found blkid_erofs 'TYPE="erofs"' blkid -p "$F/a.erofs"
	found wipefs_list 'ext4' wipefs "$F/ext4-a.img"
	cp "$F/ext4-a.img" "$F/ext4-wipe.img"
	run wipefs_erase wipefs -a "$F/ext4-wipe.img"
	blkid -p "$F/ext4-wipe.img" >/dev/null 2>&1
	kv blkid_after_wipe "rc$?"

	# xfs and btrfs images (no kernel support needed to build and check them)
	truncate -s 320M "$F/xfs.img"
	run mkfs_xfs mkfs.xfs -q -L toolsxfs -m uuid=$U "$F/xfs.img"
	found blkid_xfs 'TYPE="xfs"' blkid -p "$F/xfs.img"
	run xfs_repair_check xfs_repair -n "$F/xfs.img"
	truncate -s 128M "$F/btrfs.img"
	run mkfs_btrfs mkfs.btrfs -q -f -L toolsbtrfs -U $U --rootdir "$F/tree" "$F/btrfs.img"
	found btrfs_super 'label[[:space:]]+toolsbtrfs' btrfs inspect-internal dump-super "$F/btrfs.img"
	run btrfs_check btrfs check --readonly "$F/btrfs.img"
	found fsck_btrfs_wrapper 'BTRFS|btrfs' fsck.btrfs "$F/btrfs.img"

	# the setuid mount tools and the namespace they run in
	found findmnt_proc '"target": *"/proc"' findmnt -J /proc
	mkdir -p "$T/mnt"
	run mount_tmpfs_userns sh -c "mount -t tmpfs -o size=1m tmpfs $T/mnt && findmnt -n $T/mnt && umount $T/mnt"
	found losetup_version 'losetup from util-linux' losetup --version
	found lsblk_version 'lsblk from util-linux' lsblk --version
	found mountpoint_run 'is a mountpoint' mountpoint /proc
	run fallocate_run sh -c "fallocate -l 1M $T/falloc.img && [ \$(stat -c %s $T/falloc.img) = 1048576 ]"
fi

# ------------------------------------------------------------------------------------------------ QEMU (emulation only)
if section qemu; then
	V="$T/vm"
	mkdir -p "$V"
	run boot_assemble as --32 -o "$V/boot.o" vm/boot.S
	run boot_link ld -m elf_i386 -Ttext 0x7c00 -o "$V/boot.elf" "$V/boot.o"
	run boot_objcopy objcopy -O binary -j .text "$V/boot.elf" "$V/boot.bin"
	cp "$V/boot.bin" "$V/boot.img"
	truncate -s 1M "$V/boot.img"
	kv boot_sector_size "$(wc -c <"$V/boot.bin")"
	timeout 120 qemu-system-x86_64 -accel tcg -M q35 -m 64 -display none -serial "file:$V/ser.txt" -monitor none -no-reboot \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04 -drive format=raw,file="$V/boot.img",if=ide >"$V/qemu.log" 2>&1
	kv qemu_tcg_boot_rc "$?"
	found qemu_tcg_serial '^BOOT-OK$' cat "$V/ser.txt"
	found qemu_machines 'q35' qemu-system-x86_64 -M help
	found qemu_accels 'tcg' qemu-system-x86_64 -accel help
	ls /usr/share/qemu /usr/share/seabios >/dev/null 2>&1
	kv seabios_file "$(ls /usr/share/seabios/bios-256k.bin 2>/dev/null || echo missing)"
	kv ovmf_files "$(ls /usr/share/OVMF/OVMF_CODE.fd /usr/share/OVMF/OVMF_VARS.fd 2>/dev/null | tr '\n' ' ')"
	# UEFI: OVMF under TCG, console on the serial port; with no bootable disk its boot manager says so.
	cp /usr/share/OVMF/OVMF_VARS.fd "$V/vars.fd"
	timeout 240 qemu-system-x86_64 -accel tcg -M q35 -m 256 -display none -serial "file:$V/ovmf-ser.txt" -monitor none -no-reboot \
		-drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE.fd -drive if=pflash,format=raw,file="$V/vars.fd" \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04 >"$V/ovmf-qemu.log" 2>&1 &
	q=$!
	i=0
	while [ $i -lt 200 ] && ! grep -aq 'No bootable option or device was found' "$V/ovmf-ser.txt" 2>/dev/null; do sleep 0.5; i=$((i + 1)); done
	kill $q 2>/dev/null
	wait $q 2>/dev/null
	found ovmf_serial 'BdsDxe: No bootable option or device was found' sh -c "grep -a -o 'BdsDxe: No bootable option or device was found' $V/ovmf-ser.txt"
fi

# ------------------------------------------------------------------------------------------------ what the unprivileged session lacks
if section limits; then
	kv dev_kvm "$([ -e /dev/kvm ] && echo present || echo absent)"
	kv dev_fuse "$([ -e /dev/fuse ] && echo present || echo absent)"
	kv ulimit_nofile "$(ulimit -n)/$(ulimit -Hn)"
	kv perf_event_paranoid "$(cat /proc/sys/kernel/perf_event_paranoid)"
fi
