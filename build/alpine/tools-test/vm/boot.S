/* 512-byte boot sector: writes BOOT-OK to the first serial port, then leaves through the isa-debug-exit device
 * (QEMU exit status (0x21 << 1) | 1 = 67), or halts where there is no such device.
 *   as --32 -o boot.o boot.S && ld -m elf_i386 -Ttext 0x7c00 -o boot.elf boot.o && objcopy -O binary -j .text boot.elf boot.bin */
	.code16
	.globl _start
_start:
	cli
	cld
	xorw %ax, %ax
	movw %ax, %ds
	movw $msg, %si
1:	lodsb
	testb %al, %al
	jz 2f
	movw $0x3f8, %dx
	outb %al, %dx
	jmp 1b
2:	movw $0xf4, %dx
	movb $0x21, %al
	outb %al, %dx
3:	hlt
	jmp 3b
msg:	.asciz "BOOT-OK\n"
	.org 510
	.word 0xaa55
